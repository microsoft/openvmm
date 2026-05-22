// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL Port PCIe DVSEC extended capability implementation.

use pci_core::capabilities::extended::PciExtendedCapability;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
use pci_core::spec::caps::dvsec::DvsecHeader1;
use pci_core::spec::caps::dvsec::DvsecHeader2;

use super::spec::CXL_DVSEC_VENDOR_ID;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_ALT_MEMORY_BASE_LIMIT_WRITABLE_MASK;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT_WRITABLE_MASK;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_CONTROL_WRITABLE_MASK;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_CXL_RCRB_BASE_WRITABLE_MASK;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_ID;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_LENGTH;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_REVISION;
use super::spec::cxl_port_dvsec::CXL_PORT_DVSEC_STATUS_RW1C_MASK;
use super::spec::cxl_port_dvsec::CxlPortDvsecAltMemoryBase;
use super::spec::cxl_port_dvsec::CxlPortDvsecAltMemoryLimit;
use super::spec::cxl_port_dvsec::CxlPortDvsecAltPrefetchableMemoryBase;
use super::spec::cxl_port_dvsec::CxlPortDvsecAltPrefetchableMemoryLimit;
use super::spec::cxl_port_dvsec::CxlPortDvsecControl;
use super::spec::cxl_port_dvsec::CxlPortDvsecExtendedCapability;
use super::spec::cxl_port_dvsec::CxlPortDvsecRcrbBase;
use super::spec::cxl_port_dvsec::CxlPortDvsecRegisterOffset;
use super::spec::cxl_port_dvsec::CxlPortDvsecStatus;

impl Default for CxlPortDvsecExtendedCapability {
    fn default() -> Self {
        Self {
            status: CxlPortDvsecStatus::new(),
            control: CxlPortDvsecControl::new(),
            alt_bus_base: 0,
            alt_bus_limit: 0,
            alt_mem_base: CxlPortDvsecAltMemoryBase::new(),
            alt_mem_limit: CxlPortDvsecAltMemoryLimit::new(),
            alt_prefetch_mem_base: CxlPortDvsecAltPrefetchableMemoryBase::new(),
            alt_prefetch_mem_limit: CxlPortDvsecAltPrefetchableMemoryLimit::new(),
            alt_prefetch_mem_base_high: 0,
            alt_prefetch_mem_limit_high: 0,
            cxl_rcrb_base: CxlPortDvsecRcrbBase::new(),
            cxl_rcrb_base_high: 0,
            supports_uio_to_hdm_enable: false,
            supports_viral: false,
        }
    }
}

impl CxlPortDvsecExtendedCapability {
    /// Creates a new CXL Port DVSEC capability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables support for the `uio_to_hdm_enable` control bit.
    pub fn with_uio_to_hdm_enable(mut self, supported: bool) -> Self {
        self.supports_uio_to_hdm_enable = supported;
        self
    }

    /// Enables support for viral control and status bits.
    pub fn with_viral_support(mut self, supported: bool) -> Self {
        self.supports_viral = supported;
        self
    }

    /// Sets `Port Power Management Initialization Complete` in status.
    pub fn set_port_power_management_initialization_complete(&mut self, complete: bool) {
        self.status = self
            .status
            .with_port_power_management_initialization_complete(complete);
    }

    /// Sets `Viral Status` in status when this port type supports it.
    pub fn set_viral_status(&mut self, viral: bool) {
        if self.supports_viral {
            self.status = self.status.with_viral_status(viral);
        }
    }

    fn dvsec_len(&self) -> usize {
        usize::from(CXL_PORT_DVSEC_LENGTH)
    }

