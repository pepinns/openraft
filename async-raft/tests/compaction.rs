use std::sync::Arc;

use anyhow::Result;
use async_raft::raft::Entry;
use async_raft::raft::EntryPayload;
use async_raft::raft::MembershipConfig;
use async_raft::Config;
use async_raft::LogId;
use async_raft::RaftStorage;
use async_raft::SnapshotPolicy;
use async_raft::State;
use fixtures::RaftRouter;
use maplit::btreeset;

#[macro_use]
mod fixtures;

/// Compaction test.
///
/// What does this test do?
///
/// - build a stable single node cluster.
/// - send enough requests to the node that log compaction will be triggered.
/// - add new nodes and assert that they receive the snapshot.
///
/// RUST_LOG=async_raft,memstore,compaction=trace cargo test -p async-raft --test compaction
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction() -> Result<()> {
    let (_log_guard, ut_span) = init_ut!();
    let _ent = ut_span.enter();

    let snapshot_threshold: u64 = 50;

    // Setup test dependencies.
    let config = Arc::new(
        Config {
            snapshot_policy: SnapshotPolicy::LogsSinceLast(snapshot_threshold),
            max_applied_log_to_keep: 2,
            ..Default::default()
        }
        .validate()?,
    );
    let router = Arc::new(RaftRouter::new(config.clone()));
    router.new_raft_node(0).await;

    let mut n_logs = 0;

    // Assert all nodes are in non-voter state & have no entries.
    router.wait_for_log(&btreeset![0], n_logs, None, "empty").await?;
    router.wait_for_state(&btreeset![0], State::NonVoter, None, "empty").await?;

    router.assert_pristine_cluster().await;

    tracing::info!("--- initializing cluster");

    router.initialize_from_single_node(0).await?;
    n_logs += 1;

    router.wait_for_log(&btreeset![0], n_logs, None, "init leader").await?;
    router.assert_stable_cluster(Some(1), Some(1)).await;

    // Send enough requests to the cluster that compaction on the node should be triggered.
    // Puts us exactly at the configured snapshot policy threshold.
    router.client_request_many(0, "0", (snapshot_threshold - n_logs) as usize).await;
    n_logs = snapshot_threshold;

    router.wait_for_log(&btreeset![0], n_logs, None, "write").await?;
    router.assert_stable_cluster(Some(1), Some(n_logs)).await;
    router.wait_for_snapshot(&btreeset![0], LogId { term: 1, index: n_logs }, None, "snapshot").await?;

    router
        .assert_storage_state(
            1,
            n_logs,
            Some(0),
            LogId { term: 1, index: n_logs },
            Some((n_logs.into(), 1, MembershipConfig {
                members: btreeset![0],
                members_after_consensus: None,
            })),
        )
        .await;

    // Add a new node and assert that it received the same snapshot.
    let sto1 = router.new_store(1).await;
    sto1.append_to_log(&[&Entry {
        log_id: LogId { term: 1, index: 1 },
        payload: EntryPayload::Blank,
    }])
    .await?;

    router.new_raft_node_with_sto(1, sto1.clone()).await;
    router.add_non_voter(0, 1).await.expect("failed to add new node as non-voter");

    tracing::info!("--- add 1 log after snapshot");
    {
        router.client_request_many(0, "0", 1).await;
        n_logs += 1;
    }

    router.wait_for_log(&btreeset![0, 1], n_logs, None, "add follower").await?;

    tracing::info!("--- logs should be deleted after installing snapshot; left only the last one");
    {
        let sto = router.get_storage_handle(&1).await?;
        let logs = sto.get_log_entries(..).await?;
        assert_eq!(1, logs.len());
        assert_eq!(LogId { term: 1, index: 51 }, logs[0].log_id)
    }

    let expected_snap = Some((snapshot_threshold.into(), 1, MembershipConfig {
        members: btreeset![0u64],
        members_after_consensus: None,
    }));
    router
        .assert_storage_state(
            1,
            n_logs,
            None, /* non-voter does not vote */
            LogId { term: 1, index: n_logs },
            expected_snap,
        )
        .await;

    Ok(())
}
