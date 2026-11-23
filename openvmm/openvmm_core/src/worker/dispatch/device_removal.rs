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
            mesh::rpc::Rpc::detached("test".into())
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
}
