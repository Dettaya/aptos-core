// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! QUIC Transport
use crate::transport::Transport;
use aptos_proxy::Proxy;
use aptos_types::{
    network_address::{parse_dns_udp, parse_ip_udp, IpFilter, NetworkAddress},
    PeerId,
};
use futures::{
    future::{self, Either, Future},
    io::{AsyncRead, AsyncWrite},
    ready,
    stream::FuturesUnordered,
    Stream,
};
use quinn::{ClientConfig, Connecting, IdleTimeout, ServerConfig, TransportConfig, VarInt};
use std::{
    fmt::Debug,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::net::lookup_host;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use url::Url;

// Useful constants
const SERVER_STRING: &str = "aptos-node";

/// Transport to build QUIC connections
#[derive(Debug, Clone, Default)]
pub struct QuicTransport {
    server_endpoint: Option<quinn::Endpoint>,
}

impl QuicTransport {
    pub fn new() -> Self {
        Self {
            server_endpoint: None,
        }
    }
}

impl Transport for QuicTransport {
    type Error = ::std::io::Error;
    type Inbound = future::Ready<io::Result<Self::Output>>;
    type Listener = QuicConnectionStream;
    type Outbound = QuicOutboundConnection;
    type Output = QuicConnection;

    fn listen_on(
        &mut self,
        addr: NetworkAddress,
    ) -> Result<(Self::Listener, NetworkAddress), Self::Error> {
        // Parse the IP address, port and suffix
        let ((ipaddr, port), addr_suffix) =
            parse_ip_udp(addr.as_slice()).ok_or_else(|| invalid_addr_error(&addr))?;
        if !addr_suffix.is_empty() {
            return Err(invalid_addr_error(&addr));
        }

        // Create the QUIC server endpoint. This will call bind on the socket addr.
        let (server_config, _server_certificate) = configure_server()?;
        let socket_addr = SocketAddr::new(ipaddr, port);
        let server_endpoint = quinn::Endpoint::server(server_config, socket_addr)?;

        // Get the listen address
        let listen_addr = NetworkAddress::from_udp(server_endpoint.local_addr()?);

        // Save the server endpoint
        self.server_endpoint = Some(server_endpoint.clone());

        // Create the QUIC connection stream
        let quic_connection_stream = QuicConnectionStream::new(server_endpoint);

        Ok((quic_connection_stream, listen_addr))
    }

    fn dial(&self, _peer_id: PeerId, addr: NetworkAddress) -> Result<Self::Outbound, Self::Error> {
        let protos = addr.as_slice();

        // ensure addr is well formed to save some work before potentially
        // spawning a dial task that will fail anyway.
        parse_ip_udp(protos)
            .map(|_| ())
            .or_else(|| parse_dns_udp(protos).map(|_| ()))
            .ok_or_else(|| invalid_addr_error(&addr))?;

        let proxy = Proxy::new();

        let proxy_addr = {
            use aptos_types::network_address::Protocol::*;

            let addr = match protos.first() {
                Some(Ip4(ip)) => proxy.https(&ip.to_string()),
                Some(Ip6(ip)) => proxy.https(&ip.to_string()),
                Some(Dns(name)) | Some(Dns4(name)) | Some(Dns6(name)) => proxy.https(name.as_ref()),
                _ => None,
            };

            addr.and_then(|https_proxy| Url::parse(https_proxy).ok())
                .and_then(|url| {
                    if url.has_host() && url.scheme() == "http" {
                        Some(format!(
                            "{}:{}",
                            url.host().unwrap(),
                            url.port_or_known_default().unwrap()
                        ))
                    } else {
                        None
                    }
                })
        };

        let f: Pin<Box<dyn Future<Output = io::Result<QuicConnection>> + Send + 'static>> =
            Box::pin(match proxy_addr {
                Some(proxy_addr) => Either::Left(connect_via_proxy(proxy_addr, addr)),
                None => Either::Right(resolve_and_connect(addr)),
            });

        Ok(QuicOutboundConnection { inner: f })
    }
}

/// Try to lookup the dns name, then filter addrs according to the `IpFilter`.
async fn resolve_with_filter(
    ip_filter: IpFilter,
    dns_name: &str,
    port: u16,
) -> io::Result<impl Iterator<Item = SocketAddr> + '_> {
    Ok(lookup_host((dns_name, port))
        .await?
        .filter(move |socketaddr| ip_filter.matches(socketaddr.ip())))
}

