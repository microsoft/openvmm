// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VPCI bus implementation.

use crate::device::NotPciDevice;
use crate::device::VpciChannel;
use crate::device::VpciConfigSpace;
use crate::device::VpciConfigSpaceOffset;
use crate::device::VpciConfigSpaceVtom;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use closeable_mutex::CloseableMutex;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guid::Guid;
use hvdef::HV_PAGE_SIZE;
use inspect::InspectMut;
use std::sync::Arc;
use thiserror::Error;
use vmbus_channel::simple::SimpleDeviceHandle;
use vmbus_channel::simple::offer_simple_device;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vpci_protocol as protocol;
use vpci_protocol::SlotNumber;

/// A VPCI bus, which can be used to enumerate PCI devices to a guest over
/// vmbus.
///
/// Note that this implementation only allows a single device per bus currently.
/// In practice, this is the only used and well-tested configuration in Hyper-V.
#[derive(InspectMut)]
pub struct VpciBus {
    #[inspect(mut, flatten)]
    bus_device: VpciBusDevice,
    #[inspect(flatten)]
    channel: SimpleDeviceHandle<VpciChannel>,
}

/// The chipset device portion of the VPCI bus.
///
/// This is primarily used for testing. You should use [`VpciBus`] in
/// product code to get a single device/state unit.
#[derive(InspectMut)]
pub struct VpciBusDevice {
    #[inspect(skip)]
    device: Arc<CloseableMutex<dyn ChipsetDevice>>,
    config_space_offset: VpciConfigSpaceOffset,
    #[inspect(with = "|&x| u32::from(x)")]
    current_slot: SlotNumber,
    /// Track vtom as when isolated with vtom enabled, guests may access mmio
    /// with or without vtom set.
    vtom: Option<u64>,
}

/// An error creating a VPCI bus.
#[derive(Debug, Error)]
pub enum CreateBusError {
    /// The device is not a PCI device.
    #[error(transparent)]
    NotPci(NotPciDevice),
    /// The vmbus channel offer failed.
    #[error("failed to offer vpci vmbus channel")]
    Offer(#[source] anyhow::Error),
}

impl VpciBusDevice {
    /// Returns a new VPCI bus device, along with the vmbus channel used for bus
    /// communications.
    pub fn new(
        instance_id: Guid,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        register_mmio: &mut dyn RegisterMmioIntercept,
        msi_controller: VpciInterruptMapper,
        vtom: Option<u64>,
    ) -> Result<(Self, VpciChannel), NotPciDevice> {
        let config_space = VpciConfigSpace::new(
            register_mmio.new_io_region(&format!("vpci-{instance_id}-config"), 2 * HV_PAGE_SIZE),
            vtom.map(|vtom| VpciConfigSpaceVtom {
                vtom,
                control_mmio: register_mmio
                    .new_io_region(&format!("vpci-{instance_id}-config-vtom"), 2 * HV_PAGE_SIZE),
            }),
        );
        let config_space_offset = config_space.offset().clone();
        let channel = VpciChannel::new(&device, instance_id, config_space, msi_controller)?;

        let this = Self {
            device,
            config_space_offset,
            current_slot: SlotNumber::from(0),
            vtom,
        };

        Ok((this, channel))
    }
}

impl VpciBus {
    /// Creates a new VPCI bus.
    pub async fn new(
        driver_source: &VmTaskDriverSource,
        instance_id: Guid,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        register_mmio: &mut dyn RegisterMmioIntercept,
        vmbus: &dyn vmbus_channel::bus::ParentBus,
        msi_controller: VpciInterruptMapper,
        vtom: Option<u64>,
    ) -> Result<Self, CreateBusError> {
        let (bus, channel) = VpciBusDevice::new(
            instance_id,
            device.clone(),
            register_mmio,
            msi_controller.clone(),
            vtom,
        )
        .map_err(CreateBusError::NotPci)?;
        let channel = offer_simple_device(driver_source, vmbus, channel)
            .await
            .map_err(CreateBusError::Offer)?;

        Ok(Self {
            bus_device: bus,
            channel,
        })
    }
}

impl ChangeDeviceState for VpciBus {
    fn start(&mut self) {
        self.channel.start();
    }

    async fn stop(&mut self) {
        self.channel.stop().await;
    }

