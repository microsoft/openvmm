// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the interrupts of a restored transport: a restored PCI device
//! delivers its queues' MSI-X messages once it starts.

use super::*;
use test_with_tracing::test;

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

/// A restored PCI device delivers its queue's interrupts to the MSI-X vector
/// that the guest programmed before the save, although the guest does not
/// program it again, and the restore itself delivers nothing.
#[async_test]
async fn pci_restored_msix_vector_delivers_after_start(driver: DefaultDriver) {
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::AsyncSaveRestore;

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
