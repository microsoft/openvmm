// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe Advanced Error Reporting (AER) extended capability.

use super::PciExtendedCapability;
use crate::spec::caps::ExtendedCapabilityId;
use crate::spec::caps::aer as aer_spec;
use crate::spec::caps::aer::AerExtendedCapabilityHeader;
use crate::spec::caps::pci_express::DevicePortType;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use inspect::Inspect;

#[derive(Debug, Clone, Copy, Inspect)]
/// PCIe function/port class that determines AER register behavior.
pub enum AerPortType {
    /// Endpoint Function.
    Endpoint,
    /// Root Port.
    RootPort,
    /// Upstream Switch Port.
    UpstreamSwitchPort,
    /// Downstream Switch Port.
    DownstreamSwitchPort,
}

#[derive(Debug, Clone, Copy, Default, Inspect)]
/// Optional AER register defaults used at capability construction time.
pub struct AerCapabilityConfig {
    /// Override for the Correctable Error Mask register default.
    pub correctable_mask: Option<u32>,
    /// Override for the Uncorrectable Error Mask register default.
    pub uncorrectable_mask: Option<u32>,
    /// Override for the Uncorrectable Error Severity register default.
    pub uncorrectable_severity_mask: Option<u32>,
}

impl From<DevicePortType> for AerPortType {
    fn from(value: DevicePortType) -> Self {
        match value {
            DevicePortType::Endpoint => Self::Endpoint,
            DevicePortType::RootPort => Self::RootPort,
            DevicePortType::UpstreamSwitchPort => Self::UpstreamSwitchPort,
            DevicePortType::DownstreamSwitchPort => Self::DownstreamSwitchPort,
        }
    }
}

impl From<&DevicePortType> for AerPortType {
    fn from(value: &DevicePortType) -> Self {
        match value {
            DevicePortType::Endpoint => Self::Endpoint,
            DevicePortType::RootPort => Self::RootPort,
            DevicePortType::UpstreamSwitchPort => Self::UpstreamSwitchPort,
            DevicePortType::DownstreamSwitchPort => Self::DownstreamSwitchPort,
        }
    }
}

#[derive(Debug, Inspect)]
struct AerAdvancedCapabilities {
    ecrc_generation_capable: bool,
    ecrc_check_capable: bool,
    multiple_header_recording_capable: bool,
    tlp_prefix_log_present: bool,
    completion_timeout_prefix_header_log_capable: bool,
    header_log_size_dw: u8,
}

impl AerAdvancedCapabilities {
    fn new(port_type: AerPortType) -> Self {
        // Keep defaults conservative: no ECRC engines, no multi-header logging,
        // no End-End TLP Prefix logging capability advertised.
        let _ = port_type;
        Self {
            ecrc_generation_capable: false,
            ecrc_check_capable: false,
            multiple_header_recording_capable: false,
            tlp_prefix_log_present: false,
            completion_timeout_prefix_header_log_capable: false,
            // 4 DW header log (DW1-4) in non-Flit structure.
            header_log_size_dw: 4,
        }
    }
}

#[derive(Debug, Inspect)]
/// PCIe Advanced Error Reporting (AER) extended capability emulator.
pub struct AerExtendedCapability {
    port_type: AerPortType,
    unc_err_status: aer_spec::UncorrectableErrorStatus,
    unc_err_mask: aer_spec::UncorrectableErrorMask,
    unc_err_severity: aer_spec::UncorrectableErrorSeverity,
    cor_err_status: aer_spec::CorrectableErrorStatus,
    cor_err_mask: aer_spec::CorrectableErrorMask,
    aer_cap_ctl: aer_spec::AdvancedErrorCapabilitiesAndControl,
    #[inspect(skip)]
    header_log: [u32; 4],
    root_error_command: aer_spec::RootErrorCommand,
    root_error_status: aer_spec::RootErrorStatus,
    error_source_identification: aer_spec::ErrorSourceIdentification,
    #[inspect(skip)]
    tlp_prefix_log: [u32; 4],
    advanced_capabilities: AerAdvancedCapabilities,
}

impl AerExtendedCapability {
    /// Creates a new AER extended capability for the given PCIe port type.
    pub fn new(port_type: AerPortType) -> Self {
        Self::with_config(port_type, AerCapabilityConfig::default())
    }

