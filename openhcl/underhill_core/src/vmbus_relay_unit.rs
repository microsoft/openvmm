// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inspect::Inspect;
use pal_async::task::Spawn;
use state_unit::run_async_unit;
use state_unit::NameInUse;
use state_unit::SpawnedUnit;
use state_unit::StateUnit;
use state_unit::UnitBuilder;
use vmbus_relay::HostVmbusTransport;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// A handle to a vmbus relay that is registered as a state unit.
///
/// FUTURE: incorporate the state unit handling directly into [`HostVmbusTransport`].
pub struct VmbusRelayHandle {
    unit: SpawnedUnit<VmbusRelayUnit>,
}

impl VmbusRelayHandle {
    /// Makes a new handle, registering the server via `builder`.
    pub fn new(
        spawner: &impl Spawn,
        builder: UnitBuilder<'_>,
        relay: HostVmbusTransport,
    ) -> Result<Self, NameInUse> {
        let unit = builder.spawn(spawner, |recv| run_async_unit(VmbusRelayUnit(relay), recv))?;
        Ok(Self { unit })
    }

    /// Tears down the vmbus relay, leaving any host state untouched.
    pub async fn teardown(self) {
        self.unit.remove().await;
    }
}

/// A newtype over [`HostVmbusTransport`] implementing [`StateUnit`].
#[derive(Inspect)]
#[inspect(transparent)]
struct VmbusRelayUnit(HostVmbusTransport);

impl StateUnit for &'_ VmbusRelayUnit {
    async fn start(&mut self) {
        self.0.start();
    }

    async fn stop(&mut self) {
        self.0.stop().await;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(Some(SavedStateBlob::new(self.0.save().await)))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.0
            .restore(state.parse()?)
            .await
            .map_err(RestoreError::Other)
    }

    async fn post_restore(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
