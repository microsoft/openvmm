// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSI Capability.

use super::PciCapability;
use crate::capabilities::msix::MsiInterrupt;
use crate::msi::MsiTarget;
use crate::spec::caps::CapabilityId;
use crate::spec::caps::msi::MsiCapabilityHeader;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use std::fmt::Debug;
use std::sync::Arc;
use vmcore::interrupt::Interrupt;

/// MSI capability implementation for PCI configuration space.
#[derive(Debug, Inspect)]
pub struct MsiCapability {
    #[inspect(with = "|x| inspect::adhoc(|req| x.lock().inspect_mut(req))")]
    state: Arc<Mutex<MsiCapabilityState>>,
    addr_64bit: bool,
    per_vector_masking: bool,
}

#[derive(Debug, InspectMut)]
struct MsiCapabilityState {
    enabled: bool,
    multiple_message_enable: u8,  // 2^(MME) messages allocated
    multiple_message_capable: u8, // 2^(MMC) maximum messages requestable
    #[inspect(hex)]
    address: u64,
    #[inspect(hex)]
    data: u16,
    #[inspect(hex)]
    mask_bits: u32,
    #[inspect(hex)]
    pending_bits: u32,
    interrupt: Option<MsiInterrupt>,
}

impl MsiCapabilityState {
    fn new(multiple_message_capable: u8, _addr_64bit: bool, per_vector_masking: bool) -> Self {
        Self {
            enabled: false,
            multiple_message_enable: 0,
            multiple_message_capable,
            address: 0,
            data: 0,
            mask_bits: if per_vector_masking { 0xFFFFFFFF } else { 0 },
            pending_bits: 0,
            interrupt: None,
        }
    }

    fn control_register(&self, addr_64bit: bool, per_vector_masking: bool) -> u32 {
        let mut control = 0u32;
        control |= (self.multiple_message_capable as u32) << 1; // MMC field (bits 1-3)
        control |= (self.multiple_message_enable as u32) << 4; // MME field (bits 4-6)
        if addr_64bit {
            control |= 1 << 7; // 64-bit Address Capable (bit 7)
        }
        if per_vector_masking {
            control |= 1 << 8; // Per-vector Masking Capable (bit 8)
        }
        if self.enabled {
            control |= 1 << 0; // MSI Enable (bit 0)
        }
        control
    }
}

impl MsiCapability {
    /// Create a new MSI capability.
    ///
    /// # Arguments
    /// * `multiple_message_capable` - log2 of maximum number of messages (0-5)
    /// * `addr_64bit` - Whether 64-bit addressing is supported
    /// * `per_vector_masking` - Whether per-vector masking is supported
    /// * `msi_target` - MSI target
    pub fn new(
        multiple_message_capable: u8,
        addr_64bit: bool,
        per_vector_masking: bool,
        msi_target: &MsiTarget,
    ) -> Self {
        assert!(multiple_message_capable <= 5, "MMC must be 0-5");

        let interrupt = MsiInterrupt::new(msi_target.clone());
        let state = MsiCapabilityState {
            interrupt: Some(interrupt),
            ..MsiCapabilityState::new(multiple_message_capable, addr_64bit, per_vector_masking)
        };

        Self {
            state: Arc::new(Mutex::new(state)),
            addr_64bit,
            per_vector_masking,
        }
    }

    /// Get the interrupt object for signaling MSI.
    pub fn interrupt(&self) -> Option<Interrupt> {
        self.state.lock().interrupt.as_mut().map(|i| i.interrupt())
    }

    fn len_bytes(&self) -> usize {
        let mut len = 8; // Base: ID + Next + Control + Message Address Low
        if self.addr_64bit {
            len += 4; // Message Address High
        }
        len += 2; // Message Data (16-bit, but aligned to 4-byte boundary)
        if self.per_vector_masking {
            len += 8; // Mask Bits + Pending Bits
        }
        // Round up to next 4-byte boundary
        (len + 3) & !3
    }
}

impl PciCapability for MsiCapability {
    fn label(&self) -> &str {
        "msi"
    }

    fn capability_id(&self) -> CapabilityId {
        CapabilityId::MSI
    }

    fn len(&self) -> usize {
        self.len_bytes()
    }

