// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL Flex Bus Port PCIe DVSEC extended capability implementation.

use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use pci_core::capabilities::extended::PciExtendedCapability;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
use pci_core::spec::caps::dvsec::DvsecHeader1;
use pci_core::spec::caps::dvsec::DvsecHeader2;

use super::spec::CXL_DVSEC_VENDOR_ID;
use super::spec::flex_bus_port_dvsec::CXL_FLEX_BUS_PORT_DVSEC_CONTROL_WRITABLE_MASK;
use super::spec::flex_bus_port_dvsec::CXL_FLEX_BUS_PORT_DVSEC_CONTROL2_WRITABLE_MASK;
use super::spec::flex_bus_port_dvsec::CXL_FLEX_BUS_PORT_DVSEC_ID;
use super::spec::flex_bus_port_dvsec::CXL_FLEX_BUS_PORT_DVSEC_LENGTH;
use super::spec::flex_bus_port_dvsec::CXL_FLEX_BUS_PORT_DVSEC_REVISION;
use super::spec::flex_bus_port_dvsec::CXL_FLEX_BUS_PORT_DVSEC_STATUS_RW1CS_MASK;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability2;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecControl;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecControl2;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecExtendedCapability;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecRegisterOffset;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecStatus;
use super::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecStatus2;

impl Default for CxlFlexBusPortDvsecExtendedCapability {
    fn default() -> Self {
        let capability = CxlFlexBusPortDvsecCapability::new().with_io_capable(true);
        let capability2 = CxlFlexBusPortDvsecCapability2::new();
        Self {
            capability,
            control: CxlFlexBusPortDvsecControl::new().with_io_enable(true),
            status: CxlFlexBusPortDvsecStatus::new(),
            received_modified_ts_data_phase1: CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1::new(
            ),
            capability2,
            control2: CxlFlexBusPortDvsecControl2::new(),
            status2: CxlFlexBusPortDvsecStatus2::new(),
            reset_baseline_capability: capability,
            reset_baseline_capability2: capability2,
        }
    }
}

impl CxlFlexBusPortDvsecExtendedCapability {
    /// Creates a new CXL Flex Bus Port DVSEC capability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures optional Flex Bus Port capabilities.
    #[expect(clippy::fn_params_excessive_bools)]
    pub fn with_optional_capabilities(
        mut self,
        cache_capable: bool,
        cxl_68b_flit_and_vh_capable: bool,
        cxl_multi_logical_device_capable: bool,
        cxl_latency_optimized_256b_flit_capable: bool,
        cxl_pbr_flit_capable: bool,
    ) -> Self {
        self.capability = self
            .capability
            .with_cache_capable(cache_capable)
            .with_cxl_68b_flit_and_vh_capable(cxl_68b_flit_and_vh_capable)
            .with_cxl_multi_logical_device_capable(cxl_multi_logical_device_capable)
            .with_cxl_latency_optimized_256b_flit_capable(cxl_latency_optimized_256b_flit_capable)
            .with_cxl_pbr_flit_capable(cxl_pbr_flit_capable);
        self.reset_baseline_capability = self.capability;
        self
    }

    /// Configures whether this port advertises CXL.mem capability.
    pub fn with_mem_capable(mut self, mem_capable: bool) -> Self {
        self.capability = self.capability.with_mem_capable(mem_capable);
        self.reset_baseline_capability = self.capability;
        self
    }

    /// Configures whether this port advertises CXL.cache capability.
    pub fn with_cache_capable(mut self, cache_capable: bool) -> Self {
        self.capability = self.capability.with_cache_capable(cache_capable);
        self.reset_baseline_capability = self.capability;
        self
    }

    /// Configures Flex Bus Port Capability2 bits.
    pub fn with_capability2(mut self, nop_hint_capable: bool, streamlined_port: bool) -> Self {
        self.capability2 = self
            .capability2
            .with_nop_hint_capable(nop_hint_capable)
            .with_streamlined_port(streamlined_port);
        self.reset_baseline_capability2 = self.capability2;
        self
    }

