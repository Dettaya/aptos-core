// Copyright © Aptos Foundation

use super::dag_test::MockStorage;
use crate::dag::{
    dag_fetcher::FetchRequestHandler,
    dag_store::Dag,
    tests::helpers::{new_certified_node, TEST_DAG_WINDOW},
    types::{DagSnapshotBitmask, FetchResponse, RemoteFetchRequest},
    RpcHandler,
};
use aptos_infallible::RwLock;
use aptos_types::{epoch_state::EpochState, validator_verifier::random_validator_verifier};
use claims::assert_ok_eq;
use std::sync::Arc;

#[tokio::test]
async fn test_dag_fetcher_receiver() {
    let (signers, validator_verifier) = random_validator_verifier(4, None, false);
    let epoch_state = Arc::new(EpochState {
        epoch: 1,
        verifier: validator_verifier,
    });
    let storage = Arc::new(MockStorage::new());
    let dag = Arc::new(RwLock::new(Dag::new(
        epoch_state.clone(),
        storage,
        0,
        TEST_DAG_WINDOW,
    )));

    let mut fetcher = FetchRequestHandler::new(dag.clone(), epoch_state);

    let mut first_round_nodes = vec![];

    // Round 1 - nodes 0, 1, 2 links to vec![]
    for signer in &signers[0..3] {
        let node = new_certified_node(1, signer.author(), vec![]);
        assert!(dag.write().add_node(node.clone()).is_ok());
        first_round_nodes.push(node);
    }

    // Round 2 - node 0
    let target_node = new_certified_node(2, signers[0].author(), vec![
        first_round_nodes[0].certificate(),
        first_round_nodes[1].certificate(),
    ]);

    let request = RemoteFetchRequest::new(
        target_node.epoch(),
        target_node
            .parents()
            .iter()
            .map(|parent| parent.metadata().clone())
            .collect(),
        DagSnapshotBitmask::new(1, vec![vec![true, false]]),
    );
    assert_ok_eq!(
        fetcher.process(request).await,
        FetchResponse::new(1, vec![first_round_nodes[1].clone()])
    );
}

// TODO: add more tests after commit rule tests