    fn read_u32(&self, offset: u16) -> u32 {
        let state = self.state.lock();

        match MsiCapabilityHeader(offset) {
            MsiCapabilityHeader::CONTROL_CAPS => {
                let control_reg = state.control_register(self.addr_64bit, self.per_vector_masking);
                CapabilityId::MSI.0 as u32 | (control_reg << 16)
            }
            MsiCapabilityHeader::MSG_ADDR_LO => state.address as u32,
            MsiCapabilityHeader::MSG_ADDR_HI if self.addr_64bit => (state.address >> 32) as u32,
            MsiCapabilityHeader::MSG_DATA_32 if !self.addr_64bit => state.data as u32,
            MsiCapabilityHeader::MSG_DATA_64 if self.addr_64bit => state.data as u32,
            MsiCapabilityHeader::MASK_BITS if self.addr_64bit && self.per_vector_masking => {
                state.mask_bits
            }
            MsiCapabilityHeader::PENDING_BITS if self.addr_64bit && self.per_vector_masking => {
                state.pending_bits
            }
            _ => {
                tracelimit::warn_ratelimited!("Unexpected MSI read offset {:#x}", offset);
                0
            }
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        let mut state = self.state.lock();

        match MsiCapabilityHeader(offset) {
            MsiCapabilityHeader::CONTROL_CAPS => {
                let control_val = (val >> 16) & 0xFFFF;
                let old_enabled = state.enabled;
                let new_enabled = control_val & 1 != 0;
                let mme = ((control_val >> 4) & 0x7) as u8;

                // Update MME (Multiple Message Enable) - limited by MMC
                state.multiple_message_enable = mme.min(state.multiple_message_capable);
                state.enabled = new_enabled;

                // Handle enable/disable state changes
                let address = state.address;
                let data = state.data as u32;
                if let Some(ref mut interrupt) = state.interrupt {
                    if new_enabled && !old_enabled {
                        // Enable MSI
                        interrupt.enable(address, data, false);
                    } else if !new_enabled && old_enabled {
                        // Disable MSI
                        interrupt.disable();
                    }
                }
            }
            MsiCapabilityHeader::MSG_ADDR_LO => {
                state.address = (state.address & 0xFFFFFFFF00000000) | (val as u64);

                // Update interrupt if enabled
                if state.enabled {
                    let address = state.address;
                    let data = state.data as u32;
                    if let Some(ref mut interrupt) = state.interrupt {
                        interrupt.enable(address, data, false);
                    }
                }
            }
            MsiCapabilityHeader::MSG_ADDR_HI if self.addr_64bit => {
                state.address = (state.address & 0xFFFFFFFF) | ((val as u64) << 32);

                // Update interrupt if enabled
                if state.enabled {
                    let address = state.address;
                    let data = state.data as u32;
                    if let Some(ref mut interrupt) = state.interrupt {
                        interrupt.enable(address, data, false);
                    }
                }
            }
            MsiCapabilityHeader::MSG_DATA_32 if !self.addr_64bit => {
                state.data = val as u16;

                // Update interrupt if enabled
                if state.enabled {
                    let address = state.address;
                    let data = state.data as u32;
                    if let Some(ref mut interrupt) = state.interrupt {
                        interrupt.enable(address, data, false);
                    }
                }
            }
            MsiCapabilityHeader::MSG_DATA_64 if self.addr_64bit => {
                state.data = val as u16;

                // Update interrupt if enabled
                if state.enabled {
                    let address = state.address;
                    let data = state.data as u32;
                    if let Some(ref mut interrupt) = state.interrupt {
                        interrupt.enable(address, data, false);
                    }
                }
            }
            MsiCapabilityHeader::MASK_BITS if self.addr_64bit && self.per_vector_masking => {
                state.mask_bits = val;
            }
            MsiCapabilityHeader::PENDING_BITS if self.addr_64bit && self.per_vector_masking => {
                // Pending bits are typically read-only, but some implementations may allow clearing
                tracelimit::warn_ratelimited!(
                    "Write to MSI pending bits register (typically read-only)"
                );
            }
            _ => {
                tracelimit::warn_ratelimited!("Unexpected MSI write offset {:#x}", offset);
            }
        }
    }

    fn reset(&mut self) {
        let mut state = self.state.lock();

        // Disable MSI
        if state.enabled {
            if let Some(ref mut interrupt) = state.interrupt {
                interrupt.disable();
            }
        }

        // Reset to default values
        state.enabled = false;
        state.multiple_message_enable = 0;
        state.address = 0;
        state.data = 0;
        if self.per_vector_masking {
            state.mask_bits = 0;
            state.pending_bits = 0;
        }
    }
}

mod save_restore {
    use super::*;
    use thiserror::Error;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Debug, Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.caps.msi")]
        pub struct SavedState {
            #[mesh(1)]
            pub enabled: bool,
            #[mesh(2)]
            pub multiple_message_enable: u8,
            #[mesh(3)]
            pub address: u64,
            #[mesh(4)]
            pub data: u16,
            #[mesh(5)]
            pub mask_bits: u32,
            #[mesh(6)]
            pub pending_bits: u32,
        }
    }

    #[derive(Debug, Error)]
    enum MsiRestoreError {
        #[error("invalid multiple message enable value: {0}")]
        InvalidMultipleMessageEnable(u8),
    }

