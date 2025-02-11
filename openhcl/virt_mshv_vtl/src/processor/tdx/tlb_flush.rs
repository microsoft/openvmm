// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLB flush implementation for TDX partitions.

use crate::TdxBacked;
use crate::UhProcessor;
use hcl::ioctl::tdx::Tdx;
use hcl::ioctl::ProcessorRunner;
use hcl::GuestVtl;
use hvdef::hypercall::HvGvaRange;
use inspect::Inspect;
use std::collections::VecDeque;
use std::num::Wrapping;
use x86defs::tdx::TdGlaVmAndFlags;
use x86defs::tdx::TdxGlaListInfo;
use x86defs::tdx::TdxVmFlags;
use zerocopy::IntoBytes;

pub(super) const FLUSH_GVA_LIST_SIZE: usize = 32;

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Inspect)]
pub(super) struct TdxPartitionFlushState {
    /// A fixed-size ring buffer of GVAs that need to be flushed.
    #[inspect(with = "|vd| inspect::iter_by_index(vd).map_value(|g| inspect::AsHex(g.0))")]
    pub(super) gva_list: VecDeque<HvGvaRange>,
    pub(super) s: TdxFlushState,
}

#[cfg(guest_arch = "x86_64")]
impl TdxPartitionFlushState {
    pub(super) fn new() -> Self {
        Self {
            gva_list: VecDeque::with_capacity(FLUSH_GVA_LIST_SIZE),
            s: TdxFlushState::new(),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Clone, Inspect)]
pub(super) struct TdxFlushState {
    /// On the partition, the number of GVAs that have been added over the lifetime of the VM.
    /// On a VP, the last observed value of the partition's counter.
    /// If the difference between the partition's value and a VP's value is greater
    /// than FLUSH_GVA_LIST_SIZE, then the VP has missed some GVAs and must flush
    /// the entire TLB.
    pub(super) gva_list_count: Wrapping<usize>,
    /// On the partition, the number of times an entire TLB flush has been requested.
    /// On a VP, the last observed value of the partition's counter.
    /// If the values differ, the VP must flush the entire TLB.
    pub(super) flush_entire_counter: Wrapping<u32>,
    /// On the partition, the number of times a non-global TLB flush has been requested.
    /// On a VP, the last observed value of the partition's counter.
    /// If the values differ, the VP must flush the non-global portion of the TLB.
    pub(super) flush_entire_non_global_counter: Wrapping<u32>,
}

#[cfg(guest_arch = "x86_64")]
impl TdxFlushState {
    pub(super) fn new() -> Self {
        Self {
            gva_list_count: Wrapping(0),
            flush_entire_counter: Wrapping(0),
            flush_entire_non_global_counter: Wrapping(0),
        }
    }
}

impl UhProcessor<'_, TdxBacked> {
    /// Completes any pending TLB flush activity on the current VP.
    pub(super) fn do_tlb_flush(&mut self, target_vtl: GuestVtl) {
        let partition_flush_state = self.shared.flush_state[target_vtl].read();
        let self_flush_state = &mut self.backing.vtls[target_vtl].flush_state;

        // NOTE: It is theoretically possible that we haven't run in so long
        // that the partition counters have wrapped all the way around u32::MAX
        // and are back to our current values. However this is so extremely
        // unlikely that we don't bother to worry about it.

        // Check first to see whether a full flush is required.
        let flush_entire_required = if self_flush_state.flush_entire_counter
            != partition_flush_state.s.flush_entire_counter
        {
            true
        }
        // Attempt to perform a flush by list and promote to flush entire if required.
        else {
            !Self::try_flush_list(
                target_vtl,
                &partition_flush_state,
                &mut self_flush_state.gva_list_count,
                &mut self.runner,
                &self.backing.flush_page,
            )
        };

        // If a flush entire is required, then complete the flush and update the
        // flush counters to indicate that a complete flush has been accomplished.
        if flush_entire_required {
            *self_flush_state = partition_flush_state.s.clone();
            Self::set_flush_entire(
                true,
                &mut self.backing.vtls[target_vtl].private_regs.vp_entry_flags,
            );
        }
        // If no flush entire is required, then check to see whether a full
        // non-global flush is required.
        else if self_flush_state.flush_entire_non_global_counter
            != partition_flush_state.s.flush_entire_non_global_counter
        {
            self_flush_state.flush_entire_non_global_counter =
                partition_flush_state.s.flush_entire_non_global_counter;
            Self::set_flush_entire(
                false,
                &mut self.backing.vtls[target_vtl].private_regs.vp_entry_flags,
            );
        }
    }

    /// Performs any TLB flush by list that may be required. Returns true
    /// if successful, false if a flush entire is required instead.
    fn try_flush_list(
        target_vtl: GuestVtl,
        partition_flush_state: &TdxPartitionFlushState,
        gva_list_count: &mut Wrapping<usize>,
        runner: &mut ProcessorRunner<'_, Tdx>,
        flush_page: &page_pool_alloc::PagePoolHandle,
    ) -> bool {
        // Check quickly to see whether any new addresses are in the list.
        if partition_flush_state.s.gva_list_count == *gva_list_count {
            return true;
        }

        // If the list has overflowed, then a flush entire is required.
        let count_diff = (partition_flush_state.s.gva_list_count - *gva_list_count).0;
        if count_diff > FLUSH_GVA_LIST_SIZE {
            return false;
        }

        // The last `count_diff` addresses are the new ones.
        let mut flush_addrs = partition_flush_state
            .gva_list
            .range(partition_flush_state.gva_list.len() - count_diff..);

        // Now we can build the TDX structs and actually call INVGLA.
        tracing::trace!(count = count_diff, ?target_vtl, "flushing TLB by list");
        let mut gla_flags = TdGlaVmAndFlags::new().with_vm_index(target_vtl as u64 + 1);

        if count_diff == 1 {
            let gva_range = flush_addrs.next().unwrap();
            runner
                .invgla(gla_flags, TdxGlaListInfo::from(gva_range.0))
                .unwrap();
        } else {
            gla_flags.set_list(true);

            let page_mapping = flush_page.mapping().unwrap();

            for (i, gva_range) in flush_addrs.enumerate() {
                page_mapping
                    .write_at(i * size_of::<HvGvaRange>(), gva_range.as_bytes())
                    .unwrap();
            }

            let gla_list = TdxGlaListInfo::new()
                .with_list_gpa(flush_page.base_pfn())
                .with_num_entries(count_diff as u64);
            runner.invgla(gla_flags, gla_list).unwrap();
        };

        *gva_list_count = partition_flush_state.s.gva_list_count;
        true
    }

    fn set_flush_entire(global: bool, vp_flags: &mut TdxVmFlags) {
        if global {
            // TODO: Track EPT invalidations separately.
            vp_flags.set_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVEPT);
        } else if !global && vp_flags.invd_translations() == 0 {
            vp_flags.set_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVVPID_NON_GLOBAL);
        }
    }
}