    /// Updates status bits that are reported by the physical layer.
    pub fn set_status(
        &mut self,
        status: CxlFlexBusPortDvsecStatus,
        received_modified_ts_data_phase1: u32,
        status2: CxlFlexBusPortDvsecStatus2,
    ) {
        self.status = status;
        self.received_modified_ts_data_phase1 =
            CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1::new()
                .with_received_flex_bus_data_phase1(received_modified_ts_data_phase1 & 0x00ff_ffff);
        self.status2 = status2;
    }

    fn dvsec_len(&self) -> usize {
        usize::from(CXL_FLEX_BUS_PORT_DVSEC_LENGTH)
    }

    fn read_dvsec(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        const DVSEC_HEADER1_OFFSET: u16 = DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0;

        match CxlFlexBusPortDvsecRegisterOffset(offset) {
            _ if offset == DVSEC_HEADER1_OFFSET => value.set(Self::dvsec_header1().into_bits()),
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_HEADER2_CAPABILITY => {
                value.set_low_high(
                    Self::dvsec_header2().into_bits(),
                    self.capability.into_bits(),
                );
            }
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS => {
                value.set_low_high(self.control.into_bits(), self.status.into_bits());
            }
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_RECEIVED_MODIFIED_TS_DATA_PHASE1 => {
                value.set(self.received_modified_ts_data_phase1.into_bits());
            }
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CAPABILITY2 => {
                value.set(self.capability2.into_bits())
            }
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2 => {
                value.set(self.control2.into_bits())
            }
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_STATUS2 => value.set(self.status2.into_bits()),
            _ => value.set(!0),
        }
    }

    fn write_dvsec(&mut self, offset: u16, value: ByteEnabledDwordWrite) {
        match CxlFlexBusPortDvsecRegisterOffset(offset) {
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS => {
                self.handle_control_status_write(value)
            }
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2 => self.handle_control2_write(value),
            _ => {}
        }
    }

    fn reset_state(&mut self) {
        self.capability = self.reset_baseline_capability;
        self.control =
            CxlFlexBusPortDvsecControl::new().with_io_enable(self.capability.io_capable());
        self.status = CxlFlexBusPortDvsecStatus::new();
        self.received_modified_ts_data_phase1 =
            CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1::new();
        self.capability2 = self.reset_baseline_capability2;
        self.control2 = CxlFlexBusPortDvsecControl2::new();
        self.status2 = CxlFlexBusPortDvsecStatus2::new();
    }

    fn dvsec_header1() -> DvsecHeader1 {
        DvsecHeader1::new()
            .with_dvsec_vendor_id(CXL_DVSEC_VENDOR_ID)
            .with_dvsec_revision(CXL_FLEX_BUS_PORT_DVSEC_REVISION)
            .with_dvsec_length(CXL_FLEX_BUS_PORT_DVSEC_LENGTH)
    }

    fn dvsec_header2() -> DvsecHeader2 {
        DvsecHeader2::new().with_dvsec_id(CXL_FLEX_BUS_PORT_DVSEC_ID)
    }