pub async fn connect_to_remote(
    port: u16,
    remote_ipaddr: std::net::IpAddr,
) -> io::Result<quinn::Connection> {
    // Create the QUIC client endpoint. This will call bind on 127.0.0.1.
    let mut client_endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("could not create client endpoint: {:?}", error),
            )
        })?;
    client_endpoint.set_default_client_config(configure_client());

    // Connect to the remote server
    let socket_addr = SocketAddr::new(remote_ipaddr, port);
    let connecting = client_endpoint
        .connect(socket_addr, SERVER_STRING)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("could not connect to remote server: {:?}", error),
            )
        })?;

    Ok(connecting.await?)
}

/// Note: we need to take ownership of this `NetworkAddress` (instead of just
/// borrowing the `&[Protocol]` slice) so this future can be `Send + 'static`.
pub async fn resolve_and_connect(addr: NetworkAddress) -> io::Result<QuicConnection> {
    // Open a connection to the remote server
    let connection = open_connection_to_remote(addr).await?;

    // Create the QUIC connection
    create_quic_connection(connection).await
}

/// Attempts to connect to the remote address
async fn open_connection_to_remote(addr: NetworkAddress) -> io::Result<quinn::Connection> {
    let protos = addr.as_slice();
    if let Some(((ipaddr, port), _addr_suffix)) = parse_ip_udp(protos) {
        // this is an /ip4 or /ip6 address, so we can just connect without any
        // extra resolving or filtering.
        connect_to_remote(port, ipaddr).await
    } else if let Some(((ip_filter, dns_name, port), _addr_suffix)) = parse_dns_udp(protos) {
        // resolve dns name and filter
        let socketaddr_iter = resolve_with_filter(ip_filter, dns_name.as_ref(), port).await?;
        let mut last_err = None;

        // try to connect until the first succeeds
        for socketaddr in socketaddr_iter {
            match connect_to_remote(socketaddr.port(), socketaddr.ip()).await {
                Ok(connection) => return Ok(connection),
                Err(err) => last_err = Some(err),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "could not resolve dns name to any address: name: {}, ip filter: {:?}",
                    dns_name.as_ref(),
                    ip_filter,
                ),
            )
        }))
    } else {
        Err(invalid_addr_error(&addr))
    }
}

async fn connect_via_proxy(
    _proxy_addr: String,
    _addr: NetworkAddress,
) -> io::Result<QuicConnection> {
    unimplemented!("CONNECT VIA PROXY!!")
}

fn invalid_addr_error(addr: &NetworkAddress) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Invalid NetworkAddress: '{}'", addr),
    )
}

#[must_use = "streams do nothing unless polled"]
#[allow(dead_code)]
pub struct QuicConnectionStream {
    server_endpoint: Pin<Box<quinn::Endpoint>>,
    pending_connections: Pin<Box<FuturesUnordered<Connecting>>>,
    pending_quic_connections: Pin<Box<FuturesUnordered<PendingQuicConnection>>>,
}

impl QuicConnectionStream {
    pub fn new(server_endpoint: quinn::Endpoint) -> Self {
        Self {
            server_endpoint: Box::pin(server_endpoint),
            pending_connections: Box::pin(FuturesUnordered::new()),
            pending_quic_connections: Box::pin(FuturesUnordered::new()),
        }
    }
}

