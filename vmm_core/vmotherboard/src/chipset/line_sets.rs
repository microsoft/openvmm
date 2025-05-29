// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::ChipsetDevice;
use chipset_device_resources::LineSetId;
use closeable_mutex::CloseableMutex;
use inspect::InspectMut;
use state_unit::SpawnedUnit;
use state_unit::StateUnit;
use state_unit::StateUnits;
use state_unit::UnitHandle;
use std::collections::HashMap;
use std::sync::Arc;
use vmcore::line_interrupt::LineSet;
use vmcore::line_interrupt::LineSetTarget;
use vmcore::vm_task::VmTaskDriverSource;

pub struct LineSets {
    map: HashMap<LineSetId, (Arc<LineSet>, usize)>,
    pub units: Vec<SpawnedUnit<()>>,
}

impl LineSets {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            units: Vec::new(),
        }
    }

    pub fn line_set(
        &mut self,
        driver: &VmTaskDriverSource,
        units: &StateUnits,
        id: LineSetId,
    ) -> (&LineSet, &UnitHandle) {
        let (line_set, index) = self.map.entry(id.clone()).or_insert_with(|| {
            let set = Arc::new(LineSet::new());
            let unit = units
                .add(format!("lines/{}", id.name()))
                .spawn(driver.simple(), {
                    let set = set.clone();
                    async move |mut recv| {
                        while let Ok(req) = recv.recv().await {
                            req.apply(&mut LineSetUnit(&set)).await;
                        }
                    }
                })
                .unwrap();
            let index = self.units.len();
            self.units.push(unit);
            (set, index)
        });
        (line_set, self.units[*index].handle())
    }
}

#[derive(InspectMut)]
#[inspect(transparent)]
struct LineSetUnit<'a>(&'a LineSet);

impl StateUnit for LineSetUnit<'_> {
    async fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn save(
        &mut self,
    ) -> Result<Option<vmcore::save_restore::SavedStateBlob>, vmcore::save_restore::SaveError> {
        Ok(None)
    }

    async fn restore(
        &mut self,
        _: vmcore::save_restore::SavedStateBlob,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        Err(vmcore::save_restore::RestoreError::SavedStateNotSupported)
    }
}

pub struct LineSetTargetDevice<T> {
    device: Arc<CloseableMutex<T>>,
}

impl<T: ChipsetDevice> LineSetTargetDevice<T> {
    pub fn new(device: Arc<CloseableMutex<T>>) -> Self {
        Self { device }
    }
}

impl<T: ChipsetDevice> LineSetTarget for LineSetTargetDevice<T> {
    fn set_irq(&self, vector: u32, high: bool) {
        self.device
            .lock()
            .supports_line_interrupt_target()
            .unwrap()
            .set_irq(vector, high);
    }
}
