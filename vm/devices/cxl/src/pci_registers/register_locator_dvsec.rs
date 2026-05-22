// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL Register Locator PCIe DVSEC extended capability implementation.

use pci_core::capabilities::extended::PciExtendedCapability;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
use pci_core::spec::caps::dvsec::DvsecHeader1;
use pci_core::spec::caps::dvsec::DvsecHeader2;

use super::spec::CXL_DVSEC_VENDOR_ID;
use super::spec::register_locator_dvsec::CXL_REGISTER_LOCATOR_DVSEC_ID;
use super::spec::register_locator_dvsec::CXL_REGISTER_LOCATOR_DVSEC_REVISION;
use super::spec::register_locator_dvsec::CxlRegisterLocatorDvsecExtendedCapability;
use super::spec::register_locator_dvsec::CxlRegisterLocatorDvsecRegisterBlockEntry;
use super::spec::register_locator_dvsec::CxlRegisterLocatorDvsecRegisterOffset;
use super::spec::register_locator_dvsec::CxlRegisterLocatorDvsecRegisterOffsetLow;
use super::spec::register_locator_dvsec::CxlRegisterLocatorRegisterBir;
use super::spec::register_locator_dvsec::CxlRegisterLocatorRegisterBlockIdentifier;
use crate::spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES;
use thiserror::Error;

/// Register block offset encoding shift: DVSEC stores A[63:16], not byte address A[63:0].
const REGISTER_BLOCK_OFFSET_ENCODING_SHIFT: u32 = 16;

/// Errors returned when configuring Register Locator entries.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Error)]
pub enum RegisterLocatorConfigError {
    /// Register block offset must be aligned to `CXL_COMPONENT_REGISTERS_SIZE_BYTES`.
    #[error("register block offset must be aligned to CXL component-register size")]
    RegisterBlockOffsetUnaligned,
}

impl CxlRegisterLocatorDvsecExtendedCapability {
    /// Creates a new Register Locator DVSEC capability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one register-block entry to the Register Locator DVSEC.
    pub fn with_register_block(
        mut self,
        register_bir: CxlRegisterLocatorRegisterBir,
        register_block_identifier: CxlRegisterLocatorRegisterBlockIdentifier,
        register_block_offset: u64,
    ) -> Result<Self, RegisterLocatorConfigError> {
        if !register_block_offset.is_multiple_of(CXL_COMPONENT_REGISTERS_SIZE_BYTES) {
            return Err(RegisterLocatorConfigError::RegisterBlockOffsetUnaligned);
        }

        // The spec encodes register block offsets as A[63:16], so convert byte offset here.
        let register_block_offset_encoded =
            register_block_offset >> REGISTER_BLOCK_OFFSET_ENCODING_SHIFT;

        let offset_low = CxlRegisterLocatorDvsecRegisterOffsetLow::new()
            .with_register_bir(register_bir.bits())
            .with_register_block_identifier(register_block_identifier.bits())
            .with_register_block_offset_low(register_block_offset_encoded as u16);
        let offset_high =
            (register_block_offset_encoded >> REGISTER_BLOCK_OFFSET_ENCODING_SHIFT) as u32;

        self.register_blocks
            .push(CxlRegisterLocatorDvsecRegisterBlockEntry {
                offset_low,
                offset_high,
            });
        self.reset_baseline_register_blocks = self.register_blocks.clone();

        Ok(self)
    }

    fn dvsec_len(&self) -> usize {
        usize::from(self.encoded_length())
    }

    fn read_dvsec_u32(&self, offset: u16) -> u32 {
        if offset == DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0 {
            return self.dvsec_header1().into_bits();
        }

        if offset == CxlRegisterLocatorDvsecRegisterOffset::DVSEC_HEADER2 {
            return u32::from(Self::dvsec_header2().into_bits());
        }

        if offset < CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW {
            return !0;
        }

        let rel = offset - CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW;
        let index = usize::from(rel / CxlRegisterLocatorDvsecRegisterOffset::REGISTER_BLOCK_STRIDE);
        let within = rel % CxlRegisterLocatorDvsecRegisterOffset::REGISTER_BLOCK_STRIDE;

        let Some(entry) = self.register_blocks.get(index) else {
            return !0;
        };

        match within {
            0x0 => entry.offset_low.into_bits(),
            0x4 => entry.offset_high,
            _ => !0,
        }
    }