impl Stream for QuicConnectionStream {
    type Item = io::Result<(future::Ready<io::Result<QuicConnection>>, NetworkAddress)>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Option<Self::Item>> {
        println!("(QuicConnectionStream) Polling Next PendingQuicConnection");

        // Check if there are any new pending connections to accept
        let server_endpoint = self.server_endpoint.clone();
        let mut server_accept = Box::pin(server_endpoint.accept());
        if let Poll::Ready(Some(pending_connection)) = server_accept.as_mut().poll(context) {
            println!("(QuicConnectionStream) Got a new pending connection!");

            // Add the new pending connection to the list of pending connections
            self.pending_connections.as_mut().push(pending_connection);
        }

        // Check if there are any pending connections that are now ready
        if let Poll::Ready(Some(Ok(connection))) =
            self.pending_connections.as_mut().poll_next(context)
        {
            println!("(QuicConnectionStream) Got a new pending QUIC connection!");

            // Create the pending QUIC connection
            let pending_quic_connection = PendingQuicConnection::new(connection);

            // Add the new pending QUIC connection to the list of pending QUIC connections
            self.pending_quic_connections.push(pending_quic_connection);
        }

        // Check if there are any pending QUIC connections that are now ready
        if let Poll::Ready(Some(Ok(quic_connection))) =
            self.pending_quic_connections.as_mut().poll_next(context)
        {
            println!("(QuicConnectionStream) Got a new and ready QUIC connection!");

            // Get the remote address
            let remote_address =
                NetworkAddress::from_udp(quic_connection.connection.remote_address());

            // Return the QUIC connection and remote address
            return Poll::Ready(Some(Ok((
                future::ready(Ok(quic_connection)),
                remote_address,
            ))));
        }

        Poll::Pending
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct QuicOutboundConnection {
    inner: Pin<Box<dyn Future<Output = io::Result<QuicConnection>> + Send + 'static>>,
}

impl Future for QuicOutboundConnection {
    type Output = io::Result<QuicConnection>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Self::Output> {
        let quic_connection = ready!(Pin::new(&mut self.inner).poll(context))?;
        Poll::Ready(Ok(quic_connection))
    }
}

/// A set of pending QUIC connections
#[derive(Debug)]
#[allow(dead_code)]
pub struct PendingQuicConnection {
    pending_quic_connections: Pin<
        Box<
            FuturesUnordered<
                Pin<Box<dyn Future<Output = io::Result<QuicConnection>> + Send + 'static>>,
            >,
        >,
    >,
}

impl PendingQuicConnection {
    pub fn new(connection: quinn::Connection) -> Self {
        let pending_quic_connections = Box::pin(FuturesUnordered::new());
        let future: Pin<Box<dyn Future<Output = io::Result<QuicConnection>> + Send + 'static>> =
            Box::pin(create_quic_connection(connection));
        pending_quic_connections.push(future);
        Self {
            pending_quic_connections,
        }
    }
}

impl Future for PendingQuicConnection {
    type Output = io::Result<QuicConnection>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Self::Output> {
        println!("(PendingQuicConnection) Polling PendingQuicConnection");

        // Poll the pending QUIC connections to see if any of them are ready
        match self.pending_quic_connections.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(quic_connection))) => {
                println!("(PendingQuicConnection) Got a new and ready QUIC connection!");
                Poll::Ready(Ok(quic_connection))
            },
            Poll::Ready(Some(Err(error))) => {
                println!("(PendingQuicConnection) Got an error: {:?}", error);

                // Something went wrong!
                let error = io::Error::new(
                    io::ErrorKind::Other,
                    format!("Could not accept connection: {:?}", error),
                );
                Poll::Ready(Err(error))
            },
            Poll::Ready(None) => {
                println!("(PendingQuicConnection) Got None!");

                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "No pending QUIC connections!",
                )))
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

async fn create_quic_connection(connection: quinn::Connection) -> io::Result<QuicConnection> {
    // Create the QUIC connection
    let remote_address = connection.remote_address();

    println!(
        "Creating QUIC connection from QUINN connection: {:?}",
        remote_address
    );

    let connection = QuicConnection::new(connection).await;

    println!("Got QUIC connection for: {:?}", remote_address);

    connection
}

