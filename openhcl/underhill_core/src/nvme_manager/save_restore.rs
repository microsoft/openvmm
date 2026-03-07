// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh::payload::Protobuf;
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
    pub driver_state: nvme_driver::save_restore::NvmeDriverSavedState,
}
