// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests that drop the futures of [`VmTimeKeeper`]'s methods at their await
//! points, then check that later calls see a consistent keeper.
//!
//! The primary keeper waits for every secondary keeper to answer its
//! requests, so a manual keeper, which the test answers by hand, holds an
//! operation in flight while the test drops the future that started it.

use super::KeeperRequest;
use super::NewKeeperRequest;
use super::SavedState;
use super::Timestamp;
use super::VmTime;
use super::VmTimeKeeper;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::Either;
use futures::future::select;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::pin::pin;
use test_with_tracing::test;

/// A stop time that the manual keeper reports: an hour after the start, which
/// the other keepers have not reached, so it becomes the common stop time.
const AHEAD: VmTime = VmTime(36_000_000_000);

/// A restored VM time.
const RESTORED: VmTime = VmTime(1_234_567);

/// Creates a keeper, stopped at 0, with a manual keeper.
async fn new_keeper(driver: &DefaultDriver) -> (VmTimeKeeper, ManualKeeper) {
    let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
    let manual = ManualKeeper::new(&keeper).await;
    (keeper, manual)
}

/// Polls `op` until `until` completes, then drops `op` and returns `until`'s
/// output.
async fn drop_when<T>(op: impl Future, until: impl Future<Output = T>) -> T {
    match select(pin!(op), pin!(until)).await {
        Either::Left(_) => panic!("the operation finished before it could be dropped"),
        Either::Right((output, _)) => output,
    }
}

/// A secondary keeper that the test answers by hand.
struct ManualKeeper(mesh::Receiver<KeeperRequest>);

impl ManualKeeper {
    /// Registers a manual keeper with `keeper`'s primary keeper.
    async fn new(keeper: &VmTimeKeeper) -> Self {
        let (send, recv) = mesh::channel();
        keeper
            .builder()
            .new_send
            .call(NewKeeperRequest::New, send)
            .await
            .unwrap();
        Self(recv)
    }

    /// Receives the primary keeper's next request.
    async fn next(&mut self) -> KeeperRequest {
        self.0.next().await.expect("the primary keeper is gone")
    }

    /// Receives a start request, without answering it.
    async fn hold_start(&mut self) -> Rpc<Timestamp, ()> {
        match self.next().await {
            KeeperRequest::Start(rpc) => rpc,
            req => panic!("expected a start request, got a {} request", name(&req)),
        }
    }

    /// Receives a stop request, without answering it.
    async fn hold_stop(&mut self) -> Rpc<(), VmTime> {
        match self.next().await {
            KeeperRequest::Stop(rpc) => rpc,
            req => panic!("expected a stop request, got a {} request", name(&req)),
        }
    }

    /// Receives a reset request, without answering it.
    async fn hold_reset(&mut self) -> Rpc<VmTime, ()> {
        match self.next().await {
            KeeperRequest::Reset(rpc) => rpc,
            req => panic!("expected a reset request, got a {} request", name(&req)),
        }
    }

    /// Answers a start request, returning the VM time that it starts from.
    async fn answer_start(&mut self) -> VmTime {
        let rpc = self.hold_start().await;
        let vmtime = rpc.input().vmtime;
        rpc.complete(());
        vmtime
    }

    /// Answers a stop request as if this keeper stopped at `vmtime`.
    async fn answer_stop(&mut self, vmtime: VmTime) {
        self.hold_stop().await.complete(vmtime);
    }

    /// Answers a reset request, returning the VM time that it resets to.
    async fn answer_reset(&mut self) -> VmTime {
        let rpc = self.hold_reset().await;
        let vmtime = *rpc.input();
        rpc.complete(());
        vmtime
    }

    /// Runs `op`, which must finish without sending this keeper a request.
    async fn quietly<T>(&mut self, op: impl Future<Output = T>) -> T {
        match select(pin!(op), pin!(self.next())).await {
            Either::Left((output, _)) => output,
            Either::Right((req, _)) => panic!("unexpected {} request", name(&req)),
        }
    }
}

/// Names the kind of `req`, for failure messages.
fn name(req: &KeeperRequest) -> &'static str {
    match req {
        KeeperRequest::Start(_) => "start",
        KeeperRequest::Stop(_) => "stop",
        KeeperRequest::Reset(_) => "reset",
        KeeperRequest::Inspect(_) => "inspect",
    }
}

