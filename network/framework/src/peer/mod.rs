// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! [`Peer`] manages a single connection to a remote peer after the initial connection
//! establishment and handshake.
//!
//! Its responsibilities include sending and receiving [`NetworkMessage`]s
//! over-the-wire, maintaining a completion queue of pending RPC requests (through
//! the [`InboundRpcs`] and [`OutboundRpcs`] completion queues), and eventually
//! shutting down when the [`PeerManager`] requests it or the connection is lost.
//!
//! [`Peer`] owns the actual underlying connection socket and is reponsible for
//! the socket's shutdown, graceful or otherwise.
//!
//! [`PeerManager`]: crate::peer_manager::PeerManager

use crate::{
    counters::{
        self, network_application_inbound_traffic, network_application_outbound_traffic,
        FAILED_LABEL, RECEIVED_LABEL, SENT_LABEL,
    },
    logging::NetworkSchema,
    peer_manager::{PeerManagerError, TransportNotification},
    protocols::{
        direct_send::Message,
        rpc::{InboundRpcRequest, InboundRpcs, OutboundRpcRequest, OutboundRpcs},
        stream::{InboundStreamBuffer, OutboundStream, StreamMessage},
        wire::messaging::v1::{
            DirectSendMsg, ErrorCode, MultiplexMessage, MultiplexMessageSink,
            MultiplexMessageStream, NetworkMessage, Priority, ReadError, WriteError,
        },
    },
    transport::{self, Connection, ConnectionMetadata},
    ProtocolId,
};
use aptos_channels::aptos_channel;
use aptos_config::network_id::NetworkContext;
use aptos_logger::prelude::*;
use aptos_short_hex_str::AsShortHexStr;
use aptos_time_service::{TimeService, TimeServiceTrait};
use aptos_types::PeerId;
use bytes::Bytes;
use futures::{
    self,
    channel::oneshot,
    io::{AsyncRead, AsyncWrite},
    stream::StreamExt,
    SinkExt,
};
use futures_util::stream::{select, select_all};
use serde::Serialize;
use std::{fmt, panic, time::Duration};
use tokio::runtime::Handle;
use tokio_util::compat::{
    FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt,
};

#[cfg(test)]
mod test;

#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

/// Requests [`Peer`] receives from the [`PeerManager`](crate::peer_manager::PeerManager).
#[derive(Debug)]
pub enum PeerRequest {
    /// Send an RPC request to peer.
    SendRpc(OutboundRpcRequest),
    /// Fire-and-forget style message send to peer.
    SendDirectSend(Message),
}

/// Notifications that [`Peer`] sends to the [`PeerManager`](crate::peer_manager::PeerManager).
#[derive(Debug, PartialEq)]
pub enum PeerNotification {
    /// A new RPC request has been received from peer.
    RecvRpc(InboundRpcRequest),
    /// A new message has been received from peer.
    RecvMessage(Message),
}

/// The reason for closing a connection.
///
/// For example, if the remote peer closed the connection or the connection was
/// lost, the disconnect reason will be `ConnectionLost`. In contrast, if the
/// [`PeerManager`](crate::peer_manager::PeerManager) requested us to close this
/// connection, then the disconnect reason will be `Requested`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum DisconnectReason {
    Requested,
    ConnectionLost,
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DisconnectReason::Requested => "Requested",
            DisconnectReason::ConnectionLost => "ConnectionLost",
        };
        write!(f, "{}", s)
    }
}

enum State {
    Connected,
    ShuttingDown(DisconnectReason),
}