    fn write_dvsec_u32(&mut self, _offset: u16, _value: u32) {
        // Register Locator fields are HwInit/RO from software perspective.
    }

    fn reset_state(&mut self) {
        self.register_blocks = self.reset_baseline_register_blocks.clone();
    }

    fn encoded_length(&self) -> u16 {
        let blocks = self.register_blocks.len() as u16;
        CxlRegisterLocatorDvsecRegisterOffset::BASE_LENGTH.saturating_add(
            blocks.saturating_mul(CxlRegisterLocatorDvsecRegisterOffset::REGISTER_BLOCK_STRIDE),
        )
    }

    fn dvsec_header1(&self) -> DvsecHeader1 {
        DvsecHeader1::new()
            .with_dvsec_vendor_id(CXL_DVSEC_VENDOR_ID)
            .with_dvsec_revision(CXL_REGISTER_LOCATOR_DVSEC_REVISION)
            .with_dvsec_length(self.encoded_length())
    }

    fn dvsec_header2() -> DvsecHeader2 {
        DvsecHeader2::new().with_dvsec_id(CXL_REGISTER_LOCATOR_DVSEC_ID)
    }
}

impl PciExtendedCapability for CxlRegisterLocatorDvsecExtendedCapability {
    fn label(&self) -> &str {
        "register_locator_dvsec"
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
        #[mesh(package = "cxl.pci_registers.register_locator_dvsec")]
        pub struct SavedState {
            #[mesh(1)]
            pub register_blocks: Vec<u64>,
            #[mesh(2)]
            pub reset_baseline_register_blocks: Vec<u64>,
        }
    }

    impl SaveRestore for CxlRegisterLocatorDvsecExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let register_blocks = self
                .register_blocks
                .iter()
                .map(|entry| {
                    ((u64::from(entry.offset_high)) << 32) | u64::from(entry.offset_low.into_bits())
                })
                .collect();
            let reset_baseline_register_blocks = self
                .reset_baseline_register_blocks
                .iter()
                .map(|entry| {
                    ((u64::from(entry.offset_high)) << 32) | u64::from(entry.offset_low.into_bits())
                })
                .collect();
            Ok(state::SavedState {
                register_blocks,
                reset_baseline_register_blocks,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.register_blocks = state
                .register_blocks
                .into_iter()
                .map(|packed| CxlRegisterLocatorDvsecRegisterBlockEntry {
                    offset_low: CxlRegisterLocatorDvsecRegisterOffsetLow::from_bits(packed as u32),
                    offset_high: (packed >> 32) as u32,
                })
                .collect();
            self.reset_baseline_register_blocks = state
                .reset_baseline_register_blocks
                .into_iter()
                .map(|packed| CxlRegisterLocatorDvsecRegisterBlockEntry {
                    offset_low: CxlRegisterLocatorDvsecRegisterOffsetLow::from_bits(packed as u32),
                    offset_high: (packed >> 32) as u32,
                })
                .collect();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use pci_core::capabilities::extended::PciExtendedCapability;
    use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
    use vmcore::save_restore::SaveRestore;

    use super::CxlRegisterLocatorDvsecExtendedCapability;
    use super::CxlRegisterLocatorDvsecRegisterOffset;
    use super::CxlRegisterLocatorRegisterBir;
    use super::CxlRegisterLocatorRegisterBlockIdentifier;
    use super::RegisterLocatorConfigError;
    use crate::spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES;

    #[test]
    fn header_registers_match_required_constants() {
        let cap = CxlRegisterLocatorDvsecExtendedCapability::new()
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_10H,
                CxlRegisterLocatorRegisterBlockIdentifier::COMPONENT_REGISTERS,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES,
            )
            .expect("first block should configure")
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_10H,
                CxlRegisterLocatorRegisterBlockIdentifier::CXL_DEVICE_REGISTERS,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES * 2,
            )
            .expect("second block should configure")
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_14H,
                CxlRegisterLocatorRegisterBlockIdentifier::DESIGNATED_VENDOR_SPECIFIC,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES * 3,
            )
            .expect("third block should configure");

