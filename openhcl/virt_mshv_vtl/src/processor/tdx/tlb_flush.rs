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

        // NOTE: It is theoretically possible that we haven't run in so long that the
        // partition counters have wrapped all the way around and are back to
        // our current values. However this is so extremely unlikely that we don't
        // bother to worry about it.

        // Check first to see whether a full flush is required.
        let flush_entire_required = if self.backing.flush_state[target_vtl].flush_entire_counter
            != partition_flush_state.s.flush_entire_counter
        {
            true
        }
        // Attempt to perform a flush by list and promote to flush entire if required.
        else {
            !Self::try_flush_list(
                target_vtl,
                &partition_flush_state,
                &mut self.backing.flush_state[target_vtl].gva_list_count,
                &mut self.runner,
                &self.backing.flush_page,
            )
        };

        let self_flush_state = &mut self.backing.flush_state[target_vtl];

        // If a flush entire is required, then complete the flush and update the
        // flush counters to indicate that a complete flush has been accomplished.
        if flush_entire_required {
            *self_flush_state = partition_flush_state.s.clone();
            Self::do_flush_entire(false, &mut self.runner);
        }
        // If no flush entire is required, then check to see whether a full
        // non-global flush is required.
        else if self_flush_state.flush_entire_non_global_counter
            != partition_flush_state.s.flush_entire_non_global_counter
        {
            self_flush_state.flush_entire_non_global_counter =
                partition_flush_state.s.flush_entire_non_global_counter;
            Self::do_flush_entire(true, &mut self.runner);
        }
    }

    /// Performs any TLB flush by list that may be required. Returns true
    /// if successful, false if a flush entire is required instead.
    fn try_flush_list(
        target_vtl: GuestVtl,
        partition_flush_state: &TdxPartitionFlushState,
        gva_list_count: &mut Wrapping<usize>,
        runner: &mut ProcessorRunner<'_, Tdx>,
        flush_page: &shared_pool_alloc::SharedPoolHandle,
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
        // TODO: don't double copy?
        let flush_addrs: Vec<_> = partition_flush_state
            .gva_list
            .range(partition_flush_state.gva_list.len() - count_diff..)
            .copied()
            .collect();

        // Any extended entry can't be handled, promote to a flush entire.
        if flush_addrs.iter().any(|a| a.as_extended().large_page()) {
            return false;
        }

        *gva_list_count = partition_flush_state.s.gva_list_count;
        Self::do_flush_list(target_vtl, &flush_addrs, runner, flush_page);

        true
    }

    fn do_flush_list(
        target_vtl: GuestVtl,
        flush_addrs: &[HvGvaRange],
        runner: &mut ProcessorRunner<'_, Tdx>,
        flush_page: &shared_pool_alloc::SharedPoolHandle,
    ) {
        // Now we can build the TDX structs and actually call INVGLA.
        tracing::trace!(
            count = flush_addrs.len(),
            ?target_vtl,
            "flushing TLB by list"
        );
        let mut gla_flags = TdGlaVmAndFlags::new().with_vm_index(target_vtl as u64 + 1);

        if flush_addrs.len() == 1 {
            runner
                .invgla(gla_flags, TdxGlaListInfo::from(flush_addrs[0].0))
                .expect("should never fail");
        } else {
            gla_flags.set_list(true);

            // TODO: Actually copy addresses in.
            // let page_mapping = flush_page.sparse_mapping().expect("allocated");

            // for (i, gva_range) in flush_addrs.iter().enumerate() {
            //     page_mapping
            //         .write_at(i * size_of::<HvGvaRange>(), gva_range.as_bytes())
            //         .expect("just allocated, should never fail");
            // }

            let gla_list = TdxGlaListInfo::new()
                .with_list_gpa(flush_page.base_pfn())
                .with_num_entries(flush_addrs.len() as u64);
            runner
                .invgla(gla_flags, gla_list)
                .expect("should never fail");
        };
    }

    fn do_flush_entire(non_global: bool, runner: &mut ProcessorRunner<'_, Tdx>) {
        let vp_flags = runner.tdx_vp_entry_flags_mut();

        if !non_global {
            // TODO: Track EPT invalidations separately.
            vp_flags.set_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVEPT);
        } else if non_global && vp_flags.invd_translations() == 0 {
            vp_flags.set_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVVPID_NON_GLOBAL);
        }
    }
}
