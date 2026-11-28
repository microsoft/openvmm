// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use acpi::ssdt::PcieHotplugSlot;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use closeable_mutex::CloseableMutex;
use inspect::InspectMut;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use pcie::root::GenericPcieRootComplex;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::vm_task::VmTaskDriver;
use vmotherboard::MmioRevoker;

pub(super) const BASE: u32 = 0xfed8_1000;
pub(super) const IRQ: u32 = 18;

pub(super) type EjectResponse = FailableRpc<(), ()>;

struct PendingEject {
    response: Arc<Mutex<Option<EjectResponse>>>,
    _timeout: Task<()>,
}

impl PendingEject {
    fn new(
        driver: &(impl pal_async::driver::Driver + Spawn),
        deadline: Instant,
        response: EjectResponse,
    ) -> Self {
        let response = Arc::new(Mutex::new(Some(response)));
        let task = driver.spawn("acpi-pcie-eject-timeout", {
            let response = response.clone();
            let mut timer = PolledTimer::new(driver);
            async move {
                timer.sleep_until(deadline).await;
                let response = response.lock().take();
                if let Some(response) = response {
                    response.complete(Err(RemoteError::new(anyhow::anyhow!(
                        "timed out waiting for guest ACPI PCIe eject"
                    ))));
                }
            }
        });
        Self {
            response,
            _timeout: task,
        }
    }

    fn claim_response(self) -> Option<EjectResponse> {
        self.response.lock().take()
    }
}

pub(super) struct AcpiPcieHotplug {
    pub port: String,
    pub slot: PcieHotplugSlot,
    pub device: Arc<CloseableMutex<AcpiPcieEventDevice>>,
    pub ejected: mesh::Receiver<Option<EjectResponse>>,
}

#[derive(InspectMut)]
pub(super) struct AcpiPcieEventDevice {
    #[inspect(skip)]
    root: Arc<CloseableMutex<GenericPcieRootComplex>>,
    port_devfn: u8,
    #[inspect(skip)]
    registers: [(&'static str, RangeInclusive<u64>); 1],
    interrupt: LineInterrupt,
    events: u32,
    eject_requested: bool,
    #[inspect(skip)]
    pending_eject: Option<PendingEject>,
    #[inspect(skip)]
    endpoint: Option<MmioRevoker>,
    #[inspect(skip)]
    ejected: mesh::Sender<Option<EjectResponse>>,
}

impl AcpiPcieEventDevice {
    pub fn new(
        root: Arc<CloseableMutex<GenericPcieRootComplex>>,
        slot: PcieHotplugSlot,
        interrupt: LineInterrupt,
        ejected: mesh::Sender<Option<EjectResponse>>,
    ) -> Self {
        let base = u64::from(slot.mmio_base);
        Self {
            root,
            port_devfn: slot.port_devfn,
            registers: [(
                "registers",
                base..=base + u64::from(PcieHotplugSlot::REGISTER_LEN) - 1,
            )],
            interrupt,
            events: 0,
            eject_requested: false,
            pending_eject: None,
            endpoint: None,
            ejected,
        }
    }

    pub fn attach(
        &mut self,
        endpoint: Box<dyn pci_bus::GenericPciBusDevice>,
        mmio: MmioRevoker,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.endpoint.is_none(), "ACPI PCIe slot is occupied");
        self.root
            .lock()
            .add_pcie_device(self.port_devfn, "hotplug-device", endpoint)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.endpoint = Some(mmio);
        self.set_events(PcieHotplugSlot::BUS_CHECK);
        Ok(())
    }

    pub fn request_eject(
        &mut self,
        driver: VmTaskDriver,
        timeout: Duration,
        response: EjectResponse,
    ) {
        let deadline = Instant::now().saturating_add(timeout);
        let result = (|| {
            anyhow::ensure!(self.endpoint.is_some(), "ACPI PCIe slot is empty");
            anyhow::ensure!(
                !timeout.is_zero(),
                "ACPI PCIe eject timeout must be positive"
            );
            anyhow::ensure!(
                !self
                    .pending_eject
                    .as_ref()
                    .is_some_and(|pending| pending.response.lock().is_some()),
                "ACPI PCIe eject is already pending"
            );
            anyhow::Ok(())
        })();
        if let Err(error) = result {
            response.complete(Err(RemoteError::new(error)));
            return;
        }

        self.pending_eject = Some(PendingEject::new(&driver, deadline, response));
        if !self.eject_requested {
            self.eject_requested = true;
            self.set_events(self.events | PcieHotplugSlot::EJECT_REQUEST);
        }
    }

    fn eject(&mut self) -> IoResult {
        let Some(endpoint) = self.endpoint.take() else {
            tracelimit::warn_ratelimited!("ACPI PCIe eject command failed: slot is empty");
            return IoResult::Err(IoError::NoResponse);
        };
        let response = self.take_eject_response();
        if let Err(error) = self.root.lock().remove_pcie_device(self.port_devfn) {
            self.endpoint = Some(endpoint);
            self.eject_requested = false;
            tracelimit::warn_ratelimited!(%error, "ACPI PCIe eject command failed");
            if let Some(response) = response {
                response.complete(Err(RemoteError::new(error)));
            }
            return IoResult::Err(IoError::NoResponse);
        }
        endpoint.revoke();
        self.eject_requested = false;
        self.set_events(0);
        self.ejected.send(response);
        IoResult::Ok
    }

    fn take_eject_response(&mut self) -> Option<EjectResponse> {
        self.pending_eject
            .take()
            .and_then(PendingEject::claim_response)
    }

    fn set_events(&mut self, events: u32) {
        self.events = events;
        self.interrupt.set_level(events != 0);
    }

    fn register_offset(&self, address: u64, len: usize) -> Result<u64, IoError> {
        if len != size_of::<u32>() {
            return Err(IoError::InvalidAccessSize);
        }
        let offset = address
            .checked_sub(*self.registers[0].1.start())
            .ok_or(IoError::InvalidRegister)?;
        if !offset.is_multiple_of(4) {
            return Err(IoError::UnalignedAccess);
        }
        Ok(offset)
    }
}

impl ChipsetDevice for AcpiPcieEventDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use futures::FutureExt;
    use guestmem::GuestMemory;
    use memory_range::MemoryRange;
    use mesh::rpc::RpcSend;
    use pal_async::DefaultDriver;
    use pci_core::msi::MsiTarget;
    use state_unit::StateUnits;
    use test_with_tracing::test;
    use vm_resource::ResourceResolver;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;
    use vmotherboard::BaseChipsetBuilder;
    use vmotherboard::options::BaseChipsetDevices;
    use vmotherboard::options::BaseChipsetFoundation;