    /// Creates a new AER extended capability with explicit register defaults.
    pub fn with_config(port_type: AerPortType, config: AerCapabilityConfig) -> Self {
        let default_correctable_mask = config
            .correctable_mask
            .unwrap_or(aer_spec::DEFAULT_COR_ERR_MASK);
        let default_uncorrectable_mask = config
            .uncorrectable_mask
            .unwrap_or(aer_spec::DEFAULT_UNC_ERR_MASK);
        let default_uncorrectable_severity = config
            .uncorrectable_severity_mask
            .unwrap_or(aer_spec::DEFAULT_UNC_ERR_SEVERITY);

        Self {
            port_type,
            unc_err_status: aer_spec::UncorrectableErrorStatus::new(),
            unc_err_mask: aer_spec::UncorrectableErrorMask::from_bits(default_uncorrectable_mask),
            unc_err_severity: aer_spec::UncorrectableErrorSeverity::from_bits(
                default_uncorrectable_severity,
            ),
            cor_err_status: aer_spec::CorrectableErrorStatus::new(),
            cor_err_mask: aer_spec::CorrectableErrorMask::from_bits(default_correctable_mask),
            aer_cap_ctl: aer_spec::AdvancedErrorCapabilitiesAndControl::new(),
            header_log: [0; 4],
            root_error_command: aer_spec::RootErrorCommand::new(),
            root_error_status: aer_spec::RootErrorStatus::new(),
            error_source_identification: aer_spec::ErrorSourceIdentification::new(),
            tlp_prefix_log: [0; 4],
            advanced_capabilities: AerAdvancedCapabilities::new(port_type),
        }
    }

    fn supports_root_registers(&self) -> bool {
        matches!(self.port_type, AerPortType::RootPort)
    }

    fn advanced_capabilities_and_control(&self) -> u32 {
        let mut v = self.aer_cap_ctl;

        v.set_first_error_pointer(0);
        v.set_logged_tlp_was_flit_mode(false);
        v.set_logged_tlp_size(0);

        v.set_ecrc_generation_capable(self.advanced_capabilities.ecrc_generation_capable);
        v.set_ecrc_check_capable(self.advanced_capabilities.ecrc_check_capable);
        v.set_multiple_header_recording_capable(
            self.advanced_capabilities.multiple_header_recording_capable,
        );
        v.set_tlp_prefix_log_present(self.advanced_capabilities.tlp_prefix_log_present);
        v.set_completion_timeout_prefix_header_log_capable(
            self.advanced_capabilities
                .completion_timeout_prefix_header_log_capable,
        );
        v.set_header_log_size(self.advanced_capabilities.header_log_size_dw & 0x1f);

        v.into_bits()
    }

    fn writable_advanced_enable_mask(&self) -> u32 {
        let mut mask = 0u32;
        if self.advanced_capabilities.ecrc_generation_capable {
            mask |= aer_spec::AER_CAP_CTL_ECRC_GEN_ENABLE_BIT;
        }
        if self.advanced_capabilities.ecrc_check_capable {
            mask |= aer_spec::AER_CAP_CTL_ECRC_CHK_ENABLE_BIT;
        }
        if self.advanced_capabilities.multiple_header_recording_capable {
            mask |= aer_spec::AER_CAP_CTL_MULTI_HEADER_RECORDING_ENABLE_BIT;
        }
        mask
    }
}

impl PciExtendedCapability for AerExtendedCapability {
    fn label(&self) -> &str {
        "aer"
    }

    fn extended_capability_id(&self) -> u16 {
        ExtendedCapabilityId::AER.0
    }

    fn capability_version(&self) -> u8 {
        2
    }