    fn read_dvsec_u32(&self, offset: u16) -> u32 {
        const DVSEC_HEADER1_OFFSET: u16 = DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0;

        match offset {
            DVSEC_HEADER1_OFFSET => Self::dvsec_header1().into_bits(),
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS => {
                u32::from(Self::dvsec_header2().into_bits())
                    | (u32::from(self.status.into_bits()) << 16)
            }
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT => {
                u32::from(self.control.into_bits())
                    | (u32::from(self.alt_bus_base) << 16)
                    | (u32::from(self.alt_bus_limit) << 24)
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT => {
                u32::from(self.alt_mem_base.into_bits())
                    | (u32::from(self.alt_mem_limit.into_bits()) << 16)
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT => {
                u32::from(self.alt_prefetch_mem_base.into_bits())
                    | (u32::from(self.alt_prefetch_mem_limit.into_bits()) << 16)
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH => {
                self.alt_prefetch_mem_base_high
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH => {
                self.alt_prefetch_mem_limit_high
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE => self.cxl_rcrb_base.into_bits(),
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH => self.cxl_rcrb_base_high,
            _ => !0,
        }
    }

    fn write_dvsec_u32(&mut self, offset: u16, value: u32) {
        match offset {
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS => {
                let mut clear_mask = ((value >> 16) as u16) & CXL_PORT_DVSEC_STATUS_RW1C_MASK;
                if !self.supports_viral {
                    clear_mask &= !CxlPortDvsecStatus::new()
                        .with_viral_status(true)
                        .into_bits();
                }
                if clear_mask != 0 {
                    let next_bits = self.status.into_bits() & !clear_mask;
                    self.status = CxlPortDvsecStatus::from_bits(next_bits);
                }
            }
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT => {
                let requested = (value as u16) & CXL_PORT_DVSEC_CONTROL_WRITABLE_MASK;
                let mut next_control = CxlPortDvsecControl::from_bits(requested);
                if !self.supports_uio_to_hdm_enable {
                    next_control = next_control.with_uio_to_hdm_enable(false);
                }
                if !self.supports_viral {
                    next_control = next_control.with_viral_enable(false);
                }
                self.control = next_control;
                self.alt_bus_base = (value >> 16) as u8;
                self.alt_bus_limit = (value >> 24) as u8;
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT => {
                let base_bits = (value as u16) & CXL_PORT_DVSEC_ALT_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                let limit_bits =
                    ((value >> 16) as u16) & CXL_PORT_DVSEC_ALT_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                self.alt_mem_base = CxlPortDvsecAltMemoryBase::from_bits(base_bits);
                self.alt_mem_limit = CxlPortDvsecAltMemoryLimit::from_bits(limit_bits);
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT => {
                let base_bits = (value as u16)
                    & CXL_PORT_DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                let limit_bits = ((value >> 16) as u16)
                    & CXL_PORT_DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                self.alt_prefetch_mem_base =
                    CxlPortDvsecAltPrefetchableMemoryBase::from_bits(base_bits);
                self.alt_prefetch_mem_limit =
                    CxlPortDvsecAltPrefetchableMemoryLimit::from_bits(limit_bits);
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH => {
                self.alt_prefetch_mem_base_high = value;
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH => {
                self.alt_prefetch_mem_limit_high = value;
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE => {
                let bits = value & CXL_PORT_DVSEC_CXL_RCRB_BASE_WRITABLE_MASK;
                self.cxl_rcrb_base = CxlPortDvsecRcrbBase::from_bits(bits);
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH => {
                self.cxl_rcrb_base_high = value;
            }
            _ => {}
        }
    }

    fn reset_state(&mut self) {
        *self = Self::default();
    }

    fn dvsec_header1() -> DvsecHeader1 {
        DvsecHeader1::new()
            .with_dvsec_vendor_id(CXL_DVSEC_VENDOR_ID)
            .with_dvsec_revision(CXL_PORT_DVSEC_REVISION)
            .with_dvsec_length(CXL_PORT_DVSEC_LENGTH)
    }

    fn dvsec_header2() -> DvsecHeader2 {
        DvsecHeader2::new().with_dvsec_id(CXL_PORT_DVSEC_ID)
    }
}

impl PciExtendedCapability for CxlPortDvsecExtendedCapability {
    fn label(&self) -> &str {
        "cxl_port_dvsec"
    }

    fn extended_capability_id(&self) -> u16 {
        ExtendedCapabilityId::DVSEC.0
    }

    fn capability_version(&self) -> u8 {
        1
    }

    fn len(&self) -> usize {
        self.dvsec_len()
    }

    fn read_u32(&self, offset: u16) -> u32 {
        if offset == 0 {
            u32::from(self.extended_capability_id()) | (u32::from(self.capability_version()) << 16)
        } else {
            self.read_dvsec_u32(offset)
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        if offset != 0 {
            self.write_dvsec_u32(offset, val);
        }
    }

    fn reset(&mut self) {
        self.reset_state();
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "cxl.pci_registers.cxl_port_dvsec")]
        pub struct SavedState {
            #[mesh(1)]
            pub status: u32,
            #[mesh(2)]
            pub control: u32,
            #[mesh(3)]
            pub alt_bus_base: u32,
            #[mesh(4)]
            pub alt_bus_limit: u32,
            #[mesh(5)]
            pub alt_mem_base: u32,
            #[mesh(6)]
            pub alt_mem_limit: u32,
            #[mesh(7)]
            pub alt_prefetch_mem_base: u32,
            #[mesh(8)]
            pub alt_prefetch_mem_limit: u32,
            #[mesh(9)]
            pub alt_prefetch_mem_base_high: u32,
            #[mesh(10)]
            pub alt_prefetch_mem_limit_high: u32,
            #[mesh(11)]
            pub cxl_rcrb_base: u32,
            #[mesh(12)]
            pub cxl_rcrb_base_high: u32,
            #[mesh(13)]
            pub supports_uio_to_hdm_enable: bool,
            #[mesh(14)]
            pub supports_viral: bool,
        }
    }

    impl SaveRestore for CxlPortDvsecExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                status: u32::from(self.status.into_bits()),
                control: u32::from(self.control.into_bits()),
                alt_bus_base: u32::from(self.alt_bus_base),
                alt_bus_limit: u32::from(self.alt_bus_limit),
                alt_mem_base: u32::from(self.alt_mem_base.into_bits()),
                alt_mem_limit: u32::from(self.alt_mem_limit.into_bits()),
                alt_prefetch_mem_base: u32::from(self.alt_prefetch_mem_base.into_bits()),
                alt_prefetch_mem_limit: u32::from(self.alt_prefetch_mem_limit.into_bits()),
                alt_prefetch_mem_base_high: self.alt_prefetch_mem_base_high,
                alt_prefetch_mem_limit_high: self.alt_prefetch_mem_limit_high,
                cxl_rcrb_base: self.cxl_rcrb_base.into_bits(),
                cxl_rcrb_base_high: self.cxl_rcrb_base_high,
                supports_uio_to_hdm_enable: self.supports_uio_to_hdm_enable,
                supports_viral: self.supports_viral,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.status = CxlPortDvsecStatus::from_bits(state.status as u16);
            self.control = CxlPortDvsecControl::from_bits(state.control as u16);
            self.alt_bus_base = state.alt_bus_base as u8;
            self.alt_bus_limit = state.alt_bus_limit as u8;
            self.alt_mem_base = CxlPortDvsecAltMemoryBase::from_bits(state.alt_mem_base as u16);
            self.alt_mem_limit = CxlPortDvsecAltMemoryLimit::from_bits(state.alt_mem_limit as u16);
            self.alt_prefetch_mem_base = CxlPortDvsecAltPrefetchableMemoryBase::from_bits(
                state.alt_prefetch_mem_base as u16,
            );
            self.alt_prefetch_mem_limit = CxlPortDvsecAltPrefetchableMemoryLimit::from_bits(
                state.alt_prefetch_mem_limit as u16,
            );
            self.alt_prefetch_mem_base_high = state.alt_prefetch_mem_base_high;
            self.alt_prefetch_mem_limit_high = state.alt_prefetch_mem_limit_high;
            self.cxl_rcrb_base = CxlPortDvsecRcrbBase::from_bits(state.cxl_rcrb_base);
            self.cxl_rcrb_base_high = state.cxl_rcrb_base_high;
            self.supports_uio_to_hdm_enable = state.supports_uio_to_hdm_enable;
            self.supports_viral = state.supports_viral;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use pci_core::capabilities::extended::PciExtendedCapability;
    use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
    use vmcore::save_restore::SaveRestore;

    use super::CxlPortDvsecControl;
    use super::CxlPortDvsecExtendedCapability;
    use super::CxlPortDvsecRegisterOffset;
    use super::CxlPortDvsecStatus;

    #[test]
    fn header_registers_match_required_constants() {
        let cap = CxlPortDvsecExtendedCapability::new();

        assert_eq!(
            cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0),
            0x0280_1e98
        );
        assert_eq!(
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS) & 0xffff,
            0x0003
        );
    }

    #[test]
    fn label_is_cxl_port_dvsec() {
        let cap = CxlPortDvsecExtendedCapability::new();
        assert_eq!(cap.label(), "cxl_port_dvsec");
    }

    #[test]
    fn unsupported_optional_control_bits_are_forced_zero() {
        let mut cap = CxlPortDvsecExtendedCapability::new();
        let requested = CxlPortDvsecControl::new()
            .with_uio_to_hdm_enable(true)
            .with_viral_enable(true)
            .into_bits();

        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT,
            u32::from(requested),
        );

        let control =
            CxlPortDvsecControl::from_bits(cap.read_u32(
                CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT,
            ) as u16);
        assert!(!control.uio_to_hdm_enable());
        assert!(!control.viral_enable());
    }

    #[test]
    fn status_viral_rw1c_is_gated_by_support() {
        let mut cap = CxlPortDvsecExtendedCapability::new().with_viral_support(true);
        cap.set_viral_status(true);

        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS,
            u32::from(
                CxlPortDvsecStatus::new()
                    .with_viral_status(true)
                    .into_bits(),
            ) << 16,
        );

        let status = CxlPortDvsecStatus::from_bits(
            (cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS) >> 16)
                as u16,
        );
        assert!(!status.viral_status());
    }

    #[test]
    fn save_restore_round_trips_state() {
        let mut cap = CxlPortDvsecExtendedCapability::new()
            .with_uio_to_hdm_enable(true)
            .with_viral_support(true);
        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT,
            0xa55a_001f,
        );
        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT,
            0x1234_5678,
        );
        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT,
            0x9abc_def0,
        );
        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH,
            0x1020_3040,
        );
        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH,
            0x5060_7080,
        );
        cap.write_u32(CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE, 0x3fff_e001);
        cap.write_u32(
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH,
            0x1111_2222,
        );
        cap.set_port_power_management_initialization_complete(true);
        cap.set_viral_status(true);

        let saved = cap.save().expect("save should succeed");
        let mut restored = CxlPortDvsecExtendedCapability::new();
        restored.restore(saved).expect("restore should succeed");

        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS)
        );
        assert_eq!(
            restored.read_u32(
                CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT
            ),
            cap.read_u32(
                CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT
            )
        );
        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT)
        );
        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT)
        );
        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH)
        );
        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH)
        );
        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE)
        );
        assert_eq!(
            restored.read_u32(CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH),
            cap.read_u32(CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH)
        );
    }
}
