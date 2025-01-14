// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PS/2 mouse. Not currently implemented.

use inspect::Inspect;
use std::collections::VecDeque;

/// Not yet implemented.
#[derive(Inspect)]
pub struct Ps2Mouse {
    #[inspect(bytes)]
    output_buffer: VecDeque<u8>,
}

impl Ps2Mouse {
    pub fn new() -> Self {
        Self {
            output_buffer: VecDeque::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn output(&mut self) -> Option<u8> {
        self.output_buffer.pop_front()
    }

    pub fn input(&mut self, data: u8) {
        tracing::trace!(data, "mouse command");

        // RESET
        if data == 0xFF {
            self.output_buffer.push_back(0xFA); // ACKNOWLEDGE
            self.output_buffer.push_back(0xAA); // COMPLETE
            self.output_buffer.push_back(0); // IDENTITY
        } else {
            tracing::debug!(?data, "unimplemented mouse command");
            self.output_buffer.push_back(0xFA); // ACKNOWLEDGE
        }
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
        #[mesh(package = "chipset.i8042.mouse")]
        pub struct SavedState {
            #[mesh(1)]
            pub output_buffer: Vec<u8>,
        }
    }

    impl SaveRestore for Ps2Mouse {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self { output_buffer } = self;

            let saved_state = state::SavedState {
                output_buffer: output_buffer.iter().copied().collect(),
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { output_buffer } = state;

            *self = Self {
                output_buffer: output_buffer.into(),
            };

            Ok(())
        }
    }
}