    struct UnexpectedEvent;

    impl vmotherboard::PowerEventHandler for UnexpectedEvent {
        fn on_power_event(&self, _: vmotherboard::PowerEvent) {
            panic!("unexpected power event");
        }
    }

    impl vmotherboard::DebugEventHandler for UnexpectedEvent {
        fn on_debug_break(&self, _: Option<u32>) {
            panic!("unexpected debug break");
        }
    }

    struct TestEndpoint;

    impl pci_bus::GenericPciBusDevice for TestEndpoint {
        fn pci_cfg_read(
            &mut self,
            _: u16,
            mut value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            value.set(0x5678_1234);
            Some(IoResult::Ok)
        }

        fn pci_cfg_write(&mut self, _: u16, _: ByteEnabledDwordWrite) -> Option<IoResult> {
            Some(IoResult::Ok)
        }
    }

    #[pal_async::async_test]
    async fn ejct_disconnects_before_host_cleanup(driver: DefaultDriver) {
        const ECAM: u64 = 0xe000_0000;
        const ENDPOINT_MMIO: u32 = BASE + 0x1000;
        const DEVFN: u8 = 8;

        let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let units = StateUnits::new();
        let (time_send, _time_recv) = mesh::channel();
        let time_unit = units.add("vmtime").build(time_send).unwrap();
        let keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let time = keeper.builder().build(&driver).await.unwrap();
        let builder = BaseChipsetBuilder::new(
            BaseChipsetFoundation {
                is_restoring: false,
                untrusted_dma_memory: GuestMemory::empty(),
                trusted_vtl0_dma_memory: GuestMemory::empty(),
                power_event_handler: Arc::new(UnexpectedEvent),
                debug_event_handler: Arc::new(UnexpectedEvent),
                vmtime: &time,
                vmtime_unit: &time_unit,
                doorbell_registration: None,
            },
            BaseChipsetDevices::empty(),
        )
        .build(&source, &units, &ResourceResolver::new())
        .await
        .unwrap()
        .chipset_builder;
        let root = builder
            .arc_mutex_device("root")
            .try_add(|services| {
                GenericPcieRootComplex::builder(
                    &mut services.register_mmio(),
                    &mut services.register_pio(),
                    0..=1,
                    MemoryRange::new(ECAM..ECAM + 0x20_0000),
                )
                .root_ports(
                    vec![pcie::GenericPciePortDefinition {
                        name: "rp0".into(),
                        devfn: Some(DEVFN),
                        hotplug: false,
                        settings: Default::default(),
                    }],
                    &MsiTarget::disconnected(),
                )
                .build()
            })
            .unwrap();
        let (chipset, devices) = builder.build().unwrap();
        let slot = PcieHotplugSlot {
            bridge_index: 0,
            port_devfn: DEVFN,
            gsi: IRQ,
            mmio_base: BASE,
        };
        let (ejected, mut cleanup) = mesh::channel();
        let mut ged =
            AcpiPcieEventDevice::new(root.clone(), slot, LineInterrupt::detached(), ejected);
        let mut region = None;
        let (unit, _endpoint, revoker) = devices
            .add_dyn_device_with_revocable_mmio(&source, &units, "endpoint", async |mmio| {
                let mut control =
                    mmio.new_io_region("endpoint", u64::from(PcieHotplugSlot::REGISTER_LEN));
                control.map(u64::from(ENDPOINT_MMIO));
                region = Some(control);
                Ok(AcpiPcieEventDevice::new(
                    root,
                    PcieHotplugSlot {
                        mmio_base: ENDPOINT_MMIO,
                        ..slot
                    },
                    LineInterrupt::detached(),
                    mesh::channel().0,
                ))
            })
            .await
            .unwrap();
        ged.attach(Box::new(TestEndpoint), revoker).unwrap();
        chipset
            .mmio_write(
                0,
                ECAM + (u64::from(DEVFN) << 12) + 0x18,
                &0x0001_0100u32.to_le_bytes(),
            )
            .await;
        let endpoint_ecam = ECAM + 0x10_0000;
        let mut data = [0; 4];
        chipset.mmio_read(0, endpoint_ecam, &mut data).await;
        assert_eq!(u32::from_le_bytes(data), 0x5678_1234);
        chipset.mmio_read(0, ENDPOINT_MMIO.into(), &mut data).await;
        assert_eq!(data, [0; 4]);

        let (send, mut recv) = mesh::channel();
        let mut caller = std::pin::pin!(send.call_failable(std::convert::identity, ()));
        ged.request_eject(
            source.simple(),
            Duration::from_secs(30),
            recv.recv().await.unwrap(),
        );
        ged.mmio_write(u64::from(BASE) + 8, &1u32.to_le_bytes())
            .unwrap();

        chipset.mmio_read(0, endpoint_ecam, &mut data).await;
        assert_eq!(data, [0xff; 4]);
        assert!(chipset.is_mmio(ENDPOINT_MMIO.into()));
        chipset.mmio_read(0, ENDPOINT_MMIO.into(), &mut data).await;
        assert_eq!(data, [0xff; 4]);
        assert!(caller.as_mut().now_or_never().is_none());

        let response = cleanup.recv().await.unwrap().unwrap();
        unit.remove().await;
        drop(region);
        response.complete(Ok(()));
        caller.await.unwrap();
    }

