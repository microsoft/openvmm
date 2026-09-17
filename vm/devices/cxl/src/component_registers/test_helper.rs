// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::CxlComponentRegister;
use crate::spec::CxlComponentRegisterType;
use inspect::Inspect;

/// Simple in-memory component register block used by unit tests.
#[derive(Inspect)]
pub struct TestCxlComponentRegisterBlock {
    register_type: CxlComponentRegisterType,
    bytes: Vec<u8>,
}

impl TestCxlComponentRegisterBlock {
    /// Creates an in-memory register block used by unit tests.
    pub fn new(register_type: CxlComponentRegisterType, len: usize) -> Self {
        Self {
            register_type,
            bytes: vec![0; len],
        }
    }
}

impl CxlComponentRegister for TestCxlComponentRegisterBlock {
    fn label(&self) -> &str {
        "test-register-block"
    }

    fn register_type(&self) -> CxlComponentRegisterType {
        self.register_type
    }

    fn capability_id(&self) -> u16 {
        0x20
    }

    fn capability_version(&self) -> u8 {
        1
    }

    fn len(&self) -> u16 {
        self.bytes.len() as u16
    }

    fn read_u32(&self, offset: u16) -> Option<u32> {
        let offset = usize::from(offset);
        let end = offset.checked_add(4)?;
        if !offset.is_multiple_of(4) || end > self.bytes.len() {
            return None;
        }

        Some(u32::from_le_bytes(self.bytes[offset..end].try_into().ok()?))
    }

    fn write_u32(&mut self, offset: u16, val: u32) -> bool {
        let offset = usize::from(offset);
        let Some(end) = offset.checked_add(4) else {
            return false;
        };

        if !offset.is_multiple_of(4) || end > self.bytes.len() {
            return false;
        }

        self.bytes[offset..end].copy_from_slice(&val.to_le_bytes());
        true
    }

    fn reset(&mut self) {
        self.bytes.fill(0);
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
        #[mesh(package = "cxl.component_registers.test_helper")]
        pub struct SavedState {
            #[mesh(1)]
            pub bytes: Vec<u8>,
        }
    }

    impl SaveRestore for TestCxlComponentRegisterBlock {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                bytes: self.bytes.clone(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            if state.bytes.len() != self.bytes.len() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "test register size mismatch: saved {}, current {}",
                    state.bytes.len(),
                    self.bytes.len()
                )));
            }

            self.bytes.copy_from_slice(&state.bytes);
            Ok(())
        }
    }
}
