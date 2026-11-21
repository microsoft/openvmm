// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A standalone VM Generation ID chipset device, for platforms where no
//! firmware hosts the generation ID (e.g. Linux direct boot). The platform
//! provides the buffer and describes it to the guest in its ACPI tables.

use crate::GenerationId;
use crate::GenerationIdRuntimeDeps;
use chipset_device::ChipsetDevice;
use chipset_device::poll_device::PollDevice;
use inspect::InspectMut;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// Standalone generation ID device with a platform-provided buffer.
#[derive(InspectMut)]
pub struct GenerationIdDevice {
    #[inspect(mut)]
    generation_id: GenerationId,
}

impl GenerationIdDevice {
    /// Creates a new device whose buffer is at offset 0 of `deps.gm`.
    pub fn new(initial_generation_id: [u8; 16], deps: GenerationIdRuntimeDeps) -> Self {
        let mut generation_id = GenerationId::new(initial_generation_id, deps);
        generation_id.set_fixed_address(0);
        Self { generation_id }
    }
}

impl ChangeDeviceState for GenerationIdDevice {
    fn start(&mut self) {
        // Restore updates the ID but not the buffer, which is not guest RAM
        // and so is not part of the saved memory.
        self.generation_id.write_to_guest();
    }

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.generation_id.reset();
    }
}

impl ChipsetDevice for GenerationIdDevice {
    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for GenerationIdDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.generation_id.poll(cx);
    }
}

mod save_restore {
    use super::*;

    pub mod state {
        use crate::GenerationId;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.generation_id")]
        pub struct SavedState {
            #[mesh(1)]
            pub generation_id: <GenerationId as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for GenerationIdDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                generation_id: self.generation_id.save()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.generation_id.restore(state.generation_id)
        }
    }
}