    #[pal_async::async_test]
    async fn eject_deadline_completes_without_dispatch(driver: DefaultDriver) {
        let (send, mut recv) = mesh::channel();
        let caller = send.call_failable(std::convert::identity, ());
        let pending = PendingEject::new(&driver, Instant::now(), recv.recv().await.unwrap());

        let error = caller.await.unwrap_err();
        assert!(format!("{error:#}").contains("timed out waiting for guest ACPI PCIe eject"));
        assert!(pending.claim_response().is_none());
    }

    #[pal_async::async_test]
    async fn guest_acknowledgment_owns_response_through_cleanup(driver: DefaultDriver) {
        let (send, mut recv) = mesh::channel();
        let caller = send.call_failable(std::convert::identity, ());
        let response = recv.recv().await.unwrap();
        let now = Instant::now();
        let delay = Duration::from_millis(10);
        let pending = PendingEject::new(&driver, now + delay, response);
        let response = pending.claim_response().unwrap();

        PolledTimer::new(&driver).sleep(delay * 2).await;
        response.complete(Ok(()));
        caller.await.unwrap();
    }
}

impl MmioIntercept for AcpiPcieEventDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        let value = match self.register_offset(address, data.len()) {
            Ok(0) => self.events,
            Ok(4) => {
                if self.endpoint.is_some() {
                    0xf
                } else {
                    0
                }
            }
            Ok(_) => return IoResult::Err(IoError::InvalidRegister),
            Err(error) => return IoResult::Err(error),
        };
        data.copy_from_slice(&value.to_le_bytes());
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let offset = match self.register_offset(address, data.len()) {
            Ok(offset) => offset,
            Err(error) => return IoResult::Err(error),
        };
        let Ok(bytes) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };
        match (offset, u32::from_le_bytes(bytes)) {
            (0, mask) => self.set_events(self.events & !mask),
            (8, 1) => return self.eject(),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &self.registers
    }
}

impl ChangeDeviceState for AcpiPcieEventDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        if let Some(response) = self.take_eject_response() {
            response.complete(Err(RemoteError::new(anyhow::anyhow!(
                "VM reset while waiting for guest ACPI PCIe eject"
            ))));
        }
        self.eject_requested = false;
        self.set_events(0);
    }
}

#[derive(mesh::payload::Protobuf, vmcore::save_restore::SavedStateRoot)]
#[mesh(package = "openvmm.acpi_pcie")]
pub struct SavedState {}

impl SaveRestore for AcpiPcieEventDevice {
    type SavedState = SavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        if self.endpoint.is_some() {
            return Err(SaveError::Other(anyhow::anyhow!(
                "cannot save an occupied ACPI PCIe slot"
            )));
        }
        Ok(SavedState {})
    }

    fn restore(&mut self, _state: Self::SavedState) -> Result<(), RestoreError> {
        self.eject_requested = false;
        self.set_events(0);
        Ok(())
    }
}
