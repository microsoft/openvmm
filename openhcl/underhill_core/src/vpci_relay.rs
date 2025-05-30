use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use futures::StreamExt;
use hcl::ioctl::MshvHvcall;
use inspect::InspectMut;
use std::sync::Arc;
use user_driver::DmaClient;
use vmbus_client::local_use::Input;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ChipsetBuilder;
use vpci_client::MemoryAccess;
use vpci_client::VpciDevice;

struct Mmio(MshvHvcall);

impl MemoryAccess for Mmio {
    fn gpa(&mut self) -> u64 {
        0x2000000000 - 0x2000
    }

    fn read(&mut self, addr: u64) -> u32 {
        let mut data = [0; 4];
        match self.0.mmio_read(addr, &mut data) {
            Ok(()) => u32::from_ne_bytes(data),
            Err(err) => {
                tracelimit::error_ratelimited!(
                    addr,
                    error = &err as &dyn std::error::Error,
                    "vpci mmio read failure"
                );
                !0
            }
        }
    }

    fn write(&mut self, addr: u64, value: u32) {
        let data = value.to_ne_bytes();
        if let Err(err) = self.0.mmio_write(addr, &data) {
            tracelimit::error_ratelimited!(
                addr,
                value,
                error = &err as &dyn std::error::Error,
                "vpci mmio write failure"
            );
        }
    }
}

pub async fn relay_vpci_bus(
    chipset_builder: &mut ChipsetBuilder<'_>,
    driver_source: &VmTaskDriverSource,
    offer_info: vmbus_client::OfferInfo,
    dma_client: &dyn DmaClient,
    vmbus: &vmbus_server::VmbusServerControl,
) -> anyhow::Result<()> {
    let instance_id = offer_info.offer.instance_id;

    let mshv_hvcall = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
    mshv_hvcall.set_allowed_hypercalls(&[
        hvdef::HypercallCode::HvCallMemoryMappedIoRead,
        hvdef::HypercallCode::HvCallMemoryMappedIoWrite,
    ]);
    let mmio = Mmio(mshv_hvcall);

    let channel = vmbus_client::local_use::open_channel(
        driver_source.simple(),
        offer_info,
        Input {
            ring_pages: 20,
            ring_offset_in_pages: 10,
        },
        dma_client,
    )
    .await?;
    let (devices, mut devices_recv) = mesh::channel();
    let vpci_client =
        vpci_client::VpciClient::connect(driver_source.simple(), channel, Box::new(mmio), devices)
            .await?;
    // TODO: hang onto this guy, wire him up to the inspect graph at least.
    vpci_client.detach();
    let vpci_device = Arc::new(devices_recv.next().await.context("no device")?);

    let device_name = format!("assigned_device:vpci-{instance_id}");
    let device = chipset_builder
        .arc_mutex_device(device_name)
        .with_external_pci()
        .add(|_services| RelayedVpciDevice(vpci_device.clone()))?;

    let interrupt_mapper = VpciInterruptMapper::new(vpci_device);

    {
        let vpci_bus_name = format!("vpci:{instance_id}");
        chipset_builder
            .arc_mutex_device(vpci_bus_name)
            .try_add_async(async |services| {
                let bus = vpci::bus::VpciBus::new(
                    driver_source,
                    instance_id,
                    device,
                    &mut services.register_mmio(),
                    vmbus,
                    interrupt_mapper,
                )
                .await?;

                anyhow::Ok(bus)
            })
            .await?;
    }

    Ok(())
}

#[derive(InspectMut)]
#[inspect(transparent)]
pub struct RelayedVpciDevice(Arc<VpciDevice>);

impl ChipsetDevice for RelayedVpciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for RelayedVpciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = self.0.read_cfg(offset);
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.0.write_cfg(offset, value);
        IoResult::Ok
    }
}

impl ChangeDeviceState for RelayedVpciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl SaveRestore for RelayedVpciDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}
