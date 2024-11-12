// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`StateUnit`] support for [`VmTimeKeeper`].

use inspect::InspectMut;
use mesh::Receiver;
use state_unit::StateRequest;
use state_unit::StateUnit;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vmtime::VmTimeKeeper;

#[derive(InspectMut)]
#[inspect(transparent)]
struct KeeperUnit<'a>(#[inspect(mut)] &'a mut VmTimeKeeper);

impl StateUnit for KeeperUnit<'_> {
    async fn start(&mut self) {
        self.0.start().await;
    }

    async fn stop(&mut self) {
        self.0.stop().await;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset().await;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(Some(SavedStateBlob::new(self.0.save())))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.0.restore(state.parse()?).await;
        Ok(())
    }
}

/// Runs the VM time keeper, responding to state changes from `recv`, until
/// `recv` is closed.
pub async fn run_vmtime(keeper: &mut VmTimeKeeper, recv: Receiver<StateRequest>) {
    state_unit::run_unit(KeeperUnit(keeper), recv).await;
}
