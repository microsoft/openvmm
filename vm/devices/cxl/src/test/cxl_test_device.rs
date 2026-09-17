// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL Type-3 test endpoint device.

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use inspect::InspectMut;
use mesh::MeshPayload;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use thiserror::Error;
use tracing::debug;
use vm_resource::ResolveResource;
use vm_resource::ResourceId;
use vm_resource::declare_static_resolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;

use crate::CxlComponentRegisters;
use crate::CxlDeviceDevsecExtendedCapability;
use crate::CxlFlexBusPortDvsecExtendedCapability;
use crate::CxlRegisterLocatorDvsecExtendedCapability;
use crate::component_registers::CxlHdmDecoderCapability;
use crate::component_registers::spec::hdm_decoder::CXL_HDM_DECODER_BASE_OFFSET;
use crate::component_registers::spec::hdm_decoder::CXL_HDM_DECODER_CAPABILITY_ID;
use crate::component_registers::spec::hdm_decoder::CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderBaseLowRegister;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderControlRegister;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderGlobalControlRegister;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderInterleaveGranularity;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderInterleaveWays;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderRegisterOffset;
use crate::component_registers::spec::hdm_decoder::CxlHdmDecoderSizeLowRegister;
use crate::pci_registers::spec::cxl_device_dvsec::CxlDeviceDvsecDesiredInterleave;
use crate::pci_registers::spec::cxl_device_dvsec::CxlDeviceDvsecMediaType;
use crate::pci_registers::spec::cxl_device_dvsec::CxlDeviceDvsecMemoryActiveTimeout;
use crate::pci_registers::spec::cxl_device_dvsec::CxlDeviceDvsecMemoryClass;
use crate::pci_registers::spec::register_locator_dvsec::CxlRegisterLocatorRegisterBir;
use crate::pci_registers::spec::register_locator_dvsec::CxlRegisterLocatorRegisterBlockIdentifier;
use crate::spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES;

const CXL_TEST_DEVICE_ID: u16 = 0xc102;
const MICROSOFT_VENDOR_ID: u16 = 0x1414;
const CXL_MEMORY_SUBCLASS: u8 = 0x02;
const CXL_MEMORY_PROG_IF: u8 = 0x10;
const HDM_SIZE_GRANULARITY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_HDM_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// A test CXL Type-3 endpoint with one BAR containing component registers.
#[derive(InspectMut)]
pub struct CxlTestDevice {
    cfg_space: ConfigSpaceType0Emulator,
    #[inspect(skip)]
    component_registers: CxlComponentRegisters,
    #[inspect(skip)]
    hdm_decoder_cap_offset: u16,
    #[inspect(skip)]
    hdm_io_region: Box<dyn ControlMmioIntercept>,
    #[inspect(skip)]
    hdm_active_len_bytes: u64,
    #[inspect(skip)]
    hdm_memory: Vec<u8>,
}

/// Errors when constructing a test CXL device.
#[derive(Debug, Error)]
pub enum CxlTestDeviceCreateError {
    /// HDM size must be non-zero and 256MiB-aligned.
    #[error("invalid HDM size {0:#x}; expected non-zero and 256MiB aligned")]
    InvalidHdmSize(u64),
    /// HDM size cannot fit into host addressable memory in this process.
    #[error("HDM size {0:#x} is too large for host allocation")]
    HdmSizeTooLarge(u64),
    /// HDM size exceeds the test-device safety bound.
    #[error("HDM size {actual:#x} exceeds test-device limit {max:#x}; reduce --cxl-test mem:<len>")]
    HdmSizeExceedsLimit { actual: u64, max: u64 },
    /// HDM backing allocation failed.
    #[error("failed to allocate HDM backing memory of size {0:#x}")]
    HdmAllocationFailed(u64),
    /// Failed to configure CXL device DVSEC memory ranges.
    #[error("failed to configure CXL Device DVSEC memory")]
    InvalidDeviceDvsecConfig,
    /// Failed to configure CXL register locator DVSEC.
    #[error("failed to configure CXL Register Locator DVSEC")]
    InvalidRegisterLocatorConfig,
    /// Failed to configure CXL HDM Decoder capability.
    #[error("failed to configure CXL HDM Decoder capability")]
    InvalidHdmDecoderConfig,
}

