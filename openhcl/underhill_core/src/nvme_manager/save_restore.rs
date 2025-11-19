// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh::payload::Protobuf;
use std::collections::HashSet;
use vmcore::save_restore::SavedStateRoot;

#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "underhill")]
pub struct NvmeManagerSavedState {
    #[mesh(1)]
    pub cpu_count: u32,
    #[mesh(2)]
    pub nvme_disks: Vec<NvmeSavedDiskConfig>,
}

#[derive(Protobuf, Clone)]
#[mesh(package = "underhill")]
pub struct NvmeSavedDiskConfig {
    #[mesh(1)]
    pub pci_id: String,
    #[mesh(2)]
    pub driver_state: nvme_driver::NvmeDriverSavedState,
}

pub fn cpus_with_interrupts(state: &NvmeManagerSavedState) -> Vec<u32> {
    let mut cpus_with_interrupts = HashSet::new();
    for disk in &state.nvme_disks {
        cpus_with_interrupts.extend(disk.driver_state.worker_data.io.iter().map(|q| q.cpu));
    }
    cpus_with_interrupts.into_iter().collect()
}
