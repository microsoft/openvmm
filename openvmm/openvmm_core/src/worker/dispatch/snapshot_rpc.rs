// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Filter guest snapshot requests and management mutations at the snapshot boundary.

use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use mesh::error::RemoteError;
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;

#[derive(Debug, thiserror::Error)]
#[error("VM mutation is unavailable while a microVM snapshot boundary is active")]
struct SnapshotBoundaryActive;

pub(super) fn filter_boundary_request(
    request: MicrovmSnapshotBoundaryRequest,
) -> Option<MicrovmSnapshotBoundaryRequest> {
    // Stop/reset drops the release receiver but leaves the request queued.
    // The worker serializes reset with boundary establishment, so checking
    // before gating input or stopping VPs prevents capture of a reset VM.
    if request.release_write.is_closed() {
        tracelimit::warn_ratelimited!("dropping cancelled microVM snapshot boundary request");
        return None;
    }
    Some(request)
}

pub(super) fn filter(message: VmRpc, boundary_active: bool) -> Option<VmRpc> {
    if !boundary_active {
        return Some(message);
    }

    let operation = format!("{message:?}");
    match message {
        message @ (VmRpc::QuiesceForSnapshot(_)
        | VmRpc::ResumeAfterFailedSnapshot(_)
        | VmRpc::ReleaseSnapshotBoundary(_)
        | VmRpc::ReadMemory(_)) => return Some(message),
        VmRpc::Save(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::Resume(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::Reset(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::AddVmbusDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::ConnectHvsock(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::StartReloadIgvm(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::CompleteReloadIgvm(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::WriteMemory(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::UpdateCliParams(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::AddPcieDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::RemovePcieDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::AddVpciDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::RemoveVpciDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::DumpState(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::PulseSaveRestore(rpc) => rpc.complete(Err(PulseSaveRestoreError::Other(
            RemoteError::new(SnapshotBoundaryActive),
        ))),
        VmRpc::Pause(rpc) | VmRpc::ClearHalt(rpc) => drop(rpc),
        VmRpc::Nmi(rpc) => drop(rpc),
    }
    tracelimit::warn_ratelimited!(
        rpc = operation,
        "rejected management RPC during microVM snapshot boundary"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::filter;
    use super::filter_boundary_request;
    use chipset::microvm::MicrovmSnapshotRequest;
    use chipset_device::io::IoError;
    use chipset_device::io::IoResult;
    use chipset_device::pio::PortIoIntercept;
    use chipset_device::poll_device::PollDevice;
    use futures::executor::block_on;
    use mesh::rpc::Rpc;
    use mesh::rpc::RpcSend;
    use openvmm_defs::rpc::VmRpc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;

    const SNAPSHOT_PORT: u16 = 0x605;

    #[test]
    fn snapshot_boundary_drops_requests_cancelled_by_stop_or_reset() {
        for reset in [false, true] {
            let (send, mut requests) = mesh::channel();
            let mut device = MicrovmSnapshotRequest::new(Some(send), Duration::from_secs(1));
            device.start();
            let IoResult::Defer(mut cancelled_write) = device.io_write(SNAPSHOT_PORT, &[0]) else {
                panic!("snapshot write was not deferred");
            };

            if reset {
                block_on(device.reset());
            } else {
                block_on(device.stop());
            }
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(
                cancelled_write.poll_write(&mut cx),
                Poll::Ready(Err(IoError::InvalidRegister))
            ));

            device.start();
            let IoResult::Defer(mut next_write) = device.io_write(SNAPSHOT_PORT, &[1]) else {
                panic!("next snapshot write was not deferred");
            };
            // Cancellation leaves the old request ahead of the new one in the queue.
            let old_request = requests.try_recv().unwrap();
            assert!(old_request.release_write.is_closed());
            assert!(filter_boundary_request(old_request).is_none());

            let request = filter_boundary_request(requests.try_recv().unwrap())
                .expect("a cancelled predecessor must not discard the next request");
            request.release_write.send(());
            device.poll_device(&mut cx);
            assert!(matches!(
                next_write.poll_write(&mut cx),
                Poll::Ready(Ok(()))
            ));
            block_on(request.write_completed).unwrap();
            request.transaction_complete.complete(());
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn snapshot_boundary_accepts_live_request() {
        let (send, mut requests) = mesh::channel();
        let mut device = MicrovmSnapshotRequest::new(Some(send), Duration::from_secs(1));
        device.start();
        let IoResult::Defer(mut write) = device.io_write(SNAPSHOT_PORT, &[0]) else {
            panic!("snapshot write was not deferred");
        };
        let request = filter_boundary_request(requests.try_recv().unwrap())
            .expect("live snapshot request was discarded");
        request.release_write.send(());
        let mut cx = Context::from_waker(Waker::noop());
        device.poll_device(&mut cx);
        assert!(matches!(write.poll_write(&mut cx), Poll::Ready(Ok(()))));
        block_on(request.write_completed).unwrap();
        request.transaction_complete.complete(());
    }

    #[test]
    fn snapshot_boundary_drops_request_from_dropped_device() {
        let (send, mut requests) = mesh::channel();
        let mut device = MicrovmSnapshotRequest::new(Some(send), Duration::from_secs(1));
        assert!(matches!(
            device.io_write(SNAPSHOT_PORT, &[0]),
            IoResult::Defer(_)
        ));
        drop(device);
        assert!(filter_boundary_request(requests.try_recv().unwrap()).is_none());
    }

    #[test]
    fn mutation_outside_snapshot_reaches_dispatch() {
        for message in [
            VmRpc::WriteMemory(Rpc::detached((0, vec![1]))),
            VmRpc::Reset(Rpc::detached(())),
            VmRpc::Resume(Rpc::detached(())),
            VmRpc::Pause(Rpc::detached(())),
        ] {
            assert!(filter(message, false).is_some());
        }
    }

    #[test]
    fn snapshot_control_and_reads_reach_dispatch() {
        for message in [
            VmRpc::QuiesceForSnapshot(Rpc::detached(Duration::from_secs(1))),
            VmRpc::ResumeAfterFailedSnapshot(Rpc::detached(Duration::from_secs(1))),
            VmRpc::ReleaseSnapshotBoundary(Rpc::detached(())),
            VmRpc::ReadMemory(Rpc::detached((0, 1))),
        ] {
            assert!(filter(message, true).is_some());
        }
    }

    #[test]
    fn snapshot_mutations_fail_without_waiting_for_release() {
        let (send, mut recv) = mesh::channel();
        let write = send.call(VmRpc::WriteMemory, (0, vec![1]));
        let reset = send.call(VmRpc::Reset, ());
        let resume = send.call(VmRpc::Resume, ());
        let remove = send.call(VmRpc::RemovePcieDevice, "port".to_owned());

        for _ in 0..4 {
            assert!(filter(recv.try_recv().unwrap(), true).is_none());
        }
        for result in [block_on(write), block_on(reset), block_on(remove)] {
            let error = result.unwrap().unwrap_err();
            assert!(error.to_string().contains("snapshot boundary is active"));
        }
        assert!(block_on(resume).unwrap().is_err());

        let release = send.call(VmRpc::ReleaseSnapshotBoundary, ());
        let Some(VmRpc::ReleaseSnapshotBoundary(rpc)) = filter(recv.try_recv().unwrap(), true)
        else {
            panic!("snapshot release was blocked");
        };
        rpc.complete(Ok(()));
        block_on(release).unwrap().unwrap();
    }

    #[test]
    fn infallible_mutations_report_channel_error() {
        let (send, mut recv) = mesh::channel();
        let pause = send.call(VmRpc::Pause, ());
        let clear_halt = send.call(VmRpc::ClearHalt, ());
        let nmi = send.call(VmRpc::Nmi, 0);
        for _ in 0..3 {
            assert!(filter(recv.try_recv().unwrap(), true).is_none());
        }
        assert!(block_on(pause).is_err());
        assert!(block_on(clear_halt).is_err());
        assert!(block_on(nmi).is_err());
    }

    #[test]
    fn mutation_can_be_retried_after_boundary_release() {
        let (send, mut recv) = mesh::channel();
        let first = send.call(VmRpc::WriteMemory, (0, vec![1]));
        assert!(filter(recv.try_recv().unwrap(), true).is_none());
        assert!(block_on(first).unwrap().is_err());

        let retry = send.call(VmRpc::WriteMemory, (0, vec![2]));
        let Some(VmRpc::WriteMemory(rpc)) = filter(recv.try_recv().unwrap(), false) else {
            panic!("write was blocked after boundary release");
        };
        rpc.complete(Ok(()));
        block_on(retry).unwrap().unwrap();
    }
}
