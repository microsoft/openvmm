// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe Access Control Services (ACS) extended capability.

use super::PciExtendedCapability;
use crate::spec::caps::ExtendedCapabilityId;
use crate::spec::caps::acs::AcsCapabilities;
use crate::spec::caps::acs::AcsControl;
use crate::spec::caps::acs::AcsExtendedCapabilityHeader;
use crate::spec::caps::acs::DEFAULT_ACS_CAP_MASK;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use inspect::Inspect;

/// PCIe Access Control Services (ACS) extended capability emulator.
#[derive(Debug, Inspect)]
pub struct AcsExtendedCapability {
    capabilities: AcsCapabilities,
    control: AcsControl,
}

impl AcsExtendedCapability {
    /// Creates an ACS capability with the default set of sub-capabilities enabled (SV, TB, RR, CR, UF, DT).
    pub fn new() -> Self {
        Self::with_capabilities(DEFAULT_ACS_CAP_MASK)
    }

    /// Creates an ACS capability with the sub-capabilities indicated by `capability_bits`.
    pub fn with_capabilities(capability_bits: u16) -> Self {
        let capabilities = AcsCapabilities::from_bits(capability_bits);

        Self {
            capabilities,
            control: AcsControl::new(),
        }
    }
}

impl PciExtendedCapability for AcsExtendedCapability {
    fn label(&self) -> &str {
        "acs"
    }

    fn extended_capability_id(&self) -> u16 {
        ExtendedCapabilityId::ACS.0
    }

    fn capability_version(&self) -> u8 {
        1
    }

    fn len(&self) -> usize {
        12
    }

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        match AcsExtendedCapabilityHeader(offset) {
            AcsExtendedCapabilityHeader::HEADER => {
                value.set_low_high(
                    self.extended_capability_id(),
                    self.capability_version().into(),
                );
            }
            AcsExtendedCapabilityHeader::CAPS_CONTROL => {
                value.set_low_high(self.capabilities.into_bits(), self.control.into_bits());
            }
            AcsExtendedCapabilityHeader::EGRESS_CONTROL_VECTOR => value.set(0),
            _ => value.set(!0),
        }
    }

    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite) {
        // Note that all ACS control only affect the emulated port, and do not reflect
        // any underlying hardware capabilities.
        match AcsExtendedCapabilityHeader(offset) {
            AcsExtendedCapabilityHeader::HEADER => {
                tracelimit::warn_ratelimited!(
                    offset,
                    ?val,
                    "write to read-only ACS extended capability register"
                );
            }
            AcsExtendedCapabilityHeader::CAPS_CONTROL => {
                // Control bits are writable only if the matching capability bit is set.
                self.control = AcsControl::from_bits(
                    val.merge_high(self.control.into_bits()) & self.capabilities.into_bits(),
                );
            }
            AcsExtendedCapabilityHeader::EGRESS_CONTROL_VECTOR => {
                tracelimit::warn_ratelimited!(
                    offset,
                    ?val,
                    "ACS egress control vector writes are currently not supported; dropping write"
                );
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    offset,
                    ?val,
                    "unexpected ACS extended capability write"
                );
            }
        }
    }

    fn reset(&mut self) {
        self.control = AcsControl::new();
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
        #[mesh(package = "pci.capabilities.extended.acs")]
        pub struct SavedState {
            #[mesh(1)]
            pub control: u16,
        }
    }

    impl SaveRestore for AcsExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                control: self.control.into_bits(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.control = AcsControl::from_bits(state.control & self.capabilities.into_bits());
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
    fn test_acs_defaults() {
        let cap = AcsExtendedCapability::new();

        assert_eq!(cap.label(), "acs");
        assert_eq!(cap.extended_capability_id(), ExtendedCapabilityId::ACS.0);
        assert_eq!(cap.capability_version(), 1);
        assert_eq!(cap.len(), 12);
        assert_extended_header_contract(&cap);

        let caps_ctl = read_extended_cap_u32(&cap, AcsExtendedCapabilityHeader::CAPS_CONTROL.0);
        assert_eq!(caps_ctl as u16, DEFAULT_ACS_CAP_MASK);
        assert_eq!((caps_ctl >> 16) as u16, 0);
    }

    #[test]
    fn test_acs_control_write_masks_unsupported_bits() {
        let mut cap = AcsExtendedCapability::new();

        write_extended_cap_u32(
            &mut cap,
            AcsExtendedCapabilityHeader::CAPS_CONTROL.0,
            0xffff_0000,
        );
        let caps_ctl = read_extended_cap_u32(&cap, AcsExtendedCapabilityHeader::CAPS_CONTROL.0);

        assert_eq!((caps_ctl >> 16) as u16, DEFAULT_ACS_CAP_MASK);
    }

    #[test]
    fn test_acs_reset_clears_control() {
        let mut cap = AcsExtendedCapability::new();

        write_extended_cap_u32(
            &mut cap,
            AcsExtendedCapabilityHeader::CAPS_CONTROL.0,
            0xffff_0000,
        );
        cap.reset();

        let caps_ctl = read_extended_cap_u32(&cap, AcsExtendedCapabilityHeader::CAPS_CONTROL.0);
        assert_eq!((caps_ctl >> 16) as u16, 0);
    }

    #[test]
    fn test_acs_save_restore() {
        let mut cap = AcsExtendedCapability::new();
        write_extended_cap_u32(
            &mut cap,
            AcsExtendedCapabilityHeader::CAPS_CONTROL.0,
            0xffff_0000,
        );

        let saved = cap.save().expect("save should succeed");

        cap.reset();
        assert_eq!(
            (read_extended_cap_u32(&cap, AcsExtendedCapabilityHeader::CAPS_CONTROL.0) >> 16) as u16,
            0
        );

        cap.restore(saved).expect("restore should succeed");
        assert_eq!(
            (read_extended_cap_u32(&cap, AcsExtendedCapabilityHeader::CAPS_CONTROL.0) >> 16) as u16,
            DEFAULT_ACS_CAP_MASK
        );
    }
}