/// A stop whose future is dropped while the keepers stop still stops VM
/// time, and stopping again finishes it without stopping the keepers again.
#[async_test]
async fn dropped_stop_is_finished_by_stop(driver: DefaultDriver) {
    let (mut keeper, mut manual) = new_keeper(&driver).await;
    futures::join!(keeper.start(), manual.answer_start());

    let stop = drop_when(keeper.stop(), manual.hold_stop()).await;
    assert!(keeper.stop().now_or_never().is_none());
    stop.complete(AHEAD);
    assert_eq!(manual.answer_reset().await, AHEAD);

    manual.quietly(keeper.stop()).await;
    assert_eq!(keeper.save().vmtime, AHEAD);

    // The keepers see a start next, which starts from the stop time.
    let ((), start) = futures::join!(keeper.start(), manual.answer_start());
    assert_eq!(start, AHEAD);
}

/// A start finishes a stop whose future was dropped after the primary keeper
/// stopped, even when the start's own future is dropped while it waits for
/// the stop, and then starts VM time from the stop time.
#[async_test]
async fn dropped_stop_is_finished_by_start(driver: DefaultDriver) {
    let (mut keeper, mut manual) = new_keeper(&driver).await;
    let access = keeper
        .builder()
        .build(&driver)
        .await
        .unwrap()
        .access("test");
    futures::join!(keeper.start(), manual.answer_start());

    let reset = drop_when(keeper.stop(), async {
        manual.answer_stop(AHEAD).await;
        manual.hold_reset().await
    })
    .await;
    assert_eq!(*reset.input(), AHEAD);
    assert!(keeper.start().now_or_never().is_none());
    reset.complete(());

    let ((), start) = futures::join!(keeper.start(), manual.answer_start());
    assert_eq!(start, AHEAD);
    assert!(!access.now().is_before(AHEAD));

    let ((), stop_time) = futures::join!(keeper.stop(), async {
        manual.answer_stop(AHEAD).await;
        manual.answer_reset().await
    });
    assert_eq!(keeper.save().vmtime, stop_time);
    assert_eq!(access.now(), stop_time);
}

/// A reset or restore finishes a stop whose future was dropped, even when
/// the reset's own future is dropped while it waits for the stop, and then
/// resets VM time.
#[async_test]
async fn dropped_stop_is_finished_by_reset_and_restore(driver: DefaultDriver) {
    let (mut keeper, mut manual) = new_keeper(&driver).await;
    let access = keeper
        .builder()
        .build(&driver)
        .await
        .unwrap()
        .access("test");
    futures::join!(keeper.start(), manual.answer_start());

    let stop = drop_when(keeper.stop(), manual.hold_stop()).await;
    assert!(keeper.reset().now_or_never().is_none());
    stop.complete(AHEAD);
    assert_eq!(manual.answer_reset().await, AHEAD);

    let ((), reset) = futures::join!(
        keeper.restore(SavedState::from_vmtime(RESTORED)),
        manual.answer_reset()
    );
    assert_eq!(reset, RESTORED);
    assert_eq!(keeper.save().vmtime, RESTORED);
    assert_eq!(access.now(), RESTORED);
}

/// Saving cannot wait for a stop whose future was dropped, so it panics
/// rather than save a time that the stop has not reconciled.
#[async_test]
#[should_panic(expected = "VM time is still stopping")]
async fn save_panics_while_a_dropped_stop_is_pending(driver: DefaultDriver) {
    let (mut keeper, mut manual) = new_keeper(&driver).await;
    futures::join!(keeper.start(), manual.answer_start());

    let stop = drop_when(keeper.stop(), manual.hold_stop()).await;
    stop.complete(AHEAD);
    manual.answer_reset().await;
    keeper.save();
}

/// A start whose future is dropped while the keepers start still starts VM
/// time, so that a stop queued behind it stops VM time.
#[async_test]
async fn dropped_start_still_starts(driver: DefaultDriver) {
    let (mut keeper, mut manual) = new_keeper(&driver).await;

    let start = drop_when(keeper.start(), manual.hold_start()).await;
    futures::join!(keeper.stop(), async {
        start.complete(());
        manual.answer_stop(AHEAD).await;
        assert_eq!(manual.answer_reset().await, AHEAD);
    });
    assert_eq!(keeper.save().vmtime, AHEAD);
}

/// A restore whose future is dropped while the keepers reset still restores
/// VM time, so that a start queued behind it starts from the restored time.
#[async_test]
async fn dropped_restore_still_restores(driver: DefaultDriver) {
    let (mut keeper, mut manual) = new_keeper(&driver).await;

    let reset = drop_when(
        keeper.restore(SavedState::from_vmtime(RESTORED)),
        manual.hold_reset(),
    )
    .await;
    assert_eq!(*reset.input(), RESTORED);
    assert_eq!(keeper.save().vmtime, RESTORED);

    let ((), start) = futures::join!(keeper.start(), async {
        reset.complete(());
        manual.answer_start().await
    });
    assert_eq!(start, RESTORED);
}