    impl SaveRestore for MsiCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let state = self.state.lock();
            Ok(state::SavedState {
                enabled: state.enabled,
                multiple_message_enable: state.multiple_message_enable,
                address: state.address,
                data: state.data,
                mask_bits: state.mask_bits,
                pending_bits: state.pending_bits,
            })
        }

        fn restore(&mut self, saved_state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                enabled,
                multiple_message_enable,
                address,
                data,
                mask_bits,
                pending_bits,
            } = saved_state;

            if multiple_message_enable > 5 {
                return Err(RestoreError::InvalidSavedState(
                    MsiRestoreError::InvalidMultipleMessageEnable(multiple_message_enable).into(),
                ));
            }

            let mut state = self.state.lock();

            // Disable current interrupt if needed
            if state.enabled {
                if let Some(ref mut interrupt) = state.interrupt {
                    interrupt.disable();
                }
            }

            // Restore state
            state.enabled = enabled;
            state.multiple_message_enable =
                multiple_message_enable.min(state.multiple_message_capable);
            state.address = address;
            state.data = data;
            state.mask_bits = mask_bits;
            state.pending_bits = pending_bits;

            // Re-enable interrupt if needed
            if state.enabled {
                let address = state.address;
                let data = state.data as u32;
                if let Some(ref mut interrupt) = state.interrupt {
                    interrupt.enable(address, data, false);
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msi::MsiConnection;
    use crate::test_helpers::TestPciInterruptController;

    #[test]
    fn msi_check() {
        let msi_conn = MsiConnection::new(1);
        let mut cap = MsiCapability::new(2, true, false, msi_conn.target()); // 4 messages max, 64-bit, no masking
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(0, msi_controller.signal_msi());

        // Check initial capabilities register
        // Capability ID (0x05) + MMC=2 (4 messages) + 64-bit capable
        assert_eq!(cap.read_u32(0), 0x00840005); // 0x05 (ID) | (0x84 << 16) where 0x84 = MMC=2(<<1) + 64bit(<<7)

        // Check initial address registers
        assert_eq!(cap.read_u32(4), 0); // Address low
        assert_eq!(cap.read_u32(8), 0); // Address high
        assert_eq!(cap.read_u32(12), 0); // Data

        // Write address and data
        cap.write_u32(4, 0x12345678);
        cap.write_u32(8, 0x9abcdef0);
        cap.write_u32(12, 0x1234);

        assert_eq!(cap.read_u32(4), 0x12345678);
        assert_eq!(cap.read_u32(8), 0x9abcdef0);
        assert_eq!(cap.read_u32(12), 0x1234);

        // Enable MSI with 2 messages (MME=1)
        cap.write_u32(0, 0x00110005); // Enable + MME=1 (bits 0 and 4-6)
        assert_eq!(cap.read_u32(0), 0x00950005); // Should show enabled with all capability bits

        // Test reset
        cap.reset();
        assert_eq!(cap.read_u32(0), 0x00840005); // Back to disabled
        assert_eq!(cap.read_u32(4), 0);
        assert_eq!(cap.read_u32(8), 0);
        assert_eq!(cap.read_u32(12), 0);
    }

    #[test]
    fn msi_32bit_check() {
        let msi_conn = MsiConnection::new(1);
        let mut cap = MsiCapability::new(1, false, false, msi_conn.target()); // 2 messages max, 32-bit, no masking
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(0, msi_controller.signal_msi());

        // Check initial capabilities register (no 64-bit bit set)
        assert_eq!(cap.read_u32(0), 0x00020005); // MMC=1 (2 messages) + Capability ID

        // For 32-bit, data is at offset 8, not 12
        cap.write_u32(4, 0x12345678); // Address
        cap.write_u32(8, 0x1234); // Data

        assert_eq!(cap.read_u32(4), 0x12345678);
        assert_eq!(cap.read_u32(8), 0x1234);
    }

    #[test]
    fn test_msi_save_restore() {
        use vmcore::save_restore::SaveRestore;

        let msi_conn = MsiConnection::new(1);
        let mut cap = MsiCapability::new(2, true, false, msi_conn.target()); // 4 messages max, 64-bit, no masking
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(0, msi_controller.signal_msi());

        // Configure MSI capability with specific values
        cap.write_u32(4, 0x12345678); // Address low
        cap.write_u32(8, 0x9abcdef0); // Address high
        cap.write_u32(12, 0x5678); // Data
        cap.write_u32(0, 0x00110001); // Enable MSI with MME=1 (2 messages)

        // Verify initial state
        assert_eq!(cap.read_u32(0), 0x00950005); // Enabled with capabilities
        assert_eq!(cap.read_u32(4), 0x12345678);
        assert_eq!(cap.read_u32(8), 0x9abcdef0);
        assert_eq!(cap.read_u32(12), 0x5678);

        // Save the state
        let saved_state = cap.save().expect("save should succeed");

        // Reset the capability
        cap.reset();
        assert_eq!(cap.read_u32(0), 0x00840005); // Back to disabled
        assert_eq!(cap.read_u32(4), 0);
        assert_eq!(cap.read_u32(8), 0);
        assert_eq!(cap.read_u32(12), 0);

        // Restore the state
        cap.restore(saved_state).expect("restore should succeed");

        // Verify restored state
        assert_eq!(cap.read_u32(0), 0x00950005); // Should be enabled again
        assert_eq!(cap.read_u32(4), 0x12345678);
        assert_eq!(cap.read_u32(8), 0x9abcdef0);
        assert_eq!(cap.read_u32(12), 0x5678);
    }

    #[test]
    fn test_msi_save_restore_32bit_with_masking() {
        use vmcore::save_restore::SaveRestore;

        let msi_conn = MsiConnection::new(1);
        let mut cap = MsiCapability::new(3, false, true, msi_conn.target()); // 8 messages max, 32-bit, with masking
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(0, msi_controller.signal_msi());

        // Configure MSI capability with specific values
        cap.write_u32(4, 0x87654321); // Address (32-bit)
        cap.write_u32(8, 0x1234); // Data
        cap.write_u32(12, 0xaaaabbbb); // Mask bits (for per-vector masking)
        cap.write_u32(0, 0x00210001); // Enable MSI with MME=2 (4 messages)

        // Verify initial state
        let control_reg = cap.read_u32(0);
        let control_val = (control_reg >> 16) & 0xFFFF;
        assert!(control_val & 1 != 0); // MSI enabled
        assert_eq!((control_val >> 4) & 0x7, 2); // MME = 2
        assert_eq!(cap.read_u32(4), 0x87654321);
        assert_eq!(cap.read_u32(8), 0x1234);

        // Save the state
        let saved_state = cap.save().expect("save should succeed");

        // Modify state
        cap.write_u32(4, 0x11111111);
        cap.write_u32(8, 0x9999);
        cap.write_u32(0, 0x00000005); // Disable MSI

        // Verify changed state
        let control_reg = cap.read_u32(0);
        let control_val = (control_reg >> 16) & 0xFFFF;
        assert_eq!(control_val & 1, 0); // MSI disabled
        assert_eq!(cap.read_u32(4), 0x11111111);
        assert_eq!(cap.read_u32(8), 0x9999);

        // Restore the state
        cap.restore(saved_state).expect("restore should succeed");

        // Verify restored state
        let control_reg = cap.read_u32(0);
        let control_val = (control_reg >> 16) & 0xFFFF;
        assert!(control_val & 1 != 0); // MSI enabled
        assert_eq!((control_val >> 4) & 0x7, 2); // MME = 2
        assert_eq!(cap.read_u32(4), 0x87654321);
        assert_eq!(cap.read_u32(8), 0x1234);
    }

    #[test]
    fn test_msi_save_restore_mme_clamping() {
        use vmcore::save_restore::SaveRestore;

        let msi_conn = MsiConnection::new(1);
        let mut cap = MsiCapability::new(1, true, false, msi_conn.target()); // Only 2 messages max (MMC=1)
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(0, msi_controller.signal_msi());

        // Configure with MME=3 (8 messages), but device only supports MMC=1 (2 messages)
        cap.write_u32(4, 0x12345678); // Address low
        cap.write_u32(8, 0x9abcdef0); // Address high
        cap.write_u32(12, 0x5678); // Data
        cap.write_u32(0, 0x00310001); // Enable MSI with MME=3

        // Verify MME was clamped to MMC (1)
        let control_reg = cap.read_u32(0);
        let control_val = (control_reg >> 16) & 0xFFFF;
        let mme = (control_val >> 4) & 0x7;
        assert_eq!(mme, 1); // Should be clamped to MMC=1

        // Save the state (which should preserve the clamped MME)
        let saved_state = cap.save().expect("save should succeed");

        // Reset the capability
        cap.reset();

        // Restore the state
        cap.restore(saved_state).expect("restore should succeed");

        // Check that MME is still properly clamped after restore
        let control_reg = cap.read_u32(0);
        let control_val = (control_reg >> 16) & 0xFFFF;
        let mme = (control_val >> 4) & 0x7;
        let enabled = control_val & 1 != 0;
        assert_eq!(mme, 1); // Should still be clamped to MMC=1
        assert!(enabled); // Should be enabled
        assert_eq!(cap.read_u32(4), 0x12345678);
        assert_eq!(cap.read_u32(8), 0x9abcdef0);
        assert_eq!(cap.read_u32(12), 0x5678);
    }
}
