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

const TEMP_GPA: u64 = 0x1000000000 - 0x2000;

struct HypercallMmio(MshvHvcall);

struct DirectMmio(sparse_mmap::SparseMapping);

impl MemoryAccess for DirectMmio {
    fn gpa(&mut self) -> u64 {
        TEMP_GPA
    }

    fn read(&mut self, addr: u64) -> u32 {
        let offset = addr
            .checked_sub(self.gpa())
            .and_then(|o| o.try_into().ok())
            .unwrap_or(!0);
        match self.0.read_volatile(offset) {
            Ok(v) => v,
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
        let offset = addr
            .checked_sub(self.gpa())
            .and_then(|o| o.try_into().ok())
            .unwrap_or(!0);
        if let Err(err) = self.0.write_volatile(offset, &value) {
            tracelimit::error_ratelimited!(
                addr,
                value,
                error = &err as &dyn std::error::Error,
                "vpci mmio write failure"
            );
        }
    }
}

impl MemoryAccess for HypercallMmio {
    fn gpa(&mut self) -> u64 {
        TEMP_GPA
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

const AZIHSM_VENDOR_ID: u16 = 0x1414;
const AZIHSM_DEVICE_ID: u16 = 0xC003;

pub async fn relay_vpci_bus(
    chipset_builder: &mut ChipsetBuilder<'_>,
    driver_source: &VmTaskDriverSource,
    offer_info: vmbus_client::OfferInfo,
    dma_client: &dyn DmaClient,
    vmbus: &vmbus_server::VmbusServerControl,
) -> anyhow::Result<()> {
    let instance_id = offer_info.offer.instance_id;

    let mmio = if false {
        let mshv_hvcall = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        mshv_hvcall.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallMemoryMappedIoRead,
            hvdef::HypercallCode::HvCallMemoryMappedIoWrite,
        ]);
        Box::new(HypercallMmio(mshv_hvcall)) as _
    } else {
        let mapping = sparse_mmap::SparseMapping::new(0x2000)
            .context("failed to create sparse mapping for vpci mmio")?;
        let dev_mem = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mem")
            .context("failed to open /dev/mem")?;
        mapping
            .map_file(0, 0x2000, &dev_mem, TEMP_GPA, true)
            .context("failed to map /dev/mem for vpci mmio")?;

        Box::new(DirectMmio(mapping)) as _
    };

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
        vpci_client::VpciClient::connect(driver_source.simple(), channel, mmio, devices).await?;
    // TODO: hang onto this guy, wire him up to the inspect graph at least.
    vpci_client.detach();
    let vpci_device = devices_recv.next().await.context("no device")?;
    let vpci_device = Arc::new(
        vpci_device
            .init()
            .await
            .context("failed to initialize vpci device")?,
    );

    tracing::info!(
        "relaying vpci bus for instance {} with device {:#?}",
        instance_id,
        vpci_device.hw_ids()
    );

    if vpci_device.hw_ids().vendor_id == AZIHSM_VENDOR_ID
        && vpci_device.hw_ids().device_id == AZIHSM_DEVICE_ID
    {
        // read config space to get extended capabilities
        // extended capabilities start at offset 0x100
        let ext_cap_hdr = vpci_device.read_cfg(0x100);
        let cap_id = ext_cap_hdr & 0xFFFF;           // bits 0-15
        let cap_ver = (ext_cap_hdr >> 16) & 0xF;      // bits 16-19
        let mut next_cap_offset = (ext_cap_hdr >> 20) & 0xFFF;    // bits 20-31
        tracing::info!("cap_id: 0x{:04X}, cap_ver: 0x{:X}, next_cap_offset: 0x{:03X}", cap_id, cap_ver, next_cap_offset);

        while next_cap_offset != 0 {
            let cap_hdr = vpci_device.read_cfg(next_cap_offset as u16);
            let cap_id = cap_hdr & 0xFFFF;           // bits 0-15
            let cap_ver = (cap_hdr >> 16) & 0xF;      // bits 16-19
            let curr_offset = next_cap_offset;
            next_cap_offset = (cap_hdr >> 20) & 0xFFF;    // bits 20-31
            tracing::info!("cap_id: 0x{:04X}, cap_ver: 0x{:X}, next_cap_offset: 0x{:03X}", cap_id, cap_ver, next_cap_offset);
            if cap_id == 0x000b {
                // 000Bh Vendor-Specific Extended Capability (VSEC)
                // The structure of this capability is as follows:
                /*
                    struct
                    {
                        UINT32 VsecID : 16;  // Vendor-Specific ID
                        UINT32 VsecRev : 4;  // Version of the VSEC capability- Vendor defined
                        UINT32 VsecLen : 12; // The Number of bytes in the entire VSEC structure
                    } fields;
                */
                // header followed by 16 bytes of unique ID
                // we are interested in vsec_id as 0xc301
                let vsec_header = vpci_device.read_cfg(curr_offset as u16 + 0x4);
                let vsec_id = vsec_header & 0xFFFF; // bits 0-15
                let vsec_rev = (vsec_header >> 16) & 0xF; // bits 16-19
                let vsec_len = (vsec_header >> 20) & 0xFFF; // bits 20-31
                tracing::info!("VSEC ID: 0x{:04X}", vsec_id);
                tracing::info!("VSEC Rev: 0x{:X}, VSEC Len: 0x{:03X}", vsec_rev, vsec_len);
                if vsec_id == 0xc301 {
                    // read the unique ID
                    let unique_id = [
                        vpci_device.read_cfg(curr_offset as u16 + 0x8),
                        vpci_device.read_cfg(curr_offset as u16 + 0xC),
                        vpci_device.read_cfg(curr_offset as u16 + 0x10),
                        vpci_device.read_cfg(curr_offset as u16 + 0x14),
                    ];
                    tracing::info!("Unique ID: {:08X}-{:08X}-{:08X}-{:08X}", 
                        unique_id[0], unique_id[1], unique_id[2], unique_id[3]);
                }
            }
        }
        
    }

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
