// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLB flush implementation for TDX partitions.

use crate::TdxBacked;
use crate::UhProcessor;
use atomic_ringbuf::AtomicRingBuffer;
use hcl::GuestVtl;
use hcl::ioctl::ProcessorRunner;
use hcl::ioctl::tdx::Tdx;
use hvdef::hypercall::HvGvaRange;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use std::num::Wrapping;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use x86defs::tdx::TdGlaVmAndFlags;
use x86defs::tdx::TdxGlaListInfo;
use x86defs::tdx::TdxVmFlags;
use zerocopy::IntoBytes;

pub(super) const FLUSH_GVA_LIST_SIZE: usize = 32;

#[derive(Debug, Inspect)]
pub(super) struct TdxPartitionFlushState {
    /// A fixed-size ring buffer of GVAs that need to be flushed.
    pub(super) gva_list: AtomicRingBuffer<FLUSH_GVA_LIST_SIZE, HvGvaRange>,
    /// The number of times an entire TLB flush has been requested.
    pub(super) flush_entire_counter: AtomicU32,
    /// The number of times a non-global TLB flush has been requested.
    pub(super) flush_entire_non_global_counter: AtomicU32,
}

impl TdxPartitionFlushState {
    pub(super) fn new() -> Self {
        Self {
            gva_list: AtomicRingBuffer::new(),
            flush_entire_counter: AtomicU32::new(0),
            flush_entire_non_global_counter: AtomicU32::new(0),
        }
    }
}

#[derive(Debug, Inspect)]
pub(super) struct TdxFlushState {
    /// The last observed value of the partition's counter.
    /// If the difference between the partition's value and a VP's value is greater
    /// than [`FLUSH_GVA_LIST_SIZE`]`, then the VP has missed some GVAs and must flush
    /// the entire TLB.
    gva_list_count: Wrapping<usize>,
    /// The last observed value of the partition's counter.
    /// If the values differ, the VP must flush the entire TLB.
    flush_entire_counter: Wrapping<u32>,
    /// The last observed value of the partition's counter.
    /// If the values differ, the VP must flush the non-global portion of the TLB.
    flush_entire_non_global_counter: Wrapping<u32>,
}

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
        let partition_flush_state = &self.shared.flush_state[target_vtl];
        let self_flush_state = &mut self.backing.vtls[target_vtl].flush_state;

        // NOTE: It is theoretically possible that we haven't run in so long
        // that the partition counters have wrapped all the way around u32::MAX
        // and are back to our current values. However this is so extremely
        // unlikely that we don't bother to worry about it.

        // Check first to see whether a full flush is required.
        let partition_flush_entire = partition_flush_state
            .flush_entire_counter
            .load(Ordering::Relaxed);
        let flush_entire_required =
            if partition_flush_entire != self_flush_state.flush_entire_counter.0 {
                true
            }
            // Attempt to perform a flush by list and promote to flush entire if required.
            else {
                !Self::try_flush_list(
                    target_vtl,
                    partition_flush_state,
                    &mut self_flush_state.gva_list_count,
                    &mut self.runner,
                    &self.backing.flush_page,
                )
            };

        // If a flush entire is required, then complete the flush and update the
        // flush counters to indicate that a complete flush has been accomplished.
        let partition_flush_non_global = partition_flush_state
            .flush_entire_non_global_counter
            .load(Ordering::Relaxed);
        if flush_entire_required {
            self_flush_state.flush_entire_counter = Wrapping(partition_flush_entire);
            self_flush_state.flush_entire_non_global_counter = Wrapping(partition_flush_non_global);
            self_flush_state.gva_list_count = partition_flush_state.gva_list.count();
            Self::set_flush_entire(
                true,
                &mut self.backing.vtls[target_vtl].private_regs.vp_entry_flags,
            );
        }
        // If no flush entire is required, then check to see whether a full
        // non-global flush is required.
        else if self_flush_state.flush_entire_non_global_counter.0 != partition_flush_non_global {
            self_flush_state.flush_entire_non_global_counter = Wrapping(partition_flush_non_global);
            Self::set_flush_entire(
                false,
                &mut self.backing.vtls[target_vtl].private_regs.vp_entry_flags,
            );
        }
    }

    /// Performs any TLB flush by list that may be required. Returns true
    /// if successful, false if a flush entire is required instead.
    fn try_flush_list<'a>(
        target_vtl: GuestVtl,
        partition_flush_state: &TdxPartitionFlushState,
        gva_list_count: &mut Wrapping<usize>,
        runner: &mut ProcessorRunner<'a, Tdx<'a>>,
        flush_page: &user_driver::memory::MemoryBlock,
    ) -> bool {
        // Check quickly to see whether any new addresses are in the list.
        let partition_list_count = partition_flush_state.gva_list.count();
        if partition_list_count == *gva_list_count {
            return true;
        }

        // If the list has overflowed, then a flush entire is required.
        let count_diff = (partition_list_count - *gva_list_count).0;
        if count_diff > FLUSH_GVA_LIST_SIZE {
            return false;
        }

        // The last `count_diff` addresses are the new ones, copy them locally.
        let flush_addrs = &mut [HvGvaRange(0); FLUSH_GVA_LIST_SIZE][..count_diff];
        if !partition_flush_state
            .gva_list
            .try_copy(gva_list_count.0, flush_addrs)
        {
            return false;
        }

        // Now we can build the TDX structs and actually call INVGLA.
        tracing::trace!(count = count_diff, ?target_vtl, "flushing TLB by list");
        let mut gla_flags = TdGlaVmAndFlags::new().with_vm_index(target_vtl as u64 + 1);

        if count_diff == 1 {
            runner
                .invgla(gla_flags, TdxGlaListInfo::from(flush_addrs[0].0))
                .unwrap();
        } else {
            gla_flags.set_list(true);

            let page_mapping = flush_page.as_slice();

            for (d, s) in page_mapping
                .chunks(size_of::<HvGvaRange>())
                .zip(flush_addrs)
            {
                d.atomic_write(s.as_bytes());
            }

            let gla_list = TdxGlaListInfo::new()
                .with_list_gpa(flush_page.pfns()[0])
                .with_num_entries(count_diff as u64);
            runner.invgla(gla_flags, gla_list).unwrap();
        };

        *gva_list_count = partition_list_count;
        true
    }

    fn set_flush_entire(global: bool, vp_flags: &mut TdxVmFlags) {
        if global {
            // TODO TDX: Track EPT invalidations separately.
            vp_flags.set_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVEPT);
        } else if !global && vp_flags.invd_translations() == 0 {
            vp_flags.set_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVVPID_NON_GLOBAL);
        }
    }
}
