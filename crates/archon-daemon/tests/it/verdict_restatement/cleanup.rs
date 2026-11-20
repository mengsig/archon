//! A crash between storing a stage's outcome and forgetting its attempt's
//! record leaves the record behind. History pruning takes it with its run,
//! and a record whose run is gone never stops the sweep.

use archon_core::error::ArchonError;
use archon_core::queue::QueueState;
use archon_daemon::prune;

use super::{PASS, at_e2e, disputed, kept, key, marked, only_running, result, submit};
use crate::common::{Harness, completed};

#[tokio::test]
async fn a_record_left_behind_goes_with_its_pruned_run_and_an_orphan_is_swept() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let left = h.kv(&key(&e2e)).await.expect("the ask is kept");
    assert_eq!(h.settle_running(|_| result(PASS, marked())).await.len(), 1);
    h.dispatch().await;
    only_running(&h, "report").await;
    assert_eq!(h.settle_running(|_| completed("reported")).await.len(), 1);
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    let leave_behind = async || {
        let mut store = h.ctx.store.lock().await;
        store.kv_set(&key(&e2e), &left).expect("left behind");
    };

    leave_behind().await;
    let run = h.run(&item.id).await;
    let _ = std::fs::remove_dir_all(&run.workspace);
    let later = archon_core::now() + chrono::Duration::days(15);
    let pruned = prune::prune_once(&h.ctx, later).await.expect("prune pass");
    assert_eq!(pruned.history.runs, 1, "{pruned:?}");
    let gone = h.ctx.store.lock().await.stage_runs(&run.id);
    assert!(matches!(gone, Err(ArchonError::NotFound(_))), "{gone:?}");
    assert_eq!(kept(&h, &e2e).await, None, "pruned with its run");

    // One that outlived its run all the same is deleted, and the sweep goes on.
    leave_behind().await;
    h.dispatch().await;
    assert_eq!(kept(&h, &e2e).await, None, "swept");
}