/// The `Peer` actor manages a single connection to another remote peer after
/// the initial connection establishment and handshake.
pub struct Peer<TSocket> {
    /// The network instance this Peer actor is running under.
    network_context: NetworkContext,
    /// A handle to a tokio executor.
    executor: Handle,
    /// A handle to a time service for easily mocking time-related operations.
    time_service: TimeService,
    /// Connection specific information.
    connection_metadata: ConnectionMetadata,
    /// Underlying connection (with multiple sockets) to the remote peer
    connection: Option<Vec<TSocket>>,
    /// Channel to notify PeerManager that we've disconnected.
    connection_notifs_tx: aptos_channels::Sender<TransportNotification<TSocket>>,
    /// Channel to receive requests from PeerManager to send messages and rpcs.
    peer_reqs_rx: aptos_channel::Receiver<ProtocolId, PeerRequest>,
    /// Channel to notifty PeerManager of new inbound messages and rpcs.
    peer_notifs_tx: aptos_channel::Sender<ProtocolId, PeerNotification>,
    /// Inbound rpc request queue for handling requests from remote peer.
    inbound_rpcs: InboundRpcs,
    /// Outbound rpc request queue for sending requests to remote peer and handling responses.
    outbound_rpcs: OutboundRpcs,
    /// Flag to indicate if the actor is being shut down.
    state: State,
    /// The maximum size of an inbound or outbound request frame
    max_frame_size: usize,
    /// The maximum size of an inbound or outbound request message
    max_message_size: usize,
    /// Inbound stream buffer
    inbound_stream: InboundStreamBuffer,
    /// Counter for the next write stream index
    write_stream_index: usize,
}