/// A wrapper around a quinn Connection that implements the AsyncRead/AsyncWrite traits
#[derive(Debug)]
#[allow(dead_code)]
pub struct QuicConnection {
    connection: quinn::Connection,
    send_streams: Vec<Compat<quinn::SendStream>>,
    recv_streams: Vec<Compat<quinn::RecvStream>>,
}

impl QuicConnection {
    pub async fn new(connection: quinn::Connection) -> io::Result<Self> {
        // Open several connection streams
        let mut send_streams = vec![];
        let mut recv_streams = vec![];
        for _ in 0..1 {
            let (send_stream, recv_stream) = connection.open_bi().await?;
            send_streams.push(send_stream.compat_write());
            recv_streams.push(recv_stream.compat());
        }

        // Create the QUIC connection
        Ok(Self {
            connection,
            send_streams,
            recv_streams,
        })
    }
}

impl AsyncRead for QuicConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // TODO: read from multiple streams?
        let recv_stream = self.recv_streams.first_mut().unwrap();

        let result = Pin::new(recv_stream).poll_read(context, buf);
        println!(
            "(Remote: {:?}) Polling Read. Result: {:?}",
            self.connection.remote_address(),
            result
        );
        result
    }
}

impl AsyncWrite for QuicConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // TODO: write to multiple streams?
        let send_stream = self.send_streams.first_mut().unwrap();

        let result = Pin::new(send_stream).poll_write(context, buf);
        println!(
            "(Remote: {:?}) Polling Write. Result: {:?}",
            self.connection.remote_address(),
            result
        );
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<io::Result<()>> {
        // TODO: flush all streams?
        let send_stream = self.send_streams.first_mut().unwrap();

        let result = Pin::new(send_stream).poll_flush(context);
        println!(
            "(Remote: {:?}) Polling Flush. Result: {:?}",
            self.connection.remote_address(),
            result
        );
        result
    }

    fn poll_close(mut self: Pin<&mut Self>, _context: &mut Context) -> Poll<io::Result<()>> {
        // TODO: close all streams?
        let send_stream = self.send_streams.first_mut().unwrap();

        let result = Pin::new(send_stream).poll_close(_context);
        println!(
            "(Remote: {:?}) Polling Close. Result: {:?}",
            self.connection.remote_address(),
            result
        );
        result
    }
}

/// Dummy certificate verifier that treats any certificate as valid
struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

/// Returns the default client configured that ignores the server certificate
fn configure_client() -> ClientConfig {
    // Create the dummy crypto config
    let crypto_config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();

    // Create the client transport config
    let transport_config = create_transport_config();

    // Create the QUIC client configuration
    let mut client = ClientConfig::new(Arc::new(crypto_config));
    client.transport_config(transport_config);
    client
}

/// Returns a new transport config
fn create_transport_config() -> Arc<TransportConfig> {
    let mut transport_config = quinn::TransportConfig::default();

    transport_config.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(20_000)))); // 20 secs
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(20))); // 20 secs

    Arc::new(transport_config)
}

