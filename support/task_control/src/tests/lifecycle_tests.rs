// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Foo;
use super::yield_once;
use crate::StopTask;
use crate::TaskControl;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;

// Test: Mutate an uninserted task, complete it, then remove and reinsert state.
// Expected: Only reinsertion permits restart. Accessors and updates preserve
//       the task and fresh state through both activations.
#[async_test]
async fn completed_task_reactivates_only_after_fresh_state_insertion(driver: DefaultDriver) {
    let mut task = TaskControl::new(Foo(10));
    assert!(!task.has_state());
    assert!(!task.is_running());
    assert!(!task.start());
    assert!(!task.stop().await);
    assert!(task.state().is_none());
    let (worker, state) = task.get_mut();
    worker.0 += 1;
    assert!(state.is_none());
    task.update_with(|worker, state| {
        assert!(state.is_none());
        worker.0 += 1;
    });
    assert_eq!(task.get().0.0, 12);

    assert!(*task.insert(&driver, "first completion", true));
    assert!(task.has_state());
    assert!(!task.is_running());
    assert!(task.start());
    assert!(task.is_running());
    yield_once().await;
    assert!(!task.stop().await);
    assert!(!task.is_running());
    assert!(task.has_state());
    assert_eq!(task.task().0, 13);
    assert_eq!(task.state(), Some(&true));
    assert!(!task.start());
    assert!(task.remove());
    assert!(!task.has_state());
    assert!(!task.is_running());
    assert!(task.state_mut().is_none());
    assert!(!task.start());

    task.task_mut().0 = 40;
    assert!(!*task.insert(&driver, "fresh completion", false));
    task.update_with(|worker, state| {
        worker.0 += 1;
        let state = state.unwrap();
        assert!(!*state);
        *state = true;
    });
    assert_eq!(task.state(), Some(&true));
    assert!(task.start());
    yield_once().await;
    assert!(!task.stop().await);
    assert!(!task.start());
    let (worker, state) = task.into_inner();
    assert_eq!(worker.0, 42);
    assert_eq!(state, Some(true));
}

// Test: Try shared and mutable access while the worker owns its task and state.
// Expected: Both accesses panic without transferring ownership; the controller
//       can still stop the task normally.
#[async_test]
async fn running_task_rejects_direct_borrows(driver: DefaultDriver) {
    let mut task = TaskControl::new(Foo(0));
    task.insert(&driver, "exclusive task ownership", false);
    assert!(task.start());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = task.get();
        }))
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = task.get_mut();
        }))
        .is_err()
    );
    assert!(task.is_running());
    assert!(task.stop().await);
}

// Test: Consume a controller while its task is running.
// Expected: Extraction panics rather than handing out worker-owned state.
#[async_test]
async fn running_task_rejects_extraction(driver: DefaultDriver) {
    let mut task = TaskControl::new(Foo(0));
    task.insert(&driver, "exclusive extraction", false);
    assert!(task.start());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = task.into_inner();
        }))
        .is_err()
    );
}

// Test: Extract a task before transient state has ever been inserted.
// Expected: Return the original task unchanged, with no transient state.
#[test]
fn uninserted_task_extraction_preserves_absent_state() {
    let task = TaskControl::new(Foo(81));
    let (worker, state) = task.into_inner();
    assert_eq!(worker.0, 81);
    assert_eq!(state, None);
}

// Test: Queue two updates after completion, before the controller observes it.
// Expected: Stop drains both updates in order and preserves their exact task
//       and state changes, without reactivating the completed worker.
#[async_test]
async fn completed_worker_drains_accepted_updates_before_handoff(driver: DefaultDriver) {
    let mut task = TaskControl::new(Foo(2));
    task.insert(&driver, "completed update handoff", true);
    assert!(task.start());
    yield_once().await;
    task.update_with(|worker, state| {
        assert_eq!(worker.0, 3);
        worker.0 *= 10;
        let state = state.unwrap();
        assert!(*state);
        *state = false;
    });
    task.update_with(|worker, state| {
        assert_eq!(worker.0, 30);
        worker.0 += 7;
        let state = state.unwrap();
        assert!(!*state);
        *state = true;
    });
    assert!(!task.stop().await);
    assert_eq!(task.task().0, 37);
    assert_eq!(task.state(), Some(&true));
    assert!(!task.start());
}

// Test: Use the never-stopped helper for ready and yielding work, with a
//       borrowed stop-check handle.
// Expected: Both results are unchanged; checks and waits report no cancellation.
#[async_test]
async fn unstoppable_helper_preserves_results_across_waits() {
    let result = StopTask::run_unstoppable(async |stop| {
        let check = stop.check_handle().unwrap();
        let first = stop.until_stopped(std::future::ready(17)).await.unwrap();
        check.check().unwrap();
        let second = stop
            .until_stopped(async {
                yield_once().await;
                21
            })
            .await
            .unwrap();
        stop.check().unwrap();
        (first, second)
    })
    .await;
    assert_eq!(result, (17, 21));
}