impl<TSocket> Peer<TSocket>
where
    TSocket: AsyncRead + AsyncWrite + Send + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        network_context: NetworkContext,
        executor: Handle,
        time_service: TimeService,
        connection: Connection<TSocket>,
        connection_notifs_tx: aptos_channels::Sender<TransportNotification<TSocket>>,
        peer_reqs_rx: aptos_channel::Receiver<ProtocolId, PeerRequest>,
        peer_notifs_tx: aptos_channel::Sender<ProtocolId, PeerNotification>,
        inbound_rpc_timeout: Duration,
        max_concurrent_inbound_rpcs: u32,
        max_concurrent_outbound_rpcs: u32,
        max_frame_size: usize,
        max_message_size: usize,
    ) -> Self {
        let Connection {
            metadata: connection_metadata,
            sockets,
        } = connection;
        let remote_peer_id = connection_metadata.remote_peer_id;
        let max_fragments = max_message_size / max_frame_size;
        Self {
            network_context,
            executor,
            time_service: time_service.clone(),
            connection_metadata,
            connection: Some(sockets),
            connection_notifs_tx,
            peer_reqs_rx,
            peer_notifs_tx,
            inbound_rpcs: InboundRpcs::new(
                network_context,
                time_service.clone(),
                remote_peer_id,
                inbound_rpc_timeout,
                max_concurrent_inbound_rpcs,
            ),
            outbound_rpcs: OutboundRpcs::new(
                network_context,
                time_service,
                remote_peer_id,
                max_concurrent_outbound_rpcs,
            ),
            state: State::Connected,
            max_frame_size,
            max_message_size,
            inbound_stream: InboundStreamBuffer::new(max_fragments),
            write_stream_index: 0,
        }
    }

    fn remote_peer_id(&self) -> PeerId {
        self.connection_metadata.remote_peer_id
    }

    pub async fn start(mut self) {
        let remote_peer_id = self.remote_peer_id();
        trace!(
            NetworkSchema::new(&self.network_context)
                .connection_metadata(&self.connection_metadata),
            "{} Starting Peer actor for peer: {}",
            self.network_context,
            remote_peer_id.short_str()
        );

        // Get all the sockets and split them into read and write halves
        let mut connections = self.connection.take().unwrap();
        if connections.is_empty() {
            panic!("Peer must have at least one socket");
        }
        let mut read_sockets = Vec::new();
        let mut write_sockets = Vec::new();
        for socket in connections.drain(..) {
            let (read_socket, write_socket) = tokio::io::split(socket.compat());
            read_sockets.push(read_socket);
            write_sockets.push(write_socket);
        }

        // Create read streams for all sockets
        let mut read_streams = Vec::new();
        for read_socket in read_sockets.drain(..) {
            read_streams.push(
                MultiplexMessageStream::new(read_socket.compat(), self.max_frame_size).fuse(),
            );
        }
        let mut readers = select_all(read_streams).fuse();

        // Create write sinks for all sockets
        let mut write_sinks = Vec::new();
        for write_socket in write_sockets.drain(..) {
            write_sinks.push(MultiplexMessageSink::new(
                write_socket.compat_write(),
                self.max_frame_size,
            ));
        }

        // Start writer "processes" as separate tasks. We receive two handles to
        // communicate with the task:
        //   1. `write_reqs_tx`: Queue of pending NetworkMessages to write.
        //   2. `close_tx`: Handle to close the task and underlying connection.
        let mut write_requests_senders = Vec::new();
        let mut write_close_senders = Vec::new();
        for write_sink in write_sinks.drain(..) {
            let (write_reqs_tx, write_close_tx) = Self::start_writer_task(
                &self.executor,
                self.time_service.clone(),
                self.connection_metadata.clone(),
                self.network_context,
                write_sink,
                self.max_frame_size,
                self.max_message_size,
            );
            write_requests_senders.push(write_reqs_tx);
            write_close_senders.push(write_close_tx);
        }

        // Start main Peer event loop.
        let reason = loop {
            if let State::ShuttingDown(reason) = self.state {
                break reason;
            }

            futures::select! {
                // Handle a new outbound request from the PeerManager.
                maybe_request = self.peer_reqs_rx.next() => {
                    match maybe_request {
                        Some(request) => self.handle_outbound_request(request, &mut write_requests_senders).await,
                        // The PeerManager is requesting this connection to close
                        // by dropping the corresponding peer_reqs_tx handle.
                        None => self.shutdown(DisconnectReason::Requested),
                    }
                },
                // Handle a new inbound MultiplexMessage that we've just read off
                // the wire from the remote peer.
                maybe_message = readers.next() => {
                    match maybe_message {
                        Some(message) =>  {
                            if let Err(err) = self.handle_inbound_message(message, &mut write_requests_senders).await {
                                warn!(
                                    NetworkSchema::new(&self.network_context)
                                        .connection_metadata(&self.connection_metadata),
                                    error = %err,
                                    "{} Error in handling inbound message from peer: {}, error: {}",
                                    self.network_context,
                                    remote_peer_id.short_str(),
                                    err
                                );
                            }
                        },
                        // The socket was gracefully closed by the remote peer.
                        None => self.shutdown(DisconnectReason::ConnectionLost),
                    }
                },
                // Drive the queue of pending inbound rpcs. When one is fulfilled
                // by an upstream protocol, send the response to the remote peer.
                maybe_response = self.inbound_rpcs.next_completed_response() => {
                    if let Err(err) = self.inbound_rpcs.send_outbound_response(&mut write_requests_senders, maybe_response).await {
                        warn!(
                            NetworkSchema::new(&self.network_context).connection_metadata(&self.connection_metadata),
                            error = %err,
                            "{} Error in handling inbound rpc request, error: {}", self.network_context, err,
                        );
                    }
                },
                // Poll the queue of pending outbound rpc tasks for the next
                // successfully or unsuccessfully completed request.
                (request_id, maybe_completed_request) = self.outbound_rpcs.next_completed_request() => {
                    self.outbound_rpcs.handle_completed_request(request_id, maybe_completed_request);
                }
            }
        };

        // Finish shutting down the connection. Close the writer task and notify
        // PeerManager that this connection has shutdown.
        self.do_shutdown(write_close_senders, reason).await;
    }

    // Start a new task on the given executor which is responsible for writing outbound messages on
    // the wire. The function returns two channels which can be used to send instructions to the
    // task:
    // 1. The first channel is used to send outbound NetworkMessages to the task
    // 2. The second channel is used to instruct the task to close the connection and terminate.
    // If outbound messages are queued when the task receives a close instruction, it discards
    // them and immediately closes the connection.
    fn start_writer_task(
        executor: &Handle,
        time_service: TimeService,
        connection_metadata: ConnectionMetadata,
        network_context: NetworkContext,
        mut writer: MultiplexMessageSink<impl AsyncWrite + Unpin + Send + 'static>,
        max_frame_size: usize,
        max_message_size: usize,
    ) -> (aptos_channels::Sender<NetworkMessage>, oneshot::Sender<()>) {
        let remote_peer_id = connection_metadata.remote_peer_id;
        let (write_reqs_tx, mut write_reqs_rx): (aptos_channels::Sender<NetworkMessage>, _) =
            aptos_channels::new(1024, &counters::PENDING_WIRE_MESSAGES);
        let (close_tx, mut close_rx) = oneshot::channel();

        let (mut msg_tx, msg_rx) = aptos_channels::new(1024, &counters::PENDING_MULTIPLEX_MESSAGE);
        let (stream_msg_tx, stream_msg_rx) =
            aptos_channels::new(1024, &counters::PENDING_MULTIPLEX_STREAM);

        // this task ends when the multiplex task ends (by dropping the senders)
        let writer_task = async move {
            let mut stream = select(msg_rx, stream_msg_rx);
            let log_context =
                NetworkSchema::new(&network_context).connection_metadata(&connection_metadata);
            while let Some(message) = stream.next().await {
                if let Err(err) = writer.send(&message).await {
                    warn!(
                        log_context,
                        error = %err,
                        "{} Error in sending message to peer: {}",
                        network_context,
                        remote_peer_id.short_str(),
                    );
                }
            }
            info!(
                log_context,
                "{} Closing connection to peer: {}",
                network_context,
                remote_peer_id.short_str()
            );
            let flush_and_close = async {
                writer.flush().await?;
                writer.close().await?;
                Ok(()) as Result<(), WriteError>
            };
            match time_service
                .timeout(transport::TRANSPORT_TIMEOUT, flush_and_close)
                .await
            {
                Err(_) => {
                    info!(
                        log_context,
                        "{} Timeout in flush/close of connection to peer: {}",
                        network_context,
                        remote_peer_id.short_str()
                    );
                },
                Ok(Err(err)) => {
                    info!(
                        log_context,
                        error = %err,
                        "{} Failure in flush/close of connection to peer: {}, error: {}",
                        network_context,
                        remote_peer_id.short_str(),
                        err
                    );
                },
                Ok(Ok(())) => {
                    info!(
                        log_context,
                        "{} Closed connection to peer: {}",
                        network_context,
                        remote_peer_id.short_str()
                    );
                },
            }
        };
        let multiplex_task = async move {
            let mut outbound_stream =
                OutboundStream::new(max_frame_size, max_message_size, stream_msg_tx);
            loop {
                futures::select! {
                    message = write_reqs_rx.select_next_some() => {
                        // either channel full would block the other one
                        let result = if outbound_stream.should_stream(&message) {
                            outbound_stream.stream_message(message).await
                        } else {
                            msg_tx.send(MultiplexMessage::Message(message)).await.map_err(|_| anyhow::anyhow!("Writer task ended"))
                        };
                        if let Err(err) = result {
                            warn!(
                                error = %err,
                                "{} Error in sending message to peer: {}",
                                network_context,
                                remote_peer_id.short_str(),
                            );
                        }
                    },
                    _ = close_rx => {
                        break;
                    }
                }
            }
        };
        executor.spawn(writer_task);
        executor.spawn(multiplex_task);
        (write_reqs_tx, close_tx)
    }

    async fn handle_inbound_network_message(
        &mut self,
        message: NetworkMessage,
    ) -> Result<(), PeerManagerError> {
        match message {
            NetworkMessage::DirectSendMsg(message) => self.handle_inbound_direct_send(message),
            NetworkMessage::Error(error_msg) => {
                warn!(
                    NetworkSchema::new(&self.network_context)
                        .connection_metadata(&self.connection_metadata),
                    error_msg = ?error_msg,
                    "{} Peer {} sent an error message: {:?}",
                    self.network_context,
                    self.remote_peer_id().short_str(),
                    error_msg,
                );
            },
            NetworkMessage::RpcRequest(request) => {
                if let Err(err) = self
                    .inbound_rpcs
                    .handle_inbound_request(&mut self.peer_notifs_tx, request)
                {
                    warn!(
                        NetworkSchema::new(&self.network_context)
                            .connection_metadata(&self.connection_metadata),
                        error = %err,
                        "{} Error handling inbound rpc request: {}",
                        self.network_context,
                        err
                    );
                }
            },
            NetworkMessage::RpcResponse(response) => {
                self.outbound_rpcs.handle_inbound_response(response)
            },
        };
        Ok(())
    }

    async fn handle_inbound_stream_message(
        &mut self,
        message: StreamMessage,
    ) -> Result<(), PeerManagerError> {
        match message {
            StreamMessage::Header(header) => {
                self.inbound_stream.new_stream(header)?;
            },
            StreamMessage::Fragment(fragment) => {
                if let Some(message) = self.inbound_stream.append_fragment(fragment)? {
                    self.handle_inbound_network_message(message).await?;
                }
            },
        }
        Ok(())
    }

    /// Increment the write stream index and wrap around if necessary
    fn increment_write_stream_index(&mut self, num_streams: usize) {
        self.write_stream_index = (self.write_stream_index + 1) % num_streams;
    }

    async fn handle_inbound_message(
        &mut self,
        message: Result<MultiplexMessage, ReadError>,
        write_requests_senders: &mut Vec<aptos_channels::Sender<NetworkMessage>>,
    ) -> Result<(), PeerManagerError> {
        trace!(
            NetworkSchema::new(&self.network_context)
                .connection_metadata(&self.connection_metadata),
            "{} Received message from peer {}",
            self.network_context,
            self.remote_peer_id().short_str()
        );

        let message = match message {
            Ok(message) => message,
            Err(err) => match err {
                ReadError::DeserializeError(_, _, ref frame_prefix) => {
                    // DeserializeError's are recoverable so we'll let the other
                    // peer know about the error and log the issue, but we won't
                    // close the connection.
                    let message_type = frame_prefix.as_ref().first().unwrap_or(&0);
                    let protocol_id = frame_prefix.as_ref().get(1).unwrap_or(&0);
                    let error_code = ErrorCode::parsing_error(*message_type, *protocol_id);
                    let message = NetworkMessage::Error(error_code);

                    // Choose a channel to send the error message along
                    let num_streams = write_requests_senders.len();
                    let write_reqs_tx = write_requests_senders
                        .get_mut(self.write_stream_index)
                        .unwrap();
                    self.increment_write_stream_index(num_streams);

                    write_reqs_tx.send(message).await?;
                    return Err(err.into());
                },
                ReadError::IoError(_) => {
                    // IoErrors are mostly unrecoverable so just close the connection.
                    self.shutdown(DisconnectReason::ConnectionLost);
                    return Err(err.into());
                },
            },
        };

        match message {
            MultiplexMessage::Message(message) => {
                self.handle_inbound_network_message(message).await
            },
            MultiplexMessage::Stream(message) => self.handle_inbound_stream_message(message).await,
        }
    }

    /// Handle an inbound DirectSendMsg from the remote peer. There's not much to
    /// do here other than bump some counters and forward the message up to the
    /// PeerManager.
    fn handle_inbound_direct_send(&mut self, message: DirectSendMsg) {
        let peer_id = self.remote_peer_id();
        let protocol_id = message.protocol_id;
        let data = message.raw_msg;

        trace!(
            NetworkSchema::new(&self.network_context).remote_peer(&peer_id),
            protocol_id = protocol_id,
            "{} DirectSend: Received inbound message from peer {} for protocol {:?}",
            self.network_context,
            peer_id.short_str(),
            protocol_id
        );
        self.update_inbound_direct_send_metrics(message.protocol_id, data.len() as u64);

        let notif = PeerNotification::RecvMessage(Message {
            protocol_id,
            mdata: Bytes::from(data),
        });

        if let Err(err) = self.peer_notifs_tx.push(protocol_id, notif) {
            warn!(
                NetworkSchema::new(&self.network_context),
                error = ?err,
                "{} Failed to notify PeerManager about inbound DirectSend message. Error: {:?}",
                self.network_context,
                err
            );
        }
    }

    /// Updates the inbound direct send metrics (e.g., messages and bytes received)
    fn update_inbound_direct_send_metrics(&self, protocol_id: ProtocolId, data_len: u64) {
        // Update the metrics for the received direct send message
        counters::direct_send_messages(&self.network_context, RECEIVED_LABEL).inc();
        counters::direct_send_bytes(&self.network_context, RECEIVED_LABEL).inc_by(data_len);

        // Update the general network traffic metrics
        network_application_inbound_traffic(self.network_context, protocol_id, data_len);
    }

    async fn handle_outbound_request(
        &mut self,
        request: PeerRequest,
        write_requests_senders: &mut Vec<aptos_channels::Sender<NetworkMessage>>,
    ) {
        trace!(
            "Peer {} PeerRequest::{:?}",
            self.remote_peer_id().short_str(),
            request
        );

        // Choose a channel to send the request along
        let num_streams = write_requests_senders.len();
        let write_reqs_tx = write_requests_senders
            .get_mut(self.write_stream_index)
            .unwrap();
        self.increment_write_stream_index(num_streams);

        match request {
            // To send an outbound DirectSendMsg, we just bump some counters and
            // push it onto our outbound writer queue.
            PeerRequest::SendDirectSend(message) => {
                // Create the direct send message
                let message_len = message.mdata.len();
                let protocol_id = message.protocol_id;
                let message = NetworkMessage::DirectSendMsg(DirectSendMsg {
                    protocol_id,
                    priority: Priority::default(),
                    raw_msg: Vec::from(message.mdata.as_ref()),
                });

                match write_reqs_tx.send(message).await {
                    Ok(_) => {
                        self.update_outbound_direct_send_metrics(protocol_id, message_len as u64);
                    },
                    Err(e) => {
                        counters::direct_send_messages(&self.network_context, FAILED_LABEL).inc();
                        warn!(
                            NetworkSchema::new(&self.network_context)
                                .connection_metadata(&self.connection_metadata),
                            error = ?e,
                            "Failed to send direct send message for protocol {} to peer: {}. Error: {:?}",
                            protocol_id,
                            self.remote_peer_id().short_str(),
                            e,
                        );
                    },
                }
            },
            PeerRequest::SendRpc(request) => {
                let protocol_id = request.protocol_id;
                if let Err(e) = self
                    .outbound_rpcs
                    .handle_outbound_request(request, write_reqs_tx)
                    .await
                {
                    warn!(
                        NetworkSchema::new(&self.network_context)
                            .connection_metadata(&self.connection_metadata),
                        error = %e,
                        "Failed to send outbound rpc request for protocol {} to peer: {}. Error: {}",
                        protocol_id,
                        self.remote_peer_id().short_str(),
                        e,
                    );
                }
            },
        }
    }

    /// Updates the outbound direct send metrics (e.g., messages and bytes sent)
    fn update_outbound_direct_send_metrics(&mut self, protocol_id: ProtocolId, data_len: u64) {
        // Update the metrics for the sent direct send message
        counters::direct_send_messages(&self.network_context, SENT_LABEL).inc();
        counters::direct_send_bytes(&self.network_context, SENT_LABEL).inc_by(data_len);

        // Update the general network traffic metrics
        network_application_outbound_traffic(self.network_context, protocol_id, data_len);
    }

    fn shutdown(&mut self, reason: DisconnectReason) {
        // Set the state of the actor to `State::ShuttingDown` to true ensures that the peer actor
        // will terminate and close the connection.
        self.state = State::ShuttingDown(reason);
    }

    async fn do_shutdown(
        mut self,
        writer_close_tx: Vec<oneshot::Sender<()>>,
        reason: DisconnectReason,
    ) {
        let remote_peer_id = self.remote_peer_id();

        // Send a PeerDisconnected event to PeerManager.
        if let Err(e) = self
            .connection_notifs_tx
            .send(TransportNotification::Disconnected(
                self.connection_metadata.clone(),
                reason,
            ))
            .await
        {
            warn!(
                NetworkSchema::new(&self.network_context)
                    .connection_metadata(&self.connection_metadata),
                error = ?e,
                "{} Failed to notify upstream about disconnection of peer: {}; error: {:?}",
                self.network_context,
                remote_peer_id.short_str(),
                e
            );
        }

        // Send a close instruction to the writer tasks. On receipt of this
        // instruction, the writer task drops all pending outbound messages and
        // closes the connection.
        for writer_close_tx in writer_close_tx {
            if let Err(e) = writer_close_tx.send(()) {
                info!(
                    NetworkSchema::new(&self.network_context)
                        .connection_metadata(&self.connection_metadata),
                    error = ?e,
                    "{} Failed to send close instruction to writer task. It must already be terminating/terminated. Error: {:?}",
                    self.network_context,
                    e
                );
            }
        }

        trace!(
            NetworkSchema::new(&self.network_context)
                .connection_metadata(&self.connection_metadata),
            "{} Peer actor for '{}' terminated",
            self.network_context,
            remote_peer_id.short_str()
        );
    }
}