    fn len(&self) -> usize {
        0x48
    }

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        let v = match AerExtendedCapabilityHeader(offset) {
            AerExtendedCapabilityHeader::HEADER => {
                u32::from(self.extended_capability_id())
                    | (u32::from(self.capability_version()) << 16)
            }
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS => {
                self.unc_err_status.into_bits()
            }
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK => self.unc_err_mask.into_bits(),
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_SEVERITY => {
                self.unc_err_severity.into_bits()
            }
            AerExtendedCapabilityHeader::CORRECTABLE_ERROR_STATUS => {
                self.cor_err_status.into_bits()
            }
            AerExtendedCapabilityHeader::CORRECTABLE_ERROR_MASK => self.cor_err_mask.into_bits(),
            AerExtendedCapabilityHeader::ADVANCED_ERROR_CAPABILITIES_AND_CONTROL => {
                self.advanced_capabilities_and_control()
            }
            AerExtendedCapabilityHeader::HEADER_LOG_0 => self.header_log[0],
            AerExtendedCapabilityHeader::HEADER_LOG_1 => self.header_log[1],
            AerExtendedCapabilityHeader::HEADER_LOG_2 => self.header_log[2],
            AerExtendedCapabilityHeader::HEADER_LOG_3 => self.header_log[3],
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND => {
                if self.supports_root_registers() {
                    self.root_error_command.into_bits()
                } else {
                    0
                }
            }
            AerExtendedCapabilityHeader::ROOT_ERROR_STATUS => {
                if self.supports_root_registers() {
                    self.root_error_status.into_bits()
                } else {
                    0
                }
            }
            AerExtendedCapabilityHeader::ERROR_SOURCE_IDENTIFICATION => {
                if self.supports_root_registers() {
                    self.error_source_identification.into_bits()
                } else {
                    0
                }
            }
            AerExtendedCapabilityHeader::TLP_PREFIX_LOG_0 => self.tlp_prefix_log[0],
            AerExtendedCapabilityHeader::TLP_PREFIX_LOG_1 => self.tlp_prefix_log[1],
            AerExtendedCapabilityHeader::TLP_PREFIX_LOG_2 => self.tlp_prefix_log[2],
            AerExtendedCapabilityHeader::TLP_PREFIX_LOG_3 => self.tlp_prefix_log[3],
            _ => !0,
        };