    async fn reset(&mut self) {
        self.channel.reset().await;
    }
}

impl SaveRestore for VpciBus {
    // TODO: support saved state
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl ChipsetDevice for VpciBus {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.bus_device.supports_mmio()
    }
}

impl ChipsetDevice for VpciBusDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl MmioIntercept for VpciBusDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        tracing::trace!(addr, "VPCI bus MMIO read");

        // Remove vtom, as the guest may access it with or without set.
        let addr = addr & !self.vtom.unwrap_or(0);

        let reg = match self.register(addr, data.len()) {
            Ok(reg) => reg,
            Err(err) => return IoResult::Err(err),
        };
        match reg {
            Register::SlotNumber => return IoResult::Err(IoError::InvalidRegister),
            Register::ConfigSpace(offset) => {
                // FUTURE: support a bus with multiple devices.
                if u32::from(self.current_slot) == 0 {
                    let mut device = self.device.lock();
                    let pci = device.supports_pci().unwrap();
                    let mut buf = 0;
                    read_as_u32_chunks(offset, data, |addr| {
                        pci.pci_cfg_read(addr, &mut buf)
                            .now_or_never()
                            .map(|_| buf)
                            .unwrap_or(0)
                    });
                } else {
                    tracelimit::warn_ratelimited!(slot = ?self.current_slot, offset, "no device at slot for config space read");
                    data.fill(!0);
                }
            }
        }
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        tracing::trace!(addr, "VPCI bus MMIO write");

        // Remove vtom, as the guest may access it with or without set.
        let addr = addr & !self.vtom.unwrap_or(0);

        let reg = match self.register(addr, data.len()) {
            Ok(reg) => reg,
            Err(err) => return IoResult::Err(err),
        };
        match reg {
            Register::SlotNumber => {
                let Ok(data) = data.try_into().map(u32::from_ne_bytes) else {
                    return IoResult::Err(IoError::InvalidAccessSize);
                };
                self.current_slot = SlotNumber::from(data);
            }
            Register::ConfigSpace(offset) => {
                // FUTURE: support a bus with multiple devices.
                if u32::from(self.current_slot) == 0 {
                    let mut device = self.device.lock();
                    let pci = device.supports_pci().unwrap();
                    let mut buf = 0;
                    write_as_u32_chunks(offset, data, |address, request_type| match request_type {
                        ReadWriteRequestType::Write(value) => {
                            pci.pci_cfg_write(address, value).unwrap();
                            None
                        }
                        ReadWriteRequestType::Read => Some(
                            pci.pci_cfg_read(address, &mut buf)
                                .now_or_never()
                                .map(|_| buf)
                                .unwrap_or(0),
                        ),
                    });
                } else {
                    tracelimit::warn_ratelimited!(slot = ?self.current_slot, offset, "no device at slot for config space write");
                }
            }
        }
        IoResult::Ok
    }
}

enum Register {
    SlotNumber,
    ConfigSpace(u16),
}

impl VpciBusDevice {
    fn register(&self, addr: u64, len: usize) -> Result<Register, IoError> {
        // Note that this base address might be concurrently changing. We can
        // ignore accesses that are to addresses that don't make sense.
        let config_base = self
            .config_space_offset
            .get()
            .ok_or(IoError::InvalidRegister)?;

        let offset = addr.wrapping_sub(config_base);
        let page = offset & protocol::MMIO_PAGE_MASK;
        let offset_in_page = (offset & !protocol::MMIO_PAGE_MASK) as u16;

        // Accesses cannot straddle a page boundary.
        if (offset_in_page as u64 + len as u64) & protocol::MMIO_PAGE_MASK != 0 {
            return Err(IoError::InvalidAccessSize);
        }

        let reg = match page {
            protocol::MMIO_PAGE_SLOT_NUMBER => {
                // Only a 32-bit access at the beginning of the page is allowed.
                if offset_in_page != 0 {
                    return Err(IoError::InvalidRegister);
                }
                if len != 4 {
                    return Err(IoError::InvalidAccessSize);
                }
                Register::SlotNumber
            }
            protocol::MMIO_PAGE_CONFIG_SPACE => Register::ConfigSpace(offset_in_page),
            _ => return Err(IoError::InvalidRegister),
        };

        Ok(reg)
    }
}
