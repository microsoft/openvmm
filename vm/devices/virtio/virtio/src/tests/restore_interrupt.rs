// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the interrupts of a restored transport: a restore holds the
//! interrupt line of the MMIO transport and the INTx line of the PCI one until
//! the device starts, and a restored PCI device delivers its queues' MSI-X
//! messages once it starts.

use super::*;
use test_with_tracing::test;
use vmcore::line_interrupt::LineSetTarget;
use vmcore::save_restore::AsyncSaveRestore;
use vmcore::save_restore::SavedStateBlob;

/// A line target that records each level that its line set gives it.
#[derive(Default)]
struct RecordingLineTarget(Mutex<Vec<bool>>);

impl LineSetTarget for RecordingLineTarget {
    fn set_irq(&self, _vector: u32, high: bool) {
        self.0.lock().push(high);
    }
}

impl RecordingLineTarget {
    /// Takes the levels recorded since the last call.
    fn take(&self) -> Vec<bool> {
        std::mem::take(&mut *self.0.lock())
    }
}

fn console_traits(device_register_length: u32) -> DeviceTraits {
    DeviceTraits {
        device_id: VirtioDeviceType::CONSOLE,
        device_features: VirtioDeviceFeatures::new()
            .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
        max_queues: 1,
        device_register_length,
        ..Default::default()
    }
}

/// Waits for the next MSI that the test interrupt controller receives.
async fn next_msi(driver: &DefaultDriver, controller: &TestPciInterruptController) -> (u64, u32) {
    let mut timer = PolledTimer::new(driver);
    for _ in 0..500 {
        if let Some(interrupt) = controller.get_next_interrupt() {
            return interrupt;
        }
        timer.sleep(Duration::from_millis(10)).await;
    }
    panic!("no MSI arrived");
}

/// A restore of an active MMIO device drives no interrupt line, although its
/// restored used-buffer status asserts the line; the start drives the line to
/// that status. A save before the start keeps the status, and a reset or a
/// removal before it leaves the line low.
#[async_test]
async fn mmio_restore_holds_the_interrupt_line_until_start(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let new_device = |interrupt| {
        VirtioMmioDevice::new(
            Box::new(TestDevice::new(&driver_source, console_traits(0), None)),
            &driver_source.simple(),
            mem.clone(),
            interrupt,
            None,
            0,
            1,
        )
        .unwrap()
    };

    let mut source = new_device(LineInterrupt::detached());
    guest
        .setup_chipset_device(&mut source, guest.queue_features())
        .await;
    source.stop().await;
    let mut saved = source.save().await.unwrap();
    saved.interrupt_status = VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER;
    let saved = SavedStateBlob::new(saved);
    drop(source);

    let target = Arc::new(RecordingLineTarget::default());
    let mut restored = new_device(LineInterrupt::new_with_target(
        "restored",
        target.clone(),
        0,
    ));
    restored
        .restore(saved.clone().parse().unwrap())
        .await
        .unwrap();
    assert!(target.take().is_empty());
    restored.stop().await;
    assert_eq!(
        restored.save().await.unwrap().interrupt_status,
        VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER
    );
    assert!(target.take().is_empty());
    restored.start();
    assert_eq!(target.take(), [true]);
    restored.stop().await;
    restored.start();
    assert!(target.take().is_empty());
    restored.stop().await;

    let target = Arc::new(RecordingLineTarget::default());
    let mut reset = new_device(LineInterrupt::new_with_target("reset", target.clone(), 0));
    reset.restore(saved.clone().parse().unwrap()).await.unwrap();
    reset.reset().await;
    reset.start();
    assert!(!target.take().contains(&true));
    reset.stop().await;

    let target = Arc::new(RecordingLineTarget::default());
    let mut removed = new_device(LineInterrupt::new_with_target("removed", target.clone(), 0));
    removed.restore(saved.parse().unwrap()).await.unwrap();
    removed.teardown().await;
    assert!(!target.take().contains(&true));
}

/// A restore of a PCI device drives no INTx line, although its restored
/// interrupt status asserts the line; the start drives the line to that
/// status, and a reset before it leaves the line low.
#[async_test]
async fn pci_restore_holds_the_intx_line_until_start(driver: DefaultDriver) {
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let new_device = |interrupt| {
        VirtioPciDevice::new(
            Box::new(TestDevice::new(&driver_source, console_traits(12), None)),
            &driver,
            GuestMemory::empty(),
            PciInterruptModel::IntX(pci_core::PciInterruptPin::IntA, interrupt),
            None,
            &mut ExternallyManagedMmioIntercepts,
            None,
        )
        .unwrap()
    };

    let mut source = new_device(LineInterrupt::detached());
    source.stop().await;
    let mut saved = source.save().await.unwrap();
    saved.interrupt_status = 1;
    let saved = SavedStateBlob::new(saved);
    drop(source);

    let target = Arc::new(RecordingLineTarget::default());
    let mut restored = new_device(LineInterrupt::new_with_target(
        "restored",
        target.clone(),
        0,
    ));
    restored
        .restore(saved.clone().parse().unwrap())
        .await
        .unwrap();
    assert!(target.take().is_empty());
    restored.start();
    assert_eq!(target.take(), [true]);
    restored.stop().await;

    let target = Arc::new(RecordingLineTarget::default());
    let mut reset = new_device(LineInterrupt::new_with_target("reset", target.clone(), 0));
    reset.restore(saved.parse().unwrap()).await.unwrap();
    reset.reset().await;
    reset.start();
    assert!(!target.take().contains(&true));
    reset.stop().await;
}

/// A restored PCI device delivers its queue's interrupts to the MSI-X vector
/// that the guest programmed before the save, although the guest does not
/// program it again, and the restore itself delivers nothing.
#[async_test]
async fn pci_restored_msix_vector_delivers_after_start(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let work: TestDeviceQueueWorkFn = Arc::new(
        |_: u16, queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
            queue.complete(work, 7);
        },
    );
    let mut source = VirtioPciTestDevice::new(&driver, 1, &test_mem, Some(work.clone()));
    guest
        .setup_pci_device(&mut source, guest.queue_features())
        .await;
    source.pci_device.stop().await;
    let saved = source.pci_device.save().await.unwrap();
    drop(source);

    let mut restored = VirtioPciTestDevice::new(&driver, 1, &test_mem, Some(work));
    restored.pci_device.restore(saved).await.unwrap();
    restored.pci_device.start();
    restored.pci_device.confirm_start().await.unwrap();
    assert_eq!(restored.test_intc.get_next_interrupt(), None);

    guest.add_to_avail_queue(0);
    let address = pci_notification_address(&mut restored, 0);
    restored.write_u32(address, 0);
    // `setup_pci_device` gives queue 0 vector 1, whose message has address 1
    // and data 0.
    assert_eq!(next_msi(&driver, &restored.test_intc).await, (1, 0));
    assert_eq!(guest.get_next_completed(0), Some((0, 7)));
    restored.pci_device.stop().await;
}