        value.set(v);
    }

    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite) {
        match AerExtendedCapabilityHeader(offset) {
            AerExtendedCapabilityHeader::HEADER => {
                tracelimit::warn_ratelimited!(
                    offset,
                    ?val,
                    "write to read-only AER header register"
                );
            }
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS => {
                let clear_mask =
                    val.merge(self.unc_err_status.into_bits()) & aer_spec::UNC_ERR_STATUS_RW1C_MASK;
                self.unc_err_status = aer_spec::UncorrectableErrorStatus::from_bits(
                    self.unc_err_status.into_bits() & !clear_mask,
                );
            }
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK => {
                self.unc_err_mask = aer_spec::UncorrectableErrorMask::from_bits(
                    val.merge(self.unc_err_mask.into_bits()),
                );
            }
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_SEVERITY => {
                self.unc_err_severity = aer_spec::UncorrectableErrorSeverity::from_bits(
                    val.merge(self.unc_err_severity.into_bits()),
                );
            }
            AerExtendedCapabilityHeader::CORRECTABLE_ERROR_STATUS => {
                let clear_mask =
                    val.merge(self.cor_err_status.into_bits()) & aer_spec::COR_ERR_STATUS_RW1C_MASK;
                self.cor_err_status = aer_spec::CorrectableErrorStatus::from_bits(
                    self.cor_err_status.into_bits() & !clear_mask,
                );
            }
            AerExtendedCapabilityHeader::CORRECTABLE_ERROR_MASK => {
                self.cor_err_mask = aer_spec::CorrectableErrorMask::from_bits(
                    val.merge(self.cor_err_mask.into_bits()),
                );
            }
            AerExtendedCapabilityHeader::ADVANCED_ERROR_CAPABILITIES_AND_CONTROL => {
                let merged = val.merge(self.aer_cap_ctl.into_bits());
                let new_enable_bits = merged
                    & aer_spec::AER_CAP_CTL_WRITABLE_MASK
                    & self.writable_advanced_enable_mask();
                self.aer_cap_ctl = aer_spec::AdvancedErrorCapabilitiesAndControl::from_bits(
                    (self.aer_cap_ctl.into_bits() & !aer_spec::AER_CAP_CTL_WRITABLE_MASK)
                        | new_enable_bits,
                );
            }
            AerExtendedCapabilityHeader::HEADER_LOG_0
            | AerExtendedCapabilityHeader::HEADER_LOG_1
            | AerExtendedCapabilityHeader::HEADER_LOG_2
            | AerExtendedCapabilityHeader::HEADER_LOG_3
            | AerExtendedCapabilityHeader::ERROR_SOURCE_IDENTIFICATION
            | AerExtendedCapabilityHeader::TLP_PREFIX_LOG_0
            | AerExtendedCapabilityHeader::TLP_PREFIX_LOG_1
            | AerExtendedCapabilityHeader::TLP_PREFIX_LOG_2
            | AerExtendedCapabilityHeader::TLP_PREFIX_LOG_3 => {
                tracelimit::warn_ratelimited!(offset, ?val, "write to read-only AER register");
            }
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND => {
                if self.supports_root_registers() {
                    self.root_error_command = aer_spec::RootErrorCommand::from_bits(
                        val.merge(self.root_error_command.into_bits())
                            & aer_spec::ROOT_ERR_COMMAND_RW_MASK,
                    );
                }
            }
            AerExtendedCapabilityHeader::ROOT_ERROR_STATUS => {
                if self.supports_root_registers() {
                    let clear_mask = val.merge(self.root_error_status.into_bits())
                        & aer_spec::ROOT_ERR_STATUS_RW1C_MASK;
                    self.root_error_status = aer_spec::RootErrorStatus::from_bits(
                        self.root_error_status.into_bits() & !clear_mask,
                    );
                }
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, ?val, "unexpected AER write");
            }
        }
    }

    fn reset(&mut self) {
        self.unc_err_status = aer_spec::UncorrectableErrorStatus::new();
        self.unc_err_mask =
            aer_spec::UncorrectableErrorMask::from_bits(aer_spec::DEFAULT_UNC_ERR_MASK);
        self.unc_err_severity =
            aer_spec::UncorrectableErrorSeverity::from_bits(aer_spec::DEFAULT_UNC_ERR_SEVERITY);
        self.cor_err_status = aer_spec::CorrectableErrorStatus::new();
        self.cor_err_mask =
            aer_spec::CorrectableErrorMask::from_bits(aer_spec::DEFAULT_COR_ERR_MASK);
        self.aer_cap_ctl = aer_spec::AdvancedErrorCapabilitiesAndControl::new();
        self.header_log = [0; 4];
        self.root_error_command = aer_spec::RootErrorCommand::new();
        self.root_error_status = aer_spec::RootErrorStatus::new();
        self.error_source_identification = aer_spec::ErrorSourceIdentification::new();
        self.tlp_prefix_log = [0; 4];
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
        #[mesh(package = "pci.capabilities.extended.aer")]
        pub struct SavedState {
            #[mesh(1)]
            pub unc_err_status: u32,
            #[mesh(2)]
            pub unc_err_mask: u32,
            #[mesh(3)]
            pub unc_err_severity: u32,
            #[mesh(4)]
            pub cor_err_status: u32,
            #[mesh(5)]
            pub cor_err_mask: u32,
            #[mesh(6)]
            pub aer_cap_ctl_enable_bits: u32,
            #[mesh(7)]
            pub header_log: [u32; 4],
            #[mesh(8)]
            pub root_error_command: u32,
            #[mesh(9)]
            pub root_error_status: u32,
            #[mesh(10)]
            pub error_source_identification: u32,
            #[mesh(11)]
            pub tlp_prefix_log: [u32; 4],
        }
    }

    impl SaveRestore for AerExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                unc_err_status: self.unc_err_status.into_bits(),
                unc_err_mask: self.unc_err_mask.into_bits(),
                unc_err_severity: self.unc_err_severity.into_bits(),
                cor_err_status: self.cor_err_status.into_bits(),
                cor_err_mask: self.cor_err_mask.into_bits(),
                aer_cap_ctl_enable_bits: self.aer_cap_ctl.into_bits()
                    & aer_spec::AER_CAP_CTL_WRITABLE_MASK,
                header_log: self.header_log,
                root_error_command: self.root_error_command.into_bits(),
                root_error_status: self.root_error_status.into_bits(),
                error_source_identification: self.error_source_identification.into_bits(),
                tlp_prefix_log: self.tlp_prefix_log,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.unc_err_status =
                aer_spec::UncorrectableErrorStatus::from_bits(state.unc_err_status);
            self.unc_err_mask = aer_spec::UncorrectableErrorMask::from_bits(state.unc_err_mask);
            self.unc_err_severity =
                aer_spec::UncorrectableErrorSeverity::from_bits(state.unc_err_severity);
            self.cor_err_status = aer_spec::CorrectableErrorStatus::from_bits(state.cor_err_status);
            self.cor_err_mask = aer_spec::CorrectableErrorMask::from_bits(state.cor_err_mask);
            let enabled = state.aer_cap_ctl_enable_bits
                & aer_spec::AER_CAP_CTL_WRITABLE_MASK
                & self.writable_advanced_enable_mask();
            self.aer_cap_ctl = aer_spec::AdvancedErrorCapabilitiesAndControl::from_bits(enabled);
            self.header_log = state.header_log;
            self.root_error_command = aer_spec::RootErrorCommand::from_bits(
                state.root_error_command & aer_spec::ROOT_ERR_COMMAND_RW_MASK,
            );
            self.root_error_status = aer_spec::RootErrorStatus::from_bits(state.root_error_status);
            self.error_source_identification =
                aer_spec::ErrorSourceIdentification::from_bits(state.error_source_identification);
            self.tlp_prefix_log = state.tlp_prefix_log;
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
    use vmcore::save_restore::SaveRestore;

    #[test]
    fn test_aer_defaults_and_header_contract() {
        let cap = AerExtendedCapability::new(AerPortType::Endpoint);

        assert_eq!(cap.label(), "aer");
        assert_eq!(cap.extended_capability_id(), ExtendedCapabilityId::AER.0);
        assert_eq!(cap.capability_version(), 2);
        assert_eq!(cap.len(), 0x48);
        assert_extended_header_contract(&cap);

        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK.0
            ),
            aer_spec::DEFAULT_UNC_ERR_MASK
        );
        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_SEVERITY.0
            ),
            aer_spec::DEFAULT_UNC_ERR_SEVERITY
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::CORRECTABLE_ERROR_MASK.0),
            aer_spec::DEFAULT_COR_ERR_MASK
        );
    }

    #[test]
    fn test_aer_endpoint_drops_root_register_writes() {
        let mut cap = AerExtendedCapability::new(AerPortType::Endpoint);

        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0,
            0x7,
        );
        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ROOT_ERROR_STATUS.0,
            0xffff_ffff,
        );

        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0),
            0
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_STATUS.0),
            0
        );
    }

    #[test]
    fn test_aer_root_port_root_command_writable() {
        let mut cap = AerExtendedCapability::new(AerPortType::RootPort);

        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0,
            0xffff_ffff,
        );

        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0),
            aer_spec::ROOT_ERR_COMMAND_RW_MASK
        );
    }

    #[test]
    fn test_aer_advanced_cap_control_masks_unsupported_bits() {
        let mut cap = AerExtendedCapability::new(AerPortType::RootPort);

        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ADVANCED_ERROR_CAPABILITIES_AND_CONTROL.0,
            0xffff_ffff,
        );

        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::ADVANCED_ERROR_CAPABILITIES_AND_CONTROL.0
            ) & aer_spec::AER_CAP_CTL_WRITABLE_MASK,
            0
        );
    }

    #[test]
    fn test_aer_reset_restores_defaults() {
        let mut cap = AerExtendedCapability::new(AerPortType::RootPort);

        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK.0,
            0,
        );
        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::CORRECTABLE_ERROR_MASK.0,
            0,
        );
        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0,
            0x7,
        );

        cap.reset();

        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK.0
            ),
            aer_spec::DEFAULT_UNC_ERR_MASK
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::CORRECTABLE_ERROR_MASK.0),
            aer_spec::DEFAULT_COR_ERR_MASK
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0),
            0
        );
    }

    #[test]
    fn test_aer_save_restore_roundtrip() {
        let mut cap = AerExtendedCapability::new(AerPortType::RootPort);

        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK.0,
            0x0000_1234,
        );
        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0,
            0x7,
        );

        let saved = cap.save().expect("save should succeed");

        cap.reset();
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0),
            0
        );

        cap.restore(saved).expect("restore should succeed");

        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK.0
            ),
            0x0000_1234
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0),
            0x7
        );
    }

    #[test]
    fn test_aer_switch_port_shape_uses_non_root_behavior() {
        let mut cap = AerExtendedCapability::new(AerPortType::DownstreamSwitchPort);

        write_extended_cap_u32(
            &mut cap,
            AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0,
            0x7,
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0),
            0
        );
    }

    #[test]
    fn test_aer_with_config_overrides_default_masks() {
        let cap = AerExtendedCapability::with_config(
            AerPortType::RootPort,
            AerCapabilityConfig {
                correctable_mask: Some(0x0000_0021),
                uncorrectable_mask: Some(0x0400_0000),
                uncorrectable_severity_mask: Some(0x0001_3000),
            },
        );

        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_MASK.0
            ),
            0x0400_0000
        );
        assert_eq!(
            read_extended_cap_u32(&cap, AerExtendedCapabilityHeader::CORRECTABLE_ERROR_MASK.0),
            0x0000_0021
        );
        assert_eq!(
            read_extended_cap_u32(
                &cap,
                AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_SEVERITY.0
            ),
            0x0001_3000
        );
    }
}