/// Returns the default server configuration along with its dummy certificate
fn configure_server() -> io::Result<(ServerConfig, Vec<u8>)> {
    // Create the dummy server certificate
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_STRING.into()]).unwrap();
    let cert_der = cert.serialize_der().unwrap();
    let priv_key = cert.serialize_private_key_der();
    let priv_key = rustls::PrivateKey(priv_key);
    let cert_chain = vec![rustls::Certificate(cert_der.clone())];

    // Create the server transport config
    let transport_config = create_transport_config();

    // Create the QUIC server configuration
    let mut server_config =
        ServerConfig::with_single_cert(cert_chain, priv_key).map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Invalid server certificate: {:?}", error),
            )
        })?;
    server_config.transport_config(transport_config);

    Ok((server_config, cert_der))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::transport::{ConnectionOrigin, Transport, TransportExt};
    use aptos_types::PeerId;
    use futures::{
        future::{join, FutureExt},
        io::{AsyncReadExt, AsyncWriteExt},
        StreamExt,
    };
    use tokio::runtime::Runtime;

    #[tokio::test]
    async fn simple_listen_and_dial() -> Result<(), ::std::io::Error> {
        let mut t = QuicTransport::new().and_then(|mut out, addr, origin| async move {
            println!(
                "(simple_listen_and_dial: {:?}) Got a new connection! Addr: {:?}",
                origin, addr
            );
            match origin {
                ConnectionOrigin::Inbound => {
                    println!(
                        "(simple_listen_and_dial: {:?}, {:?}) Writing data!",
                        origin, addr
                    );
                    out.write_all(b"Earth").await?;

                    println!(
                        "(simple_listen_and_dial: {:?}, {:?}) Reading data!",
                        origin, addr
                    );
                    let mut buf = [0; 3];
                    out.read_exact(&mut buf).await?;

                    println!(
                        "(simple_listen_and_dial: {:?}, {:?}) Verifying data: {:?}",
                        origin, addr, buf
                    );
                    assert_eq!(&buf, b"Air");
                },
                ConnectionOrigin::Outbound => {
                    println!(
                        "(simple_listen_and_dial: {:?}, {:?}) Reading data!",
                        origin, addr
                    );

                    let mut buf = [0; 5];
                    out.read_exact(&mut buf).await?;

                    println!(
                        "(simple_listen_and_dial: {:?}, {:?}) Verifying data: {:?}",
                        origin, addr, buf
                    );
                    assert_eq!(&buf, b"Earth");

                    println!(
                        "(simple_listen_and_dial: {:?}, {:?}) Writing data!",
                        origin, addr
                    );
                    out.write_all(b"Air").await?;
                },
            }
            Ok(())
        });

        let (listener, addr) = t.listen_on("/ip4/127.0.0.1/udp/0".parse().unwrap())?;
        let peer_id = PeerId::random();
        let dial = t.dial(peer_id, addr)?;
        let listener = listener.into_future().then(|(maybe_result, _stream)| {
            println!(
                "In listener future! Maybe result: {:?}",
                maybe_result.is_some()
            );
            let (incoming, _addr) = maybe_result.unwrap().unwrap();
            incoming.map(Result::unwrap)
        });

        let (outgoing, _incoming) = join(dial, listener).await;
        assert!(outgoing.is_ok());
        Ok(())
    }

    #[test]
    fn unsupported_multiaddrs() {
        let mut t = QuicTransport::default();

        let result = t.listen_on("/memory/0".parse().unwrap());
        assert!(result.is_err());

        let peer_id = PeerId::random();
        let result = t.dial(peer_id, "/memory/22".parse().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_with_filter() {
        let rt = Runtime::new().unwrap();

        // note: we only lookup "localhost", which is not really a DNS name, but
        // should always resolve to something and keep this test from being flaky.

        let f = async move {
            // this should always return something
            let addrs = resolve_with_filter(IpFilter::Any, "localhost", 1234)
                .await
                .unwrap()
                .collect::<Vec<_>>();
            assert!(!addrs.is_empty(), "addrs: {:?}", addrs);

            // we should only get Ip4 addrs
            let addrs = resolve_with_filter(IpFilter::OnlyIp4, "localhost", 1234)
                .await
                .unwrap()
                .collect::<Vec<_>>();
            assert!(addrs.iter().all(SocketAddr::is_ipv4), "addrs: {:?}", addrs);

            // we should only get Ip6 addrs
            let addrs = resolve_with_filter(IpFilter::OnlyIp6, "localhost", 1234)
                .await
                .unwrap()
                .collect::<Vec<_>>();
            assert!(addrs.iter().all(SocketAddr::is_ipv6), "addrs: {:?}", addrs);
        };

        rt.block_on(f);
    }
}
