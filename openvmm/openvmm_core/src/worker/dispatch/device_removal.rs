// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device-removal tracking and control-plane policy.

use futures::FutureExt;
use futures_concurrency::prelude::*;
use openvmm_defs::rpc::VmRpc;
use pal_async::task::Spawn;
use pal_async::timer::PolledTimer;
use std::future::Future;
use std::time::Duration;

pub(super) const VPCI_EJECT_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Tracks one independently owned removal while the VM serves diagnostics.
#[derive(Default)]
pub(super) struct DeviceRemovalState {
    pending: Option<mesh::OneshotReceiver<()>>,
}

impl DeviceRemovalState {
    pub(super) fn start(
        &mut self,
        driver: &impl Spawn,
        removal: impl Future<Output = ()> + Send + 'static,
    ) {
        assert!(!self.is_pending(), "a device removal is already pending");
        let (send, recv) = mesh::oneshot();
        driver
            .spawn("remove-device", async move {
                removal.await;
                send.send(());
            })
            .detach();
        self.pending = Some(recv);
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Waits for an active removal, remaining pending while idle.
    pub(super) async fn wait(&mut self) {
        if let Some(completion) = &mut self.pending {
            completion.await.expect("device removal task failed");
            self.pending = None;
        } else {
            std::future::pending().await
        }
    }

    pub(super) async fn drain(&mut self) {
        if self.is_pending() {
            self.wait().await;
        }
    }
}

pub(super) fn can_run_during_device_removal(rpc: &VmRpc) -> bool {
    matches!(
        rpc,
        VmRpc::ReadMemory(_) | VmRpc::WriteMemory(_) | VmRpc::Nmi(_) | VmRpc::ConnectHvsock(_)
    )
}

pub(super) enum VpciEjectResult {
    Complete(anyhow::Result<()>),
    TimedOut,
}

pub(super) async fn wait_for_vpci_eject(
    driver: &(impl pal_async::driver::Driver + ?Sized),
    eject: impl Future<Output = anyhow::Result<()>>,
    grace_period: Duration,
) -> VpciEjectResult {
    let eject = eject.map(VpciEjectResult::Complete);
    let mut timer = PolledTimer::new(driver);
    let timeout = timer.sleep(grace_period).map(|_| VpciEjectResult::TimedOut);
    (eject, timeout).race().await
}

/// How waiting for the guest's half of an orderly PCIe removal ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PcieEjectResult {
    /// The guest turned slot power and the power indicator off.
    Complete,
    /// The request was voided before the guest finished: the port dropped the waiter
    /// without the guest powering the slot off.
    ///
    /// The worker defers resets, restores and other removals while one is pending, which
    /// are what void a request, so this is a defensive outcome rather than an expected one.
    Cancelled,
    /// The guest did not finish within the port's eject timeout.
    TimedOut,
}

/// Waits for the guest to power the slot off, at most `timeout`.
///
/// A completion that is already signalled wins over a zero timeout, so zero means "check,
/// do not wait". The race polls the completion first; the zero-timeout test pins that.
pub(super) async fn wait_for_pcie_eject(
    driver: &(impl pal_async::driver::Driver + ?Sized),
    completion: mesh::OneshotReceiver<()>,
    timeout: Duration,
) -> PcieEjectResult {
    let completion = completion.map(|r| match r {
        Ok(()) => PcieEjectResult::Complete,
        Err(_) => PcieEjectResult::Cancelled,
    });
    let mut timer = PolledTimer::new(driver);
    let timed_out = timer.sleep(timeout).map(|_| PcieEjectResult::TimedOut);
    (completion, timed_out).race().await
}

