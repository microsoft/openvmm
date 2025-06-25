// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for Microsoft hypervisor-backed partitions.

use crate::HypervisorBacked;
use crate::UhProcessor;
use hcl::GuestVtl;

pub mod arm64;
mod tlb_lock;
pub mod x64;

#[derive(Default, inspect::Inspect)]
pub(crate) struct VbsIsolatedVtl1State {
    #[inspect(hex, with = "|flags| flags.map(u32::from)")]
    default_vtl_protections: Option<hvdef::HvMapGpaFlags>,
    enable_vtl_protection: bool,
}

impl UhProcessor<'_, HypervisorBacked> {
    fn deliver_synic_messages(&mut self, vtl: GuestVtl, sints: u16) {
        let pending_sints =
            self.inner.message_queues[vtl].post_pending_messages(sints, |sint, message| {
                self.partition.hcl.post_message_direct(
                    self.inner.vp_info.base.vp_index.index(),
                    sint,
                    message,
                )
            });

        self.request_sint_notifications(vtl, pending_sints);
    }
}
