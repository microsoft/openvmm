// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::nvme_manager::save_restore::NvmeManagerSavedState;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// Useful state about how the VM's vCPUs interacted with NVMe device interrupts at the time of save.
///
/// This information is used to make heuristic decisions during restore, such as whether to
/// disable sidecar for VMs with active device interrupts.
pub struct VPInterruptState {
    /// List of vCPUs with any mapped device interrupts, sorted by CPU ID.
    pub vps_with_mapped_interrupts: Vec<u32>,

    /// List of vCPUs with outstanding I/O at the time of save, sorted by CPU ID.
    /// It is expected that this is a subset of `vps_with_mapped_interrupts`, since
    /// only some queues will have in-flight I/O.
    pub vps_with_outstanding_io: Vec<u32>,
}

/// Analyzes the saved NVMe manager state to determine which vCPUs had mapped device interrupts
/// and which had outstanding I/O at the time of save.
///
/// See [`VPInterruptState`] for more details.
pub fn cpus_with_interrupts(state: Option<&NvmeManagerSavedState>) -> VPInterruptState {
    let mut vp_state = BTreeMap::new();

    if let Some(state) = state {
        for disk in &state.nvme_disks {
            for q in &disk.driver_state.worker_data.io {
                match vp_state.entry(q.cpu) {
                    Entry::Vacant(e) => {
                        e.insert(!q.queue_data.handler_data.pending_cmds.commands.is_empty());
                    }
                    Entry::Occupied(mut e) => {
                        e.insert(
                            e.get() | !q.queue_data.handler_data.pending_cmds.commands.is_empty(),
                        );
                    }
                }
            }
        }
    }

    VPInterruptState {
        vps_with_mapped_interrupts: vp_state.keys().cloned().collect(),
        vps_with_outstanding_io: vp_state
            .iter()
            .filter_map(
                |(&vp, &has_outstanding_io)| {
                    if has_outstanding_io { Some(vp) } else { None }
                },
            )
            .collect(),
    }
}