impl CxlTestDevice {
    /// Creates a new test CXL Type-3 endpoint with a BAR0 component-register aperture.
    pub fn new(
        register_mmio: &mut dyn RegisterMmioIntercept,
        hdm_size_bytes: u64,
    ) -> Result<Self, CxlTestDeviceCreateError> {
        if hdm_size_bytes == 0 || !hdm_size_bytes.is_multiple_of(HDM_SIZE_GRANULARITY_BYTES) {
            return Err(CxlTestDeviceCreateError::InvalidHdmSize(hdm_size_bytes));
        }

        if hdm_size_bytes > MAX_HDM_SIZE_BYTES {
            return Err(CxlTestDeviceCreateError::HdmSizeExceedsLimit {
                actual: hdm_size_bytes,
                max: MAX_HDM_SIZE_BYTES,
            });
        }

        let hdm_len = usize::try_from(hdm_size_bytes)
            .map_err(|_| CxlTestDeviceCreateError::HdmSizeTooLarge(hdm_size_bytes))?;

        // Use fallible reservation to avoid aborting the process on huge allocations.
        let mut hdm_memory = Vec::new();
        hdm_memory
            .try_reserve_exact(hdm_len)
            .map_err(|_| CxlTestDeviceCreateError::HdmAllocationFailed(hdm_size_bytes))?;
        hdm_memory.resize(hdm_len, 0);

        let hdm_io_region = register_mmio.new_io_region("cxl-test-device-hdm", hdm_size_bytes);

        let mut component_registers = CxlComponentRegisters::new();
        let mut hdm_decoder_cap = CxlHdmDecoderCapability::new()
            .map_err(|_| CxlTestDeviceCreateError::InvalidHdmDecoderConfig)?;
        hdm_decoder_cap
            .with_decoder_slot(
                CxlHdmDecoderInterleaveGranularity::Bytes256,
                CxlHdmDecoderInterleaveWays::Way1,
            )
            .map_err(|_| CxlTestDeviceCreateError::InvalidHdmDecoderConfig)?;
        if !component_registers.add_register(Box::new(hdm_decoder_cap)) {
            return Err(CxlTestDeviceCreateError::InvalidHdmDecoderConfig);
        }
        let Some(hdm_decoder_cap_offset) =
            component_registers.capability_offset(CXL_HDM_DECODER_CAPABILITY_ID)
        else {
            return Err(CxlTestDeviceCreateError::InvalidHdmDecoderConfig);
        };

        let bars = DeviceBars::new().bar0(
            CXL_COMPONENT_REGISTERS_SIZE_BYTES,
            BarMemoryKind::Intercept(
                register_mmio
                    .new_io_region("cxl-test-device-bar0", CXL_COMPONENT_REGISTERS_SIZE_BYTES),
            ),
        );

        let mut cxl_device_dvsec = CxlDeviceDevsecExtendedCapability::new(None, None);
        cxl_device_dvsec = cxl_device_dvsec
            .with_cxl_memory(
                hdm_size_bytes,
                None,
                CxlDeviceDvsecMediaType::VolatileMemory,
                CxlDeviceDvsecMemoryClass::Memory,
                CxlDeviceDvsecDesiredInterleave::NoInterleave,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds1,
            )
            .map_err(|_| CxlTestDeviceCreateError::InvalidDeviceDvsecConfig)?;

        let flex_bus_dvsec = CxlFlexBusPortDvsecExtendedCapability::new()
            .with_mem_capable(true)
            .with_cache_capable(true);

        // TODO(cxl): This test endpoint is not a Linux-complete Type-3 memdev yet.
        // It only advertises RBI_COMPONENT in Register Locator DVSEC and does not
        // expose an RBI_MEMDEV block with Device Status / Primary Mailbox / Memdev
        // capabilities, so Linux cxl_pci may bind but cannot complete full memdev bring-up.
        let register_locator_dvsec = CxlRegisterLocatorDvsecExtendedCapability::new()
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_10H,
                CxlRegisterLocatorRegisterBlockIdentifier::COMPONENT_REGISTERS,
                0,
            )
            .map_err(|_| CxlTestDeviceCreateError::InvalidRegisterLocatorConfig)?;

        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: MICROSOFT_VENDOR_ID,
                device_id: CXL_TEST_DEVICE_ID,
                revision_id: 0,
                prog_if: ProgrammingInterface::from(CXL_MEMORY_PROG_IF),
                sub_class: Subclass::from(CXL_MEMORY_SUBCLASS),
                base_class: ClassCode::MEMORY_CONTROLLER,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::Endpoint,
                None,
            ))],
            vec![
                Box::new(cxl_device_dvsec),
                Box::new(flex_bus_dvsec),
                Box::new(register_locator_dvsec),
            ],
            bars,
        );

        Ok(Self {
            cfg_space,
            component_registers,
            hdm_decoder_cap_offset,
            hdm_io_region,
            hdm_active_len_bytes: 0,
            hdm_memory,
        })
    }

    fn read_component_u32(&self, offset: u16) -> Option<u32> {
        let mut buf = [0u8; 4];
        if !matches!(
            self.component_registers.read(offset, &mut buf),
            IoResult::Ok
        ) {
            return None;
        }

        Some(u32::from_le_bytes(buf))
    }

    fn refresh_hdm_mapping_from_decoder_state(&mut self) {
        debug!("refreshing HDM mapping from decoder state");
        let Some(global_control_bits) = self.read_component_u32(
            self.hdm_decoder_cap_offset + CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET,
        ) else {
            debug!("HDM mapping refresh: missing global control, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        };

        let global_control = CxlHdmDecoderGlobalControlRegister::from_bits(global_control_bits);
        debug!(
            global_control_bits,
            hdm_decoder_enable = global_control.hdm_decoder_enable(),
            poison_on_decode_error_enable = global_control.poison_on_decode_error_enable(),
            "HDM mapping refresh: decoded global control"
        );
        if !global_control.hdm_decoder_enable() {
            debug!("HDM mapping refresh: decoder disabled, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        }

        let decoder0 = self.hdm_decoder_cap_offset + CXL_HDM_DECODER_BASE_OFFSET;
        let Some(base_low_bits) =
            self.read_component_u32(decoder0 + CxlHdmDecoderRegisterOffset::BASE_LOW)
        else {
            debug!("HDM mapping refresh: missing BASE_LOW, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        };
        let Some(base_high) =
            self.read_component_u32(decoder0 + CxlHdmDecoderRegisterOffset::BASE_HIGH)
        else {
            debug!("HDM mapping refresh: missing BASE_HIGH, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        };
        let Some(size_low_bits) =
            self.read_component_u32(decoder0 + CxlHdmDecoderRegisterOffset::SIZE_LOW)
        else {
            debug!("HDM mapping refresh: missing SIZE_LOW, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        };
        let Some(size_high) =
            self.read_component_u32(decoder0 + CxlHdmDecoderRegisterOffset::SIZE_HIGH)
        else {
            debug!("HDM mapping refresh: missing SIZE_HIGH, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        };
        let Some(control_bits) =
            self.read_component_u32(decoder0 + CxlHdmDecoderRegisterOffset::CONTROL)
        else {
            debug!("HDM mapping refresh: missing CONTROL, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        };

        let base_low = CxlHdmDecoderBaseLowRegister::from_bits(base_low_bits);
        let size_low = CxlHdmDecoderSizeLowRegister::from_bits(size_low_bits);
        let control = CxlHdmDecoderControlRegister::from_bits(control_bits);
        debug!(
            base_low_bits,
            base_high,
            size_low_bits,
            size_high,
            control_bits,
            committed = control.committed(),
            commit = control.commit(),
            lock_on_commit = control.lock_on_commit(),
            "HDM mapping refresh: decoder register snapshot"
        );

        if !control.committed() {
            debug!("HDM mapping refresh: decoder not committed, unmapping HDM range");
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        }

        let base = (u64::from(base_high) << 32) | (u64::from(base_low.memory_base_low()) << 28);
        let size = (u64::from(size_high) << 32) | (u64::from(size_low.memory_size_low()) << 28);
        debug!(
            base,
            size, "HDM mapping refresh: computed decoder base/size"
        );

        if size == 0 || size > self.hdm_io_region.len() {
            debug!(
                size,
                max_size = self.hdm_io_region.len(),
                "HDM mapping refresh: invalid decoder size for backing memory, unmapping HDM range"
            );
            self.hdm_io_region.unmap();
            self.hdm_active_len_bytes = 0;
            return;
        }

        self.hdm_io_region.map(base);
        self.hdm_active_len_bytes = size;
        debug!(base, size, "HDM mapping refresh: mapped HDM MMIO region");
    }

    fn hdm_memory_offset(&self, addr: u64, access_len: usize) -> Option<usize> {
        if self.hdm_active_len_bytes == 0 {
            return None;
        }

        let access_len = u64::try_from(access_len).ok()?;
        let offset = self.hdm_io_region.offset_of(addr)?;
        let end = offset.checked_add(access_len)?;
        if end > self.hdm_active_len_bytes {
            return None;
        }

        usize::try_from(offset).ok()
    }

    fn read_hdm_memory(&self, addr: u64, data: &mut [u8]) -> IoResult {
        let Some(offset) = self.hdm_memory_offset(addr, data.len()) else {
            return IoResult::Err(InvalidRegister);
        };

        let end = offset + data.len();
        data.copy_from_slice(&self.hdm_memory[offset..end]);
        IoResult::Ok
    }

    fn write_hdm_memory(&mut self, addr: u64, data: &[u8]) -> IoResult {
        let Some(offset) = self.hdm_memory_offset(addr, data.len()) else {
            return IoResult::Err(InvalidRegister);
        };

        let end = offset + data.len();
        self.hdm_memory[offset..end].copy_from_slice(data);
        IoResult::Ok
    }

    fn read_component_registers(&self, offset: u64, data: &mut [u8]) -> IoResult {
        let Ok(offset) = u16::try_from(offset) else {
            data.fill(0);
            return IoResult::Ok;
        };

        match self.component_registers.read(offset, data) {
            IoResult::Err(InvalidRegister) => {
                data.fill(0);
                IoResult::Ok
            }
            res => res,
        }
    }

    fn write_component_registers(&mut self, offset: u64, data: &[u8]) -> IoResult {
        let Ok(offset) = u16::try_from(offset) else {
            return IoResult::Ok;
        };

        match self.component_registers.write(offset, data) {
            IoResult::Err(InvalidRegister) => IoResult::Ok,
            res => {
                debug!(
                    offset,
                    write_len = data.len(),
                    "component register write accepted; refreshing HDM mapping"
                );
                self.refresh_hdm_mapping_from_decoder_state();
                res
            }
        }
    }
}

impl ChangeDeviceState for CxlTestDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.cfg_space.reset();
        self.component_registers.reset();
        self.refresh_hdm_mapping_from_decoder_state();
        self.hdm_memory.fill(0);
    }
}

impl ChipsetDevice for CxlTestDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl MmioIntercept for CxlTestDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        match self.cfg_space.find_bar(addr) {
            Some((0, offset)) => self.read_component_registers(offset, data),
            _ => self.read_hdm_memory(addr, data),
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        match self.cfg_space.find_bar(addr) {
            Some((0, offset)) => self.write_component_registers(offset, data),
            _ => self.write_hdm_memory(addr, data),
        }
    }
}

impl PciConfigSpace for CxlTestDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        self.cfg_space.read_byte_enabled(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        self.cfg_space.write_byte_enabled(offset, value)
    }
}

impl SaveRestore for CxlTestDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}

/// Resource handle for the CXL Type-3 test endpoint.
#[derive(MeshPayload)]
pub struct CxlTestDeviceHandle {
    /// HDM size in bytes. Must be non-zero and 256MiB-aligned.
    pub hdm_size_bytes: u64,
}

impl ResourceId<PciDeviceHandleKind> for CxlTestDeviceHandle {
    const ID: &'static str = "cxl_test";
}

/// Resource resolver for [`CxlTestDeviceHandle`].
pub mod resolver {
    use super::CxlTestDevice;
    use super::CxlTestDeviceCreateError;
    use super::CxlTestDeviceHandle;
    use super::ResolvePciDeviceHandleParams;
    use super::ResolvedPciDevice;
    use super::*;

    /// Resolver for CXL test devices.
    pub struct CxlTestDeviceResolver;

    declare_static_resolver!(
        CxlTestDeviceResolver,
        (PciDeviceHandleKind, CxlTestDeviceHandle)
    );

    /// Error returned by [`CxlTestDeviceResolver`].
    #[derive(Debug, Error)]
    pub enum Error {
        /// CXL test device creation failed.
        #[error(transparent)]
        Create(#[from] CxlTestDeviceCreateError),
    }

    impl ResolveResource<PciDeviceHandleKind, CxlTestDeviceHandle> for CxlTestDeviceResolver {
        type Output = ResolvedPciDevice;
        type Error = Error;

        fn resolve(
            &self,
            resource: CxlTestDeviceHandle,
            input: ResolvePciDeviceHandleParams<'_>,
        ) -> Result<Self::Output, Self::Error> {
            let device = CxlTestDevice::new(input.register_mmio, resource.hdm_size_bytes)?;
            Ok(device.into())
        }
    }
}
