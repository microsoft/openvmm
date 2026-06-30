// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe Downstream Port Containment (DPC) extended capability.

#![expect(missing_docs)]

use super::PciExtendedCapability;
use crate::spec::caps::ExtendedCapabilityId;
use crate::spec::caps::dpc as dpc_spec;
use crate::spec::caps::dpc::DpcExtendedCapabilityHeader;
use crate::spec::caps::pci_express::DevicePortType;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use inspect::Inspect;

#[derive(Debug, Clone, Copy, Inspect)]
pub struct DpcCapabilityConfig {
    pub dpc_interrupt_message_number: Option<u8>,
    pub rp_extensions_for_dpc: bool,
    pub poisoned_tlp_egress_blocking_supported: bool,
    pub dpc_software_triggering_supported: bool,
    pub dl_active_err_cor_signaling_supported: bool,
    pub rp_pio_log_size_dw: u8,
}

impl Default for DpcCapabilityConfig {
    fn default() -> Self {
        Self {
            dpc_interrupt_message_number: None,
            rp_extensions_for_dpc: false,
            poisoned_tlp_egress_blocking_supported: false,
            dpc_software_triggering_supported: true,
            dl_active_err_cor_signaling_supported: false,
            rp_pio_log_size_dw: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DpcTriggerOutcome {
    pub containment_entered: bool,
    pub should_interrupt: bool,
}

#[derive(Debug, Inspect)]
pub struct DpcExtendedCapability {
    capability: dpc_spec::DpcCapability,
    control: dpc_spec::DpcControl,
    status: dpc_spec::DpcStatus,
    error_source_id: dpc_spec::DpcErrorSourceId,
    rp_pio_status: u32,
    rp_pio_mask: u32,
    rp_pio_severity: u32,
    rp_pio_syserror: u32,
    rp_pio_exception: u32,
    #[inspect(skip)]
    rp_pio_header_log: [u32; 4],
    rp_pio_impspec_log: u32,
    #[inspect(skip)]
    rp_pio_tlp_prefix_log: [u32; 4],
}

impl DpcExtendedCapability {
    pub fn new(port_type: &DevicePortType) -> Self {
        let mut config = DpcCapabilityConfig::default();
        if matches!(port_type, DevicePortType::RootPort) {
            config.rp_extensions_for_dpc = true;
        }
        Self::with_config(port_type, config)
    }

    pub fn with_config(port_type: &DevicePortType, config: DpcCapabilityConfig) -> Self {
        let mut capability = dpc_spec::DpcCapability::new()
            .with_dpc_interrupt_message_number(
                config.dpc_interrupt_message_number.unwrap_or(0) & 0x1f,
            )
            .with_rp_extensions_for_dpc(config.rp_extensions_for_dpc)
            .with_poisoned_tlp_egress_blocking_supported(
                config.poisoned_tlp_egress_blocking_supported,
            )
            .with_dpc_software_triggering_supported(config.dpc_software_triggering_supported)
            .with_dl_active_err_cor_signaling_supported(
                config.dl_active_err_cor_signaling_supported,
            )
            .with_rp_pio_log_size_3_0(config.rp_pio_log_size_dw & 0x0f)
            .with_rp_pio_log_size_4((config.rp_pio_log_size_dw & 0x10) != 0);

        if !matches!(port_type, DevicePortType::RootPort) {
            capability.set_rp_extensions_for_dpc(false);
            capability.set_rp_pio_log_size_3_0(0);
            capability.set_rp_pio_log_size_4(false);
        }

        Self {
            capability,
            control: dpc_spec::DpcControl::new(),
            status: dpc_spec::DpcStatus::new().with_rp_pio_first_error_pointer(0x1f),
            error_source_id: dpc_spec::DpcErrorSourceId::new(),
            rp_pio_status: 0,
            rp_pio_mask: 0,
            rp_pio_severity: 0,
            rp_pio_syserror: 0,
            rp_pio_exception: 0,
            rp_pio_header_log: [0; 4],
            rp_pio_impspec_log: 0,
            rp_pio_tlp_prefix_log: [0; 4],
        }
    }

    pub fn interrupt_message_number(&self) -> u8 {
        self.capability.dpc_interrupt_message_number()
    }

    pub fn containment_active(&self) -> bool {
        self.status.dpc_trigger_status()
    }

    pub fn trigger_from_uncorrectable(&mut self, source_id: u16) -> DpcTriggerOutcome {
        self.trigger(
            dpc_spec::DPC_TRIGGER_REASON_UNMASKED_UNCORRECTABLE,
            0,
            source_id,
        )
    }

    /// Phase 1 of synthetic DPC handling: enter containment for an
    /// uncorrectable error and mark RP as busy until software completes
    /// recovery.
    pub fn trigger_from_uncorrectable_begin(&mut self, source_id: u16) -> DpcTriggerOutcome {
        let outcome = self.trigger_from_uncorrectable(source_id);
        self.status.set_dpc_rp_busy(true);
        outcome
    }

    /// Phase 2 of synthetic DPC handling: clear RP busy after recovery.
    pub fn clear_rp_busy(&mut self) {
        self.status.set_dpc_rp_busy(false);
    }

    fn trigger(&mut self, reason: u8, reason_extension: u8, source_id: u16) -> DpcTriggerOutcome {
        let was_contained = self.status.dpc_trigger_status();
        if !was_contained {
            self.status.set_dpc_trigger_status(true);
            self.status.set_dpc_trigger_reason(reason & 0x3);
            self.status
                .set_dpc_trigger_reason_extension(reason_extension & 0x3);
            self.error_source_id.set_dpc_error_source_id(source_id);
        }

        let should_interrupt = self.control.dpc_interrupt_enable();
        if should_interrupt {
            self.status.set_dpc_interrupt_status(true);
        }

        DpcTriggerOutcome {
            containment_entered: !was_contained,
            should_interrupt,
        }
    }

    fn write_control(&mut self, val: ByteEnabledDwordWrite) {
        let mut writable_mask = dpc_spec::DPC_CONTROL_RW_MASK_BASE;
        if !self.capability.poisoned_tlp_egress_blocking_supported() {
            writable_mask &= !dpc_spec::DPC_CONTROL_POISONED_TLP_EGRESS_BLOCKING_ENABLE_BIT;
        }
        if !self.capability.dpc_software_triggering_supported() {
            writable_mask &= !dpc_spec::DPC_CONTROL_SOFTWARE_TRIGGER_BIT;
        }
        if !self.capability.dl_active_err_cor_signaling_supported() {
            writable_mask &= !dpc_spec::DPC_CONTROL_DL_ACTIVE_ERR_COR_ENABLE_BIT;
        }

        let current = self.control.into_bits();
        let merged = val.merge_high(current);
        let next = (current & !writable_mask) | (merged & writable_mask);
        self.control = dpc_spec::DpcControl::from_bits(next);

        if self.capability.dpc_software_triggering_supported()
            && (merged & dpc_spec::DPC_CONTROL_SOFTWARE_TRIGGER_BIT) != 0
            && self.control.dpc_trigger_enable() != 0
        {
            let _ = self.trigger(
                dpc_spec::DPC_TRIGGER_REASON_EXTENSION,
                dpc_spec::DPC_TRIGGER_REASON_EXTENSION_SOFTWARE_TRIGGER,
                0,
            );
        }

        // Software trigger bit always reads as 0.
        self.control.set_dpc_software_trigger(false);
    }

    fn read_status_source_id(&self) -> u32 {
        ((self.status.into_bits() as u32) << 16) | (self.error_source_id.into_bits() as u32)
    }
}

impl PciExtendedCapability for DpcExtendedCapability {
    fn label(&self) -> &str {
        "dpc"
    }

    fn extended_capability_id(&self) -> u16 {
        ExtendedCapabilityId::DPC.0
    }

    fn capability_version(&self) -> u8 {
        1
    }

    fn len(&self) -> usize {
        0x44
    }

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        let v = match DpcExtendedCapabilityHeader(offset) {
            DpcExtendedCapabilityHeader::HEADER => {
                u32::from(self.extended_capability_id())
                    | (u32::from(self.capability_version()) << 16)
            }
            DpcExtendedCapabilityHeader::CAPABILITY_CONTROL => {
                let mut control = self.control;
                control.set_dpc_software_trigger(false);
                ((control.into_bits() as u32) << 16) | (self.capability.into_bits() as u32)
            }
            DpcExtendedCapabilityHeader::STATUS_SOURCE_ID => self.read_status_source_id(),
            DpcExtendedCapabilityHeader::RP_PIO_STATUS => self.rp_pio_status,
            DpcExtendedCapabilityHeader::RP_PIO_MASK => self.rp_pio_mask,
            DpcExtendedCapabilityHeader::RP_PIO_SEVERITY => self.rp_pio_severity,
            DpcExtendedCapabilityHeader::RP_PIO_SYSERROR => self.rp_pio_syserror,
            DpcExtendedCapabilityHeader::RP_PIO_EXCEPTION => self.rp_pio_exception,
            DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_0 => self.rp_pio_header_log[0],
            DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_1 => self.rp_pio_header_log[1],
            DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_2 => self.rp_pio_header_log[2],
            DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_3 => self.rp_pio_header_log[3],
            DpcExtendedCapabilityHeader::RP_PIO_IMPSPEC_LOG => self.rp_pio_impspec_log,
            DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_0 => self.rp_pio_tlp_prefix_log[0],
            DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_1 => self.rp_pio_tlp_prefix_log[1],
            DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_2 => self.rp_pio_tlp_prefix_log[2],
            DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_3 => self.rp_pio_tlp_prefix_log[3],
            _ => !0,
        };

        value.set(v);
    }

    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite) {
        match DpcExtendedCapabilityHeader(offset) {
            DpcExtendedCapabilityHeader::HEADER => {
                tracelimit::warn_ratelimited!(
                    offset,
                    ?val,
                    "write to read-only DPC header register"
                );
            }
            DpcExtendedCapabilityHeader::CAPABILITY_CONTROL => self.write_control(val),
            DpcExtendedCapabilityHeader::STATUS_SOURCE_ID => {
                let merged_status = val.merge_high(self.status.into_bits());
                let clear = merged_status & dpc_spec::DPC_STATUS_RW1C_MASK;
                self.status = dpc_spec::DpcStatus::from_bits(self.status.into_bits() & !clear);
            }
            DpcExtendedCapabilityHeader::RP_PIO_STATUS
            | DpcExtendedCapabilityHeader::RP_PIO_MASK
            | DpcExtendedCapabilityHeader::RP_PIO_SEVERITY
            | DpcExtendedCapabilityHeader::RP_PIO_SYSERROR
            | DpcExtendedCapabilityHeader::RP_PIO_EXCEPTION
            | DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_0
            | DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_1
            | DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_2
            | DpcExtendedCapabilityHeader::RP_PIO_HEADER_LOG_3
            | DpcExtendedCapabilityHeader::RP_PIO_IMPSPEC_LOG
            | DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_0
            | DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_1
            | DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_2
            | DpcExtendedCapabilityHeader::RP_PIO_TLP_PREFIX_LOG_3 => {
                // These are modeled as status/log outputs in this emulator.
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    offset,
                    ?val,
                    "unexpected DPC extended capability write"
                );
            }
        }
    }

