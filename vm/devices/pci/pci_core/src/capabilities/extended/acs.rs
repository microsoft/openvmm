// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe Access Control Services (ACS) extended capability.

use super::PciExtendedCapability;
use crate::spec::caps::ExtendedCapabilityId;
use crate::spec::caps::acs::AcsCapabilities;
use crate::spec::caps::acs::AcsControl;
use crate::spec::caps::acs::AcsExtendedCapabilityHeader;
use crate::spec::caps::acs::DEFAULT_ACS_CAP_MASK;
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

    fn read_u32(&self, offset: u16) -> u32 {
        match AcsExtendedCapabilityHeader(offset) {
            AcsExtendedCapabilityHeader::HEADER => {
                u32::from(self.extended_capability_id())
                    | (u32::from(self.capability_version()) << 16)
            }
            AcsExtendedCapabilityHeader::CAPS_CONTROL => {
                self.capabilities.into_bits() as u32 | ((self.control.into_bits() as u32) << 16)
            }
            AcsExtendedCapabilityHeader::EGRESS_CONTROL_VECTOR => 0,
            _ => !0,
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        // Note that all ACS control only affect the emulated port, and do not reflect
        // any underlying hardware capabilities.
        match AcsExtendedCapabilityHeader(offset) {
            AcsExtendedCapabilityHeader::HEADER => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
                    "write to read-only ACS extended capability register"
                );
            }
            AcsExtendedCapabilityHeader::CAPS_CONTROL => {
                // Control bits are writable only if the matching capability bit is set.
                self.control =
                    AcsControl::from_bits(((val >> 16) as u16) & self.capabilities.into_bits());
            }
            AcsExtendedCapabilityHeader::EGRESS_CONTROL_VECTOR => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
                    "ACS egress control vector writes are currently not supported; dropping write"
                );
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
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
    use vmcore::save_restore::SaveRestore;

    #[test]
    fn test_acs_defaults() {
        let cap = AcsExtendedCapability::new();

        assert_eq!(cap.label(), "acs");
        assert_eq!(cap.extended_capability_id(), ExtendedCapabilityId::ACS.0);
        assert_eq!(cap.capability_version(), 1);
        assert_eq!(cap.len(), 12);
        assert_extended_header_contract(&cap);

        let caps_ctl = cap.read_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0);
        assert_eq!(caps_ctl as u16, DEFAULT_ACS_CAP_MASK);
        assert_eq!((caps_ctl >> 16) as u16, 0);
    }

    #[test]
    fn test_acs_control_write_masks_unsupported_bits() {
        let mut cap = AcsExtendedCapability::new();

        cap.write_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0, 0xffff_0000);
        let caps_ctl = cap.read_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0);

        assert_eq!((caps_ctl >> 16) as u16, DEFAULT_ACS_CAP_MASK);
    }

    #[test]
    fn test_acs_reset_clears_control() {
        let mut cap = AcsExtendedCapability::new();

        cap.write_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0, 0xffff_0000);
        cap.reset();

        let caps_ctl = cap.read_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0);
        assert_eq!((caps_ctl >> 16) as u16, 0);
    }

    #[test]
    fn test_acs_save_restore() {
        let mut cap = AcsExtendedCapability::new();
        cap.write_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0, 0xffff_0000);

        let saved = cap.save().expect("save should succeed");

        cap.reset();
        assert_eq!(
            (cap.read_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0) >> 16) as u16,
            0
        );

        cap.restore(saved).expect("restore should succeed");
        assert_eq!(
            (cap.read_u32(AcsExtendedCapabilityHeader::CAPS_CONTROL.0) >> 16) as u16,
            DEFAULT_ACS_CAP_MASK
        );
    }
}
