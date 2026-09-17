// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL Port PCIe DVSEC extended capability implementation.

use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
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

    fn read_dvsec(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        const DVSEC_HEADER1_OFFSET: u16 = DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0;

        match CxlPortDvsecRegisterOffset(offset) {
            _ if offset == DVSEC_HEADER1_OFFSET => value.set(Self::dvsec_header1().into_bits()),
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS => {
                value.set_low_high(Self::dvsec_header2().into_bits(), self.status.into_bits());
            }
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT => {
                value.set_low_high(
                    self.control.into_bits(),
                    (self.alt_bus_base as u16) | ((self.alt_bus_limit as u16) << 8),
                );
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT => {
                value.set_low_high(
                    self.alt_mem_base.into_bits(),
                    self.alt_mem_limit.into_bits(),
                );
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT => {
                value.set_low_high(
                    self.alt_prefetch_mem_base.into_bits(),
                    self.alt_prefetch_mem_limit.into_bits(),
                );
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH => {
                value.set(self.alt_prefetch_mem_base_high);
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH => {
                value.set(self.alt_prefetch_mem_limit_high);
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE => {
                value.set(self.cxl_rcrb_base.into_bits())
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH => {
                value.set(self.cxl_rcrb_base_high)
            }
            _ => value.set(!0),
        }
    }

    fn write_dvsec(&mut self, offset: u16, value: ByteEnabledDwordWrite) {
        match CxlPortDvsecRegisterOffset(offset) {
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS => {
                let mut clear_mask = value.extract_high() & CXL_PORT_DVSEC_STATUS_RW1C_MASK;
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
                let requested = value.merge_low(self.control.into_bits())
                    & CXL_PORT_DVSEC_CONTROL_WRITABLE_MASK;
                let mut next_control = CxlPortDvsecControl::from_bits(requested);
                if !self.supports_uio_to_hdm_enable {
                    next_control = next_control.with_uio_to_hdm_enable(false);
                }
                if !self.supports_viral {
                    next_control = next_control.with_viral_enable(false);
                }
                self.control = next_control;
                let alt_bus = value.merge_high(
                    u16::from(self.alt_bus_base) | (u16::from(self.alt_bus_limit) << 8),
                );
                self.alt_bus_base = alt_bus as u8;
                self.alt_bus_limit = (alt_bus >> 8) as u8;
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT => {
                let base_bits = value.merge_low(self.alt_mem_base.into_bits())
                    & CXL_PORT_DVSEC_ALT_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                let limit_bits = value.merge_high(self.alt_mem_limit.into_bits())
                    & CXL_PORT_DVSEC_ALT_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                self.alt_mem_base = CxlPortDvsecAltMemoryBase::from_bits(base_bits);
                self.alt_mem_limit = CxlPortDvsecAltMemoryLimit::from_bits(limit_bits);
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT => {
                let base_bits = value.merge_low(self.alt_prefetch_mem_base.into_bits())
                    & CXL_PORT_DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                let limit_bits = value.merge_high(self.alt_prefetch_mem_limit.into_bits())
                    & CXL_PORT_DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT_WRITABLE_MASK;
                self.alt_prefetch_mem_base =
                    CxlPortDvsecAltPrefetchableMemoryBase::from_bits(base_bits);
                self.alt_prefetch_mem_limit =
                    CxlPortDvsecAltPrefetchableMemoryLimit::from_bits(limit_bits);
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH => {
                value.merge_into(&mut self.alt_prefetch_mem_base_high);
            }
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH => {
                value.merge_into(&mut self.alt_prefetch_mem_limit_high);
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE => {
                let bits = value.merge(self.cxl_rcrb_base.into_bits())
                    & CXL_PORT_DVSEC_CXL_RCRB_BASE_WRITABLE_MASK;
                self.cxl_rcrb_base = CxlPortDvsecRcrbBase::from_bits(bits);
            }
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH => {
                value.merge_into(&mut self.cxl_rcrb_base_high);
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

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        if offset == 0 {
            value.set_low_high(
                self.extended_capability_id(),
                self.capability_version().into(),
            );
        } else {
            self.read_dvsec(offset, value);
        }
    }

    fn write(&mut self, offset: u16, value: ByteEnabledDwordWrite) {
        if offset != 0 {
            self.write_dvsec(offset, value);
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
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigByteEnable;
    use pci_core::capabilities::extended::PciExtendedCapability;
    use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
    use pci_core::test_helpers::read_extended_cap_u32;
    use pci_core::test_helpers::write_extended_cap_u32;
    use vmcore::save_restore::SaveRestore;

    use super::CxlPortDvsecControl;
    use super::CxlPortDvsecExtendedCapability;
    use super::CxlPortDvsecRegisterOffset;
    use super::CxlPortDvsecStatus;

    #[test]
    fn header_registers_match_required_constants() {
        let cap = CxlPortDvsecExtendedCapability::new();

        assert_eq!(
            read_extended_cap_u32(&cap, DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0),
            0x0280_1e98
        );
        assert_eq!(
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0
            ) & 0xffff,
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

        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT.0,
            u32::from(requested),
        );

        let control = CxlPortDvsecControl::from_bits(read_extended_cap_u32(
            &cap,
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT.0,
        ) as u16);
        assert!(!control.uio_to_hdm_enable());
        assert!(!control.viral_enable());
    }

    #[test]
    fn status_viral_rw1c_is_gated_by_support() {
        let mut cap = CxlPortDvsecExtendedCapability::new().with_viral_support(true);
        cap.set_viral_status(true);

        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0,
            u32::from(
                CxlPortDvsecStatus::new()
                    .with_viral_status(true)
                    .into_bits(),
            ) << 16,
        );

        let status = CxlPortDvsecStatus::from_bits(
            (read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0,
            ) >> 16) as u16,
        );
        assert!(!status.viral_status());
    }

    #[test]
    fn status_rw1c_ignores_disabled_byte_lanes() {
        let mut cap = CxlPortDvsecExtendedCapability::new().with_viral_support(true);
        cap.set_viral_status(true);

        cap.write(
            CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0,
            ByteEnabledDwordWrite::new(0xffff_ffff, PciConfigByteEnable::new(0b0011).unwrap()),
        );

        let status = CxlPortDvsecStatus::from_bits(
            (read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0,
            ) >> 16) as u16,
        );
        assert!(status.viral_status());
    }

    #[test]
    fn save_restore_round_trips_state() {
        let mut cap = CxlPortDvsecExtendedCapability::new()
            .with_uio_to_hdm_enable(true)
            .with_viral_support(true);
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT.0,
            0xa55a_001f,
        );
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT.0,
            0x1234_5678,
        );
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT.0,
            0x9abc_def0,
        );
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH.0,
            0x1020_3040,
        );
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH.0,
            0x5060_7080,
        );
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE.0,
            0x3fff_e001,
        );
        write_extended_cap_u32(
            &mut cap,
            CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH.0,
            0x1111_2222,
        );
        cap.set_port_power_management_initialization_complete(true);
        cap.set_viral_status(true);

        let saved = cap.save().expect("save should succeed");
        let mut restored = CxlPortDvsecExtendedCapability::new();
        restored.restore(saved).expect("restore should succeed");

        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_HEADER2_PORT_EXTENSION_STATUS.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_MEMORY_BASE_LIMIT.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlPortDvsecRegisterOffset::DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(&restored, CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE.0),
            read_extended_cap_u32(&cap, CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE.0)
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH.0
            ),
            read_extended_cap_u32(&cap, CxlPortDvsecRegisterOffset::DVSEC_CXL_RCRB_BASE_HIGH.0)
        );
    }
}