/// Settles an orderly PCIe removal once the wait for the guest ends.
///
/// The device is detached only when the guest finished: `detach` runs after a
/// [`PcieEjectResult::Complete`] and re-checks slot power under the port lock (the guest
/// can power the slot on again after signalling). On success the entry comes back for the
/// caller to tear down. On any failure the device is still attached, so the entry goes
/// back to the worker's tracked list through `requeue`, and the error says why: dropping
/// it would leave a registered device no later removal or teardown could reach.
///
/// Generic over the entry so the failure paths can be tested with a real timer and a
/// stand-in entry; the real entry holds a state unit, which a unit test cannot build.
pub(super) async fn settle_orderly_pcie_removal<T>(
    driver: &(impl pal_async::driver::Driver + ?Sized),
    entry: T,
    port: &str,
    completion: mesh::OneshotReceiver<()>,
    timeout: Duration,
    detach: impl FnOnce() -> anyhow::Result<()>,
    requeue: &mesh::Sender<T>,
) -> anyhow::Result<T> {
    let detached = match wait_for_pcie_eject(driver, completion, timeout).await {
        PcieEjectResult::Complete => detach(),
        PcieEjectResult::Cancelled => Err(anyhow::anyhow!(
            "orderly removal on port '{port}' was voided before the guest finished; \
             the device remains attached"
        )),
        PcieEjectResult::TimedOut => Err(anyhow::anyhow!(
            "orderly removal on port '{port}' timed out after {} ms; \
             the device remains attached",
            timeout.as_millis()
        )),
    };
    if let Err(error) = detached {
        // Leave the guest's pending request alone: the slot state decides on a retry
        // whether to press again, and a retry, orderly or not, finds the device.
        tracing::warn!(
            port,
            error = error.as_ref() as &dyn std::error::Error,
            "orderly PCIe removal did not finish"
        );
        requeue.send(entry);
        return Err(error);
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use test_with_tracing::test;

    #[pal_async::async_test]
    async fn pending_removal_keeps_running_when_wait_is_cancelled(driver: DefaultDriver) {
        let (entered, mut entered_recv) = mesh::channel();
        let (release, wait) = mesh::oneshot();
        let mut removal = DeviceRemovalState::default();
        removal.start(&driver, async move {
            entered.send(());
            wait.await.unwrap();
        });
        entered_recv.recv().await.unwrap();
        assert!(removal.wait().now_or_never().is_none());
        assert!(can_run_during_device_removal(&VmRpc::ReadMemory(
            mesh::rpc::Rpc::detached((0, 4))
        )));
        assert!(!can_run_during_device_removal(&VmRpc::Reset(
            mesh::rpc::Rpc::detached(())
        )));
        assert!(!can_run_during_device_removal(&VmRpc::RemovePcieDevice(
            mesh::rpc::Rpc::detached(("test".into(), false))
        )));
        assert_eq!(driver.spawn("control-request", async { 42 }).await, 42);
        assert!(removal.is_pending());
        release.send(());
        removal.drain().await;
        assert!(!removal.is_pending());
    }

    #[pal_async::async_test]
    async fn dropping_removal_tracker_does_not_cancel_cleanup(driver: DefaultDriver) {
        let (release, wait) = mesh::oneshot();
        let (finished, completed) = mesh::oneshot();
        let mut removal = DeviceRemovalState::default();
        removal.start(&driver, async move {
            wait.await.unwrap();
            finished.send(());
        });
        drop(removal);
        release.send(());
        completed.await.unwrap();
    }

    #[pal_async::async_test]
    async fn pcie_eject_completes_when_the_guest_powers_off(driver: DefaultDriver) {
        let (send, recv) = mesh::oneshot();
        send.send(());
        assert_eq!(
            wait_for_pcie_eject(&driver, recv, Duration::from_secs(60)).await,
            PcieEjectResult::Complete
        );
    }

    #[pal_async::async_test]
    async fn pcie_eject_zero_timeout_checks_without_waiting(driver: DefaultDriver) {
        // Repeated so a race that varied its polling order would be caught.
        for _ in 0..32 {
            let (send, recv) = mesh::oneshot();
            send.send(());
            assert_eq!(
                wait_for_pcie_eject(&driver, recv, Duration::ZERO).await,
                PcieEjectResult::Complete,
                "a finished eject must win over a zero timeout"
            );
        }
        let (_send, recv) = mesh::oneshot::<()>();
        assert_eq!(
            wait_for_pcie_eject(&driver, recv, Duration::ZERO).await,
            PcieEjectResult::TimedOut
        );
    }

    #[pal_async::async_test]
    async fn pcie_eject_times_out_when_the_guest_never_answers(driver: DefaultDriver) {
        let (_send, recv) = mesh::oneshot::<()>();
        assert_eq!(
            wait_for_pcie_eject(&driver, recv, Duration::from_millis(20)).await,
            PcieEjectResult::TimedOut
        );
    }

    /// Stands in for the worker's hot-plug entry, which holds a state unit.
    #[derive(Debug, PartialEq, Eq)]
    struct FakeEntry(u32);

    #[pal_async::async_test]
    async fn orderly_removal_that_times_out_hands_the_device_back(driver: DefaultDriver) {
        let (requeue, mut requeued) = mesh::channel();
        let (_guest, completion) = mesh::oneshot::<()>();
        let error = settle_orderly_pcie_removal(
            &driver,
            FakeEntry(7),
            "rp0",
            completion,
            Duration::from_millis(20),
            || panic!("a removal the guest never finished must not detach"),
            &requeue,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(error.to_string().contains("remains attached"), "{error}");
        assert_eq!(requeued.try_recv().unwrap(), FakeEntry(7));
    }

    #[pal_async::async_test]
    async fn orderly_removal_that_is_voided_hands_the_device_back(driver: DefaultDriver) {
        let (requeue, mut requeued) = mesh::channel();
        let (guest, completion) = mesh::oneshot::<()>();
        drop(guest);
        let error = settle_orderly_pcie_removal(
            &driver,
            FakeEntry(8),
            "rp0",
            completion,
            Duration::from_secs(60),
            || panic!("a voided removal must not detach"),
            &requeue,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("voided"), "{error}");
        assert_eq!(requeued.try_recv().unwrap(), FakeEntry(8));
    }

    #[pal_async::async_test]
    async fn orderly_removal_refused_at_detach_hands_the_device_back(driver: DefaultDriver) {
        // The guest powered the slot on again between signalling and the detach.
        let (requeue, mut requeued) = mesh::channel();
        let (guest, completion) = mesh::oneshot::<()>();
        guest.send(());
        let error = settle_orderly_pcie_removal(
            &driver,
            FakeEntry(9),
            "rp0",
            completion,
            Duration::from_secs(60),
            || Err(anyhow::anyhow!("slot controls changed")),
            &requeue,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("slot controls changed"),
            "{error}"
        );
        assert_eq!(requeued.try_recv().unwrap(), FakeEntry(9));
    }

    #[pal_async::async_test]
    async fn orderly_removal_that_finishes_returns_the_device_for_teardown(driver: DefaultDriver) {
        let (requeue, mut requeued) = mesh::channel();
        let (guest, completion) = mesh::oneshot::<()>();
        guest.send(());
        let mut detached = 0;
        let entry = settle_orderly_pcie_removal(
            &driver,
            FakeEntry(10),
            "rp0",
            completion,
            Duration::from_secs(60),
            || {
                detached += 1;
                Ok(())
            },
            &requeue,
        )
        .await
        .unwrap();
        assert_eq!(entry, FakeEntry(10));
        assert_eq!(detached, 1);
        assert!(
            requeued.try_recv().is_err(),
            "a detached device is not re-tracked"
        );
    }

    #[pal_async::async_test]
    async fn pcie_eject_reports_a_voided_request(driver: DefaultDriver) {
        let (send, recv) = mesh::oneshot::<()>();
        drop(send);
        assert_eq!(
            wait_for_pcie_eject(&driver, recv, Duration::from_secs(60)).await,
            PcieEjectResult::Cancelled
        );
    }
}