    fn reset(&mut self) {
        let rp_pointer = self.status.rp_pio_first_error_pointer();
        self.control = dpc_spec::DpcControl::new();
        self.status = dpc_spec::DpcStatus::new().with_rp_pio_first_error_pointer(rp_pointer);
        self.error_source_id = dpc_spec::DpcErrorSourceId::new();
        self.rp_pio_status = 0;
        self.rp_pio_mask = 0;
        self.rp_pio_severity = 0;
        self.rp_pio_syserror = 0;
        self.rp_pio_exception = 0;
        self.rp_pio_header_log = [0; 4];
        self.rp_pio_impspec_log = 0;
        self.rp_pio_tlp_prefix_log = [0; 4];
    }

    fn as_dpc(&self) -> Option<&DpcExtendedCapability> {
        Some(self)
    }

    fn as_dpc_mut(&mut self) -> Option<&mut DpcExtendedCapability> {
        Some(self)
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

        #[derive(Debug, Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.capabilities.extended.dpc")]
        pub struct SavedState {
            #[mesh(1)]
            pub control: u16,
            #[mesh(2)]
            pub status: u16,
            #[mesh(3)]
            pub error_source_id: u16,
        }
    }

    impl SaveRestore for DpcExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                control: self.control.into_bits(),
                status: self.status.into_bits(),
                error_source_id: self.error_source_id.into_bits(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.control = dpc_spec::DpcControl::from_bits(state.control);
            self.status = dpc_spec::DpcStatus::from_bits(state.status);
            self.error_source_id = dpc_spec::DpcErrorSourceId::from_bits(state.error_source_id);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::extended::assert_extended_header_contract;
    use crate::test_helpers::read_extended_cap_u32;
    use crate::test_helpers::write_extended_cap_u32;

    #[test]
    fn test_dpc_defaults() {
        let cap = DpcExtendedCapability::new(&DevicePortType::RootPort);

        assert_eq!(cap.label(), "dpc");
        assert_eq!(cap.extended_capability_id(), ExtendedCapabilityId::DPC.0);
        assert_eq!(cap.capability_version(), 1);
        assert_eq!(cap.len(), 0x44);
        assert_extended_header_contract(&cap);
        assert!(!cap.containment_active());
    }

    #[test]
    fn test_dpc_software_trigger_sets_containment() {
        let mut cap = DpcExtendedCapability::with_config(
            &DevicePortType::RootPort,
            DpcCapabilityConfig {
                dpc_software_triggering_supported: true,
                ..Default::default()
            },
        );

        let control = dpc_spec::DpcControl::new()
            .with_dpc_trigger_enable(1)
            .with_dpc_software_trigger(true);

        write_extended_cap_u32(
            &mut cap,
            DpcExtendedCapabilityHeader::CAPABILITY_CONTROL.0,
            (control.into_bits() as u32) << 16,
        );

        let status = read_extended_cap_u32(&cap, DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0);
        let status = dpc_spec::DpcStatus::from_bits((status >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(cap.containment_active());
    }

    #[test]
    fn test_dpc_status_rw1c_clears_trigger_status() {
        let mut cap = DpcExtendedCapability::new(&DevicePortType::RootPort);
        let _ = cap.trigger_from_uncorrectable(0x1234);
        assert!(cap.containment_active());

        write_extended_cap_u32(
            &mut cap,
            DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
            (dpc_spec::DpcStatus::new()
                .with_dpc_trigger_status(true)
                .into_bits() as u32)
                << 16,
        );

        assert!(!cap.containment_active());
    }

    #[test]
    fn test_dpc_two_phase_rp_busy() {
        let mut cap = DpcExtendedCapability::new(&DevicePortType::RootPort);

        let _ = cap.trigger_from_uncorrectable_begin(0x1234);
        let status = read_extended_cap_u32(&cap, DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0);
        let status = dpc_spec::DpcStatus::from_bits((status >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(status.dpc_rp_busy());

        cap.clear_rp_busy();
        let status = read_extended_cap_u32(&cap, DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0);
        let status = dpc_spec::DpcStatus::from_bits((status >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(!status.dpc_rp_busy());
    }
}
