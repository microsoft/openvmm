// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for restarting the queues of an active device when the VM starts:
//! the confirmation of the start, a queue that fails to start, and the
//! progress of the queues that started before it.

use super::*;
use test_with_tracing::test;

/// The state that a [`ProgressDevice`] queue reports when it stops, having
/// completed one request since it started at `state`.
fn advanced(state: QueueState) -> QueueState {
    QueueState {
        avail_index: state.avail_index.wrapping_add(1),
        used_index: state.used_index.wrapping_add(1),
    }
}

/// A two-queue device whose queues each complete one request while they run,
/// and whose queue start fails for the queue that the probe names.
#[derive(InspectMut)]
#[inspect(skip)]
struct ProgressDevice {
    probe: ProgressProbe,
    running: [Option<QueueState>; 2],
}

#[derive(Clone, Default)]
struct ProgressProbe {
    /// Per queue, the initial state of its last start.
    last_start: Arc<Mutex<[Option<QueueState>; 2]>>,
    /// The queue whose start fails.
    fail_at: Arc<Mutex<Option<u16>>>,
    /// The queue whose stop reports no progress.
    forget_at: Arc<Mutex<Option<u16>>>,
}

impl ProgressProbe {
    fn last_start(&self, idx: usize) -> Option<QueueState> {
        self.last_start.lock()[idx]
    }

    fn fail_at(&self, idx: Option<u16>) {
        *self.fail_at.lock() = idx;
    }

    fn forget_at(&self, idx: Option<u16>) {
        *self.forget_at.lock() = idx;
    }
}

impl ProgressDevice {
    fn new() -> (Self, ProgressProbe) {
        let probe = ProgressProbe::default();
        (
            Self {
                probe: probe.clone(),
                running: [None; 2],
            },
            probe,
        )
    }
}

impl VirtioDevice for ProgressDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VirtioDeviceType::CONSOLE,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: 2,
            device_register_length: 0,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    async fn start_queue(
        &mut self,
        idx: u16,
        _resources: QueueResources,
        _features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            *self.probe.fail_at.lock() != Some(idx),
            "intentional queue start failure"
        );
        self.probe.last_start.lock()[usize::from(idx)] = initial_state;
        self.running[usize::from(idx)] = Some(initial_state.unwrap_or_default());
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        let state = self.running[usize::from(idx)].take().map(advanced);
        // A device may report no progress, as virtio-net does for the queue
        // pair that a failed start drops.
        if *self.probe.forget_at.lock() == Some(idx) {
            None
        } else {
            state
        }
    }
}

/// Starts the device's queues, then resumes it twice, first with the second
/// queue failing to start, as `verify_failed_start_keeps_queue_progress`
/// does, but with the first queue reporting no progress when the failed start
/// stops it: the next start resumes that queue from the progress that the
/// transport saved before the failed start.
async fn verify_failed_start_keeps_saved_progress(
    transport: &mut impl TestTransport,
    probe: &ProgressProbe,
) {
    transport.write_driver_ok();
    yield_and_poll(transport).await;
    transport.stop().await;

    probe.fail_at(Some(1));
    probe.forget_at(Some(0));
    transport.start();
    transport.stop().await;

    probe.fail_at(None);
    probe.forget_at(None);
    transport.start();
    transport.stop().await;
    assert_eq!(
        probe.last_start(0),
        Some(QueueState {
            avail_index: 1,
            used_index: 1
        })
    );
}

#[async_test]
async fn failed_start_keeps_saved_progress_mmio(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_start_keeps_saved_progress(&mut transport, &probe).await;
}

#[async_test]
async fn failed_start_keeps_saved_progress_pci(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_start_keeps_saved_progress(&mut transport, &probe).await;
}

/// Starts the device's queues, then resumes it twice: first with the second
/// queue failing to start after the first queue has completed a request,
/// then successfully.
async fn verify_failed_start_keeps_queue_progress(
    transport: &mut impl TestTransport,
    probe: &ProgressProbe,
) {
    let progress = |n| QueueState {
        avail_index: n,
        used_index: n,
    };

    transport.write_driver_ok();
    yield_and_poll(transport).await;
    assert_ne!(transport.read_status() & VIRTIO_DRIVER_OK, 0);
    transport.stop().await;

    probe.fail_at(Some(1));
    transport.start();
    transport.stop().await;
    assert_eq!(probe.last_start(0), Some(progress(1)));

    // The next start resumes the first queue after the request that it
    // completed before the failed start stopped it, rather than repeating
    // that request.
    probe.fail_at(None);
    transport.start();
    transport.stop().await;
    assert_eq!(probe.last_start(0), Some(progress(2)));
    assert_eq!(probe.last_start(1), Some(progress(1)));
}

#[async_test]
async fn failed_start_keeps_queue_progress_mmio(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_start_keeps_queue_progress(&mut transport, &probe).await;
}

#[async_test]
async fn failed_start_keeps_queue_progress_pci(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_start_keeps_queue_progress(&mut transport, &probe).await;
}

/// Confirms the starts of an inactive device, which starts nothing, and of
/// the same device once its queues are active.
async fn verify_start_confirmation(transport: &mut impl TestTransport, probe: &ProgressProbe) {
    transport.start();
    transport.confirm_start().await.unwrap();
    transport.stop().await;
    assert_eq!(probe.last_start(0), None);

    transport.write_driver_ok();
    yield_and_poll(transport).await;
    transport.stop().await;
    transport.start();
    transport.confirm_start().await.unwrap();
    assert_eq!(
        probe.last_start(0),
        Some(QueueState {
            avail_index: 1,
            used_index: 1
        })
    );
    // A confirmed start leaves no answer for the stop to apply.
    transport.stop().await;
    assert_eq!(transport.read_status() & VIRTIO_DEVICE_NEEDS_RESET, 0);
}

#[async_test]
async fn start_confirmation_mmio(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_start_confirmation(&mut transport, &probe).await;
}

#[async_test]
async fn start_confirmation_pci(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_start_confirmation(&mut transport, &probe).await;
}

/// Fails the confirmation of a start whose second queue fails to start, then
/// stops and starts the device again, as the VM does when it rolls the start
/// back and later resumes: the device is never marked as needing reset, and
/// the first queue resumes after the request that it completed.
async fn verify_failed_confirmation_keeps_queue_progress(
    transport: &mut impl TestTransport,
    probe: &ProgressProbe,
) {
    transport.write_driver_ok();
    yield_and_poll(transport).await;
    transport.stop().await;

    probe.fail_at(Some(1));
    transport.start();
    let error = transport.confirm_start().await.unwrap_err();
    assert!(
        format!("{error:#}").contains("intentional queue start failure"),
        "{error:#}"
    );
    transport.stop().await;

    probe.fail_at(None);
    transport.start();
    transport.confirm_start().await.unwrap();
    assert_eq!(
        probe.last_start(0),
        Some(QueueState {
            avail_index: 2,
            used_index: 2
        })
    );
    transport.stop().await;
    assert_eq!(transport.read_status() & VIRTIO_DEVICE_NEEDS_RESET, 0);
}

#[async_test]
async fn failed_confirmation_keeps_queue_progress_mmio(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_confirmation_keeps_queue_progress(&mut transport, &probe).await;
}

#[async_test]
async fn failed_confirmation_keeps_queue_progress_pci(driver: DefaultDriver) {
    let (device, probe) = ProgressDevice::new();
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_confirmation_keeps_queue_progress(&mut transport, &probe).await;
}