        assert_eq!(
            cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0),
            0x0240_1e98
        );
        assert_eq!(
            cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::DVSEC_HEADER2) & 0xffff,
            0x0008
        );
    }

    #[test]
    fn label_is_register_locator_dvsec() {
        let cap = CxlRegisterLocatorDvsecExtendedCapability::new();
        assert_eq!(cap.label(), "register_locator_dvsec");
    }

    #[test]
    fn unaligned_register_block_offset_is_rejected() {
        let result = CxlRegisterLocatorDvsecExtendedCapability::new().with_register_block(
            CxlRegisterLocatorRegisterBir::BAR_10H,
            CxlRegisterLocatorRegisterBlockIdentifier::CXL_DEVICE_REGISTERS,
            CXL_COMPONENT_REGISTERS_SIZE_BYTES + 1,
        );
        assert!(matches!(
            result,
            Err(RegisterLocatorConfigError::RegisterBlockOffsetUnaligned)
        ));
    }

    #[test]
    fn register_block_entries_are_encoded() {
        let cap = CxlRegisterLocatorDvsecExtendedCapability::new()
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_10H,
                CxlRegisterLocatorRegisterBlockIdentifier::COMPONENT_REGISTERS,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES,
            )
            .expect("block should configure")
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_18H,
                CxlRegisterLocatorRegisterBlockIdentifier::CXL_DEVICE_REGISTERS,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES * 2,
            )
            .expect("block should configure");

        assert_eq!(
            cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW),
            0x0001_0100
        );
        assert_eq!(
            cap.read_u32(
                CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW + 0x04
            ),
            0x0000_0000
        );

        assert_eq!(
            cap.read_u32(
                CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW + 0x08
            ),
            0x0002_0302
        );
    }

    #[test]
    fn save_restore_round_trips_state() {
        let mut cap = CxlRegisterLocatorDvsecExtendedCapability::new()
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_10H,
                CxlRegisterLocatorRegisterBlockIdentifier::COMPONENT_REGISTERS,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES,
            )
            .expect("block should configure")
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_1CH,
                CxlRegisterLocatorRegisterBlockIdentifier::CHMU_REGISTERS,
                CXL_COMPONENT_REGISTERS_SIZE_BYTES * 4,
            )
            .expect("block should configure");

        let saved = cap.save().expect("save should succeed");
        let mut restored = CxlRegisterLocatorDvsecExtendedCapability::new();
        restored.restore(saved).expect("restore should succeed");

        assert_eq!(
            restored.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0),
            cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0)
        );
        assert_eq!(
            restored.read_u32(CxlRegisterLocatorDvsecRegisterOffset::DVSEC_HEADER2),
            cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::DVSEC_HEADER2)
        );
        assert_eq!(
            restored
                .read_u32(CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW),
            cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW)
        );
        assert_eq!(
            restored.read_u32(
                CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW + 0x08
            ),
            cap.read_u32(
                CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW + 0x08
            )
        );
    }

    #[test]
    fn reset_preserves_configured_register_blocks() {
        let mut cap = CxlRegisterLocatorDvsecExtendedCapability::new()
            .with_register_block(
                CxlRegisterLocatorRegisterBir::BAR_10H,
                CxlRegisterLocatorRegisterBlockIdentifier::COMPONENT_REGISTERS,
                0,
            )
            .expect("block should configure");

        let before = cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::DVSEC_HEADER2);
        cap.reset();
        let after = cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::DVSEC_HEADER2);

        assert_eq!(before, after);
        assert_eq!(
            cap.read_u32(CxlRegisterLocatorDvsecRegisterOffset::FIRST_REGISTER_BLOCK_OFFSET_LOW),
            0x0000_0100
        );
    }
}
