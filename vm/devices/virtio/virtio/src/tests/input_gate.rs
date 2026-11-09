// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for gating host input on the transports while the guest runs, as a
//! snapshot boundary does: a gate change sends its request to the device task
//! when it is made, so the device takes it in the order of the guest's
//! register accesses, and its wait leaves the transport to them.

use super::*;
use test_with_tracing::test;

/// A device that records, in order, the changes of its input gate and the
/// guest's writes to its configuration.
#[derive(InspectMut)]
#[inspect(skip)]
struct GateOrderDevice {
    events: Arc<Mutex<Vec<String>>>,
}

impl VirtioDevice for GateOrderDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VirtioDeviceType::CONSOLE,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: 1,
            device_register_length: 4,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, val: u32) {
        self.events.lock().push(format!("config write {val}"));
    }

    async fn start_queue(
        &mut self,
        _idx: u16,
        _resources: QueueResources,
        _features: &VirtioDeviceFeatures,
        _initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop_queue(&mut self, _idx: u16) -> Option<QueueState> {
        None
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.events.lock().push("quiesce".to_owned());
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.events.lock().push("resume".to_owned());
        Ok(())
    }
}

impl GateOrderDevice {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

/// Writes the device's configuration before each gate change and while it
/// waits: each write reaches the device in the order of the change that the
/// transport made before it.
async fn verify_gate_changes_keep_the_order_of_guest_accesses(
    transport: &mut impl TestTransport,
    events: &Mutex<Vec<String>>,
) {
    let before = transport.write_device_config(1);
    let quiesce = transport.quiesce_input();
    let during_quiesce = transport.write_device_config(2);
    quiesce.await.unwrap();
    let resume = transport.resume_input();
    let during_resume = transport.write_device_config(3);
    resume.await.unwrap();
    for write in [before, during_quiesce, during_resume] {
        write.write_future().await.unwrap();
    }
    assert_eq!(
        *events.lock(),
        [
            "config write 1",
            "quiesce",
            "config write 2",
            "resume",
            "config write 3"
        ]
    );
}

#[async_test]
async fn gate_changes_keep_the_order_of_guest_accesses_mmio(driver: DefaultDriver) {
    let (device, events) = GateOrderDevice::new();
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 1);
    verify_gate_changes_keep_the_order_of_guest_accesses(&mut transport, &events).await;
}

#[async_test]
async fn gate_changes_keep_the_order_of_guest_accesses_pci(driver: DefaultDriver) {
    let (device, events) = GateOrderDevice::new();
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 1);
    verify_gate_changes_keep_the_order_of_guest_accesses(&mut transport, &events).await;
}