    fn handle_control_status_write(&mut self, value: ByteEnabledDwordWrite) {
        let requested = value.merge_low(self.control.into_bits())
            & CXL_FLEX_BUS_PORT_DVSEC_CONTROL_WRITABLE_MASK;
        let mut next = CxlFlexBusPortDvsecControl::from_bits(requested);

        // IO Enable is RO and tied to IO capability.
        next = next.with_io_enable(self.capability.io_capable());

        // Writable bits are still capability-gated.
        if !self.capability.cache_capable() {
            next = next.with_cache_enable(false);
        }
        if !self.capability.mem_capable() {
            next = next.with_mem_enable(false);
        }
        if !self.capability.cxl_68b_flit_and_vh_capable() {
            next = next.with_cxl_68b_flit_and_vh_enable(false);
        }
        if !self.capability.cxl_multi_logical_device_capable() {
            next = next.with_cxl_multi_logical_device_enable(false);
        }
        if !self.capability.cxl_latency_optimized_256b_flit_capable() {
            next = next.with_cxl_latency_optimized_256b_flit_enable(false);
        }
        if !self.capability.cxl_pbr_flit_capable() {
            next = next.with_cxl_pbr_flit_enable(false);
        }

        // HwInit-only bits remain unchanged.
        next = next
            .with_cxl_sync_hdr_bypass_enable(self.control.cxl_sync_hdr_bypass_enable())
            .with_drift_buffer_enable(self.control.drift_buffer_enable());

        self.control = next;

        let clear_mask = value.extract_high() & CXL_FLEX_BUS_PORT_DVSEC_STATUS_RW1CS_MASK;
        if clear_mask != 0 {
            let next_bits = self.status.into_bits() & !clear_mask;
            self.status = CxlFlexBusPortDvsecStatus::from_bits(next_bits);
        }
    }

    fn handle_control2_write(&mut self, value: ByteEnabledDwordWrite) {
        let value = value.merge(self.control2.into_bits());
        let requested = value & CXL_FLEX_BUS_PORT_DVSEC_CONTROL2_WRITABLE_MASK;
        let mut next = CxlFlexBusPortDvsecControl2::from_bits(requested);
        if !self.capability2.nop_hint_capable() {
            next = next.with_nop_hint_enable(false);
        }
        self.control2 = next;
    }
}

impl PciExtendedCapability for CxlFlexBusPortDvsecExtendedCapability {
    fn label(&self) -> &str {
        "flex_bus_port_dvsec"
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
        #[mesh(package = "cxl.pci_registers.flex_bus_port_dvsec")]
        pub struct SavedState {
            #[mesh(1)]
            pub capability: u32,
            #[mesh(2)]
            pub control: u32,
            #[mesh(3)]
            pub status: u32,
            #[mesh(4)]
            pub received_modified_ts_data_phase1: u32,
            #[mesh(5)]
            pub capability2: u32,
            #[mesh(6)]
            pub control2: u32,
            #[mesh(7)]
            pub status2: u32,
            #[mesh(8)]
            pub reset_baseline_capability: u32,
            #[mesh(9)]
            pub reset_baseline_capability2: u32,
        }
    }

    impl SaveRestore for CxlFlexBusPortDvsecExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                capability: u32::from(self.capability.into_bits()),
                control: u32::from(self.control.into_bits()),
                status: u32::from(self.status.into_bits()),
                received_modified_ts_data_phase1: self.received_modified_ts_data_phase1.into_bits(),
                capability2: self.capability2.into_bits(),
                control2: self.control2.into_bits(),
                status2: self.status2.into_bits(),
                reset_baseline_capability: u32::from(self.reset_baseline_capability.into_bits()),
                reset_baseline_capability2: self.reset_baseline_capability2.into_bits(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.capability = CxlFlexBusPortDvsecCapability::from_bits(state.capability as u16);
            self.control = CxlFlexBusPortDvsecControl::from_bits(state.control as u16);
            self.status = CxlFlexBusPortDvsecStatus::from_bits(state.status as u16);
            self.received_modified_ts_data_phase1 =
                CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1::from_bits(
                    state.received_modified_ts_data_phase1,
                );
            self.capability2 = CxlFlexBusPortDvsecCapability2::from_bits(state.capability2);
            self.control2 = CxlFlexBusPortDvsecControl2::from_bits(state.control2);
            self.status2 = CxlFlexBusPortDvsecStatus2::from_bits(state.status2);
            self.reset_baseline_capability =
                CxlFlexBusPortDvsecCapability::from_bits(state.reset_baseline_capability as u16);
            self.reset_baseline_capability2 =
                CxlFlexBusPortDvsecCapability2::from_bits(state.reset_baseline_capability2);
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

    use super::CxlFlexBusPortDvsecCapability;
    use super::CxlFlexBusPortDvsecControl;
    use super::CxlFlexBusPortDvsecControl2;
    use super::CxlFlexBusPortDvsecExtendedCapability;
    use super::CxlFlexBusPortDvsecRegisterOffset;
    use super::CxlFlexBusPortDvsecStatus;

    #[test]
    fn header_registers_match_required_constants() {
        let cap = CxlFlexBusPortDvsecExtendedCapability::new();

        assert_eq!(
            read_extended_cap_u32(&cap, DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0),
            0x0203_1e98
        );
        assert_eq!(
            read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_HEADER2_CAPABILITY.0
            ) & 0xffff,
            0x0007
        );
    }

