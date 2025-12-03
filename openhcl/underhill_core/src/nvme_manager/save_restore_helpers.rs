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
    /// This excludes vCPUs that also had outstanding I/O at the time of save,
    /// which are counted in `vps_with_outstanding_io`.
    pub vps_with_mapped_interrupts_no_io: Vec<u32>,

    /// List of vCPUs with outstanding I/O at the time of save, sorted by CPU ID.
    pub vps_with_outstanding_io: Vec<u32>,
}

/// Analyzes the saved NVMe manager state to determine which vCPUs had mapped device interrupts
/// and which had outstanding I/O at the time of save.
///
/// See [`VPInterruptState`] for more details.
pub fn nvme_interrupt_state(state: Option<&NvmeManagerSavedState>) -> VPInterruptState {
    let mut vp_state = BTreeMap::new();

    if let Some(state) = state {
        for disk in &state.nvme_disks {
            for q in &disk.driver_state.worker_data.io {
                match vp_state.entry(q.cpu) {
                    Entry::Vacant(e) => {
                        e.insert(!q.queue_data.handler_data.pending_cmds.commands.is_empty());
                    }
                    Entry::Occupied(mut e) => {
                        *e.get_mut() |= !q.queue_data.handler_data.pending_cmds.commands.is_empty();
                    }
                }
            }
        }
    }

    let (vps_with_outstanding_io, vps_with_mapped_interrupts_no_io): (
        BTreeMap<u32, bool>,
        BTreeMap<u32, bool>,
    ) = vp_state
        .iter()
        .partition(|&(_, has_outstanding_io)| *has_outstanding_io);

    VPInterruptState {
        vps_with_mapped_interrupts_no_io: vps_with_mapped_interrupts_no_io
            .keys()
            .cloned()
            .collect(),
        vps_with_outstanding_io: vps_with_outstanding_io.keys().cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvme_manager::save_restore::{NvmeManagerSavedState, NvmeSavedDiskConfig};
    use nvme_driver::save_restore::{
        CompletionQueueSavedState, IoQueueSavedState, NvmeDriverSavedState,
        NvmeDriverWorkerSavedState, PendingCommandSavedState, PendingCommandsSavedState,
        QueueHandlerSavedState, QueuePairSavedState, SubmissionQueueSavedState,
    };
    use nvme_spec as spec;
    use zerocopy::FromZeros;

    #[test]
    fn returns_empty_when_state_absent() {
        let result = nvme_interrupt_state(None);
        assert!(result.vps_with_mapped_interrupts_no_io.is_empty());
        assert!(result.vps_with_outstanding_io.is_empty());
    }

    #[test]
    fn collects_unique_sorted_vps_and_outstanding_subset() {
        let state = build_state(vec![
            vec![QueueSpec::new(2, false), QueueSpec::new(1, true)],
            vec![QueueSpec::new(1, false), QueueSpec::new(3, true)],
            vec![QueueSpec::new(5, false), QueueSpec::new(2, false)],
        ]);

        let result = nvme_interrupt_state(Some(&state));

        assert_eq!(result.vps_with_mapped_interrupts_no_io, vec![2, 5]);
        assert_eq!(result.vps_with_outstanding_io, vec![1, 3]);
    }

    #[test]
    fn reports_outstanding_if_any_queue_pending_for_vp() {
        let state = build_state(vec![vec![
            QueueSpec::new(4, false),
            QueueSpec::new(4, true),
        ]]);

        let result = nvme_interrupt_state(Some(&state));

        assert_eq!(
            result.vps_with_mapped_interrupts_no_io,
            Vec::<u32>::from_iter([])
        );
        assert_eq!(result.vps_with_outstanding_io, vec![4]);
    }

    #[test]
    fn handles_state_with_no_disks() {
        let state = NvmeManagerSavedState {
            cpu_count: 0,
            nvme_disks: Vec::new(),
        };

        let result = nvme_interrupt_state(Some(&state));

        assert!(result.vps_with_mapped_interrupts_no_io.is_empty());
        assert!(result.vps_with_outstanding_io.is_empty());
    }

    struct QueueSpec {
        cpu: u32,
        has_outstanding_io: bool,
    }

    impl QueueSpec {
        const fn new(cpu: u32, has_outstanding_io: bool) -> Self {
            Self {
                cpu,
                has_outstanding_io,
            }
        }
    }

    // Helper to fabricate NVMe manager save-state snapshots with specific CPU/IO mappings.
    fn build_state(disk_queue_specs: Vec<Vec<QueueSpec>>) -> NvmeManagerSavedState {
        NvmeManagerSavedState {
            cpu_count: 0, // Not relevant for these tests.
            nvme_disks: disk_queue_specs
                .into_iter()
                .enumerate()
                .map(|(disk_index, queues)| NvmeSavedDiskConfig {
                    pci_id: format!("0000:{disk_index:02x}.0"),
                    driver_state: NvmeDriverSavedState {
                        identify_ctrl: spec::IdentifyController::new_zeroed(),
                        device_id: format!("disk{disk_index}"),
                        namespaces: Vec::new(),
                        worker_data: NvmeDriverWorkerSavedState {
                            admin: None,
                            io: queues
                                .into_iter()
                                .enumerate()
                                .map(|(queue_index, spec)| {
                                    // Tests only care about per-disk affinity, so queue IDs can
                                    // restart from zero for each disk without losing coverage.
                                    build_io_queue(
                                        queue_index as u16,
                                        spec.cpu,
                                        spec.has_outstanding_io,
                                    )
                                })
                                .collect(),
                            qsize: 0,
                            max_io_queues: 0,
                        },
                    },
                })
                .collect(),
        }
    }

    fn build_io_queue(qid: u16, cpu: u32, outstanding: bool) -> IoQueueSavedState {
        IoQueueSavedState {
            cpu,
            iv: qid as u32,
            queue_data: QueuePairSavedState {
                mem_len: 0,
                base_pfn: 0,
                qid,
                sq_entries: 1,
                cq_entries: 1,
                handler_data: QueueHandlerSavedState {
                    sq_state: SubmissionQueueSavedState {
                        sqid: qid,
                        head: 0,
                        tail: 0,
                        committed_tail: 0,
                        len: 1,
                    },
                    cq_state: CompletionQueueSavedState {
                        cqid: qid,
                        head: 0,
                        committed_head: 0,
                        len: 1,
                        phase: false,
                    },
                    pending_cmds: build_pending_cmds(outstanding),
                    aer_handler: None,
                },
            },
        }
    }

    fn build_pending_cmds(outstanding: bool) -> PendingCommandsSavedState {
        PendingCommandsSavedState {
            commands: if outstanding {
                vec![PendingCommandSavedState {
                    command: spec::Command::new_zeroed(),
                }]
            } else {
                Vec::new()
            },
            next_cid_high_bits: 0,
            cid_key_bits: 0,
        }
    }
}