    #[test]
    fn label_is_flex_bus_port_dvsec() {
        let cap = CxlFlexBusPortDvsecExtendedCapability::new();
        assert_eq!(cap.label(), "flex_bus_port_dvsec");
    }

    #[test]
    fn control_write_is_capability_gated() {
        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new().with_mem_capable(true);

        let requested = CxlFlexBusPortDvsecControl::new()
            .with_cache_enable(true)
            .with_mem_enable(true)
            .with_cxl_68b_flit_and_vh_enable(true)
            .with_cxl_multi_logical_device_enable(true)
            .with_cxl_latency_optimized_256b_flit_enable(true)
            .with_cxl_pbr_flit_enable(true)
            .into_bits();

        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            u32::from(requested),
        );

        let control = CxlFlexBusPortDvsecControl::from_bits(read_extended_cap_u32(
            &cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
        ) as u16);

        assert!(!control.cache_enable());
        assert!(control.mem_enable());
        assert!(!control.cxl_68b_flit_and_vh_enable());
        assert!(!control.cxl_multi_logical_device_enable());
        assert!(!control.cxl_latency_optimized_256b_flit_enable());
        assert!(!control.cxl_pbr_flit_enable());
        assert!(control.io_enable());
    }

    #[test]
    fn reset_preserves_configured_mem_capability() {
        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new().with_mem_capable(true);

        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            u32::from(
                CxlFlexBusPortDvsecControl::new()
                    .with_mem_enable(true)
                    .into_bits(),
            ),
        );

        cap.reset();

        let header2_cap = read_extended_cap_u32(
            &cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_HEADER2_CAPABILITY.0,
        );
        let capability = CxlFlexBusPortDvsecCapability::from_bits((header2_cap >> 16) as u16);
        let control = CxlFlexBusPortDvsecControl::from_bits(read_extended_cap_u32(
            &cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
        ) as u16);

        assert!(capability.mem_capable());
        assert!(!control.mem_enable());
        assert!(control.io_enable());
    }

    #[test]
    fn status_rw1cs_clears_target_bits() {
        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new();
        let status = CxlFlexBusPortDvsecStatus::new()
            .with_even_half_failed(true)
            .with_cxl_correctable_protocol_id_framing_error(true)
            .with_cxl_unexpected_protocol_id_dropped(true);
        cap.set_status(status, 0, cap.status2);

        // clear bits 7 and 10
        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            (1u32 << (16 + 7)) | (1u32 << (16 + 10)),
        );

        let next = CxlFlexBusPortDvsecStatus::from_bits(
            (read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            ) >> 16) as u16,
        );
        assert!(!next.even_half_failed());
        assert!(next.cxl_correctable_protocol_id_framing_error());
        assert!(!next.cxl_unexpected_protocol_id_dropped());
    }

    #[test]
    fn status_rw1cs_ignores_disabled_byte_lanes() {
        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new();
        let status = CxlFlexBusPortDvsecStatus::new().with_even_half_failed(true);
        cap.set_status(status, 0, cap.status2);

        cap.write(
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            ByteEnabledDwordWrite::new(0xffff_ffff, PciConfigByteEnable::new(0b0011).unwrap()),
        );

        let next = CxlFlexBusPortDvsecStatus::from_bits(
            (read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            ) >> 16) as u16,
        );
        assert!(next.even_half_failed());
    }

    #[test]
    fn control2_is_gated_by_capability2() {
        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new();
        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0,
            CxlFlexBusPortDvsecControl2::new()
                .with_nop_hint_enable(true)
                .into_bits(),
        );
        let control2 = CxlFlexBusPortDvsecControl2::from_bits(read_extended_cap_u32(
            &cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0,
        ));
        assert!(!control2.nop_hint_enable());

        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new().with_capability2(true, false);
        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0,
            CxlFlexBusPortDvsecControl2::new()
                .with_nop_hint_enable(true)
                .into_bits(),
        );
        let control2 = CxlFlexBusPortDvsecControl2::from_bits(read_extended_cap_u32(
            &cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0,
        ));
        assert!(control2.nop_hint_enable());
    }

    #[test]
    fn save_restore_round_trips_state() {
        let mut cap = CxlFlexBusPortDvsecExtendedCapability::new()
            .with_mem_capable(true)
            .with_optional_capabilities(true, true, true, true, true)
            .with_capability2(true, true);
        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0,
            u32::from(
                CxlFlexBusPortDvsecControl::new()
                    .with_cache_enable(true)
                    .with_mem_enable(true)
                    .with_cxl_68b_flit_and_vh_enable(true)
                    .with_cxl_multi_logical_device_enable(true)
                    .with_cxl_latency_optimized_256b_flit_enable(true)
                    .with_cxl_pbr_flit_enable(true)
                    .into_bits(),
            ),
        );
        cap.set_status(
            CxlFlexBusPortDvsecStatus::new().with_cxl_retimers_present_mismatch(true),
            0x00ab_cdef,
            super::CxlFlexBusPortDvsecStatus2::new()
                .with_nop_hint_info(0x2)
                .with_streamlined_port(true),
        );
        write_extended_cap_u32(
            &mut cap,
            CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0,
            CxlFlexBusPortDvsecControl2::new()
                .with_nop_hint_enable(true)
                .into_bits(),
        );

        let saved = cap.save().expect("save should succeed");
        let mut restored = CxlFlexBusPortDvsecExtendedCapability::new();
        restored.restore(saved).expect("restore should succeed");

        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_HEADER2_CAPABILITY.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_HEADER2_CAPABILITY.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL_STATUS.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_RECEIVED_MODIFIED_TS_DATA_PHASE1.0
            ),
            read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_RECEIVED_MODIFIED_TS_DATA_PHASE1.0
            )
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_CAPABILITY2.0
            ),
            read_extended_cap_u32(&cap, CxlFlexBusPortDvsecRegisterOffset::DVSEC_CAPABILITY2.0)
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0
            ),
            read_extended_cap_u32(&cap, CxlFlexBusPortDvsecRegisterOffset::DVSEC_CONTROL2.0)
        );
        assert_eq!(
            read_extended_cap_u32(
                &restored,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_STATUS2.0
            ),
            read_extended_cap_u32(&cap, CxlFlexBusPortDvsecRegisterOffset::DVSEC_STATUS2.0)
        );

        let capability = CxlFlexBusPortDvsecCapability::from_bits(
            (read_extended_cap_u32(
                &cap,
                CxlFlexBusPortDvsecRegisterOffset::DVSEC_HEADER2_CAPABILITY.0,
            ) >> 16) as u16,
        );
        assert!(capability.cache_capable());
        assert!(capability.cxl_68b_flit_and_vh_capable());
        assert!(capability.cxl_multi_logical_device_capable());
    }
}
