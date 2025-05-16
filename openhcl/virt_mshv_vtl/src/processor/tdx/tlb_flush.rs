// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLB flush implementation for TDX partitions.

use crate::TdxBacked;
use crate::UhProcessor;
use hcl::GuestVtl;
use hcl::ioctl::ProcessorRunner;
use hcl::ioctl::tdx::Tdx;
use hvdef::hypercall::HvGvaRange;
use inspect::Inspect;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use safeatomic::AtomicSliceOps;
use std::num::Wrapping;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use x86defs::tdx::TdGlaVmAndFlags;
use x86defs::tdx::TdxGlaListInfo;
use x86defs::tdx::TdxVmFlags;
use zerocopy::IntoBytes;

pub(super) const FLUSH_GVA_LIST_SIZE: usize = 32;

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Inspect)]
pub(super) struct TdxPartitionFlushState {
    /// A fixed-size ring buffer of GVAs that need to be flushed.
    pub(super) gva_list: AtomicTlbRingBuffer,
    /// The number of times an entire TLB flush has been requested.
    pub(super) flush_entire_counter: AtomicU32,
    /// The number of times a non-global TLB flush has been requested.
    pub(super) flush_entire_non_global_counter: AtomicU32,
}

#[cfg(guest_arch = "x86_64")]
impl TdxPartitionFlushState {
    pub(super) fn new() -> Self {
        Self {
            gva_list: AtomicTlbRingBuffer::new(),
            flush_entire_counter: AtomicU32::new(0),
            flush_entire_non_global_counter: AtomicU32::new(0),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
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
            self_flush_state.gva_list_count = Wrapping(partition_flush_state.gva_list.count());
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
        let partition_list_count = Wrapping(partition_flush_state.gva_list.count());
        if partition_list_count == *gva_list_count {
            return true;
        }

        // If the list has overflowed, then a flush entire is required.
        let count_diff = (partition_list_count - *gva_list_count).0;
        if count_diff > FLUSH_GVA_LIST_SIZE {
            return false;
        }

        // The last `count_diff` addresses are the new ones, copy them locally.
        let flush_addrs = &mut [0; FLUSH_GVA_LIST_SIZE][..count_diff];
        if !partition_flush_state
            .gva_list
            .try_copy(*gva_list_count, flush_addrs)
        {
            return false;
        }

        // Now we can build the TDX structs and actually call INVGLA.
        tracing::trace!(count = count_diff, ?target_vtl, "flushing TLB by list");
        let mut gla_flags = TdGlaVmAndFlags::new().with_vm_index(target_vtl as u64 + 1);

        if count_diff == 1 {
            runner
                .invgla(gla_flags, TdxGlaListInfo::from(flush_addrs[0]))
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

#[derive(Debug, Inspect)]
pub(super) struct AtomicTlbRingBuffer {
    /// The contents of the buffer.
    #[inspect(hex, with = "|x| inspect::iter_by_index(x.iter())")]
    buffer: Box<[AtomicU64; FLUSH_GVA_LIST_SIZE]>,
    /// The number of GVAs that have been added over the lifetime of the VM.
    gva_list_count: AtomicUsize,
    /// The number of GVAs that have started being added to the list over the
    /// lifetime of the VM.
    in_progress_count: AtomicUsize,
    /// A guard to ensure that only one thread is writing to the list at a time.
    write_lock: Mutex<()>,
}

pub(super) struct AtomicTlbRingBufferWriteGuard<'a> {
    buf: &'a AtomicTlbRingBuffer,
    _write_lock: MutexGuard<'a, ()>,
}

impl AtomicTlbRingBuffer {
    fn new() -> Self {
        Self {
            buffer: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
            gva_list_count: AtomicUsize::new(0),
            in_progress_count: AtomicUsize::new(0),
            write_lock: Mutex::new(()),
        }
    }

    fn count(&self) -> usize {
        self.gva_list_count.load(Ordering::Relaxed)
    }

    pub fn write(&self) -> AtomicTlbRingBufferWriteGuard<'_> {
        let write_lock = self.write_lock.lock();
        AtomicTlbRingBufferWriteGuard {
            buf: self,
            _write_lock: write_lock,
        }
    }

    fn try_copy(&self, start_count: Wrapping<usize>, flush_addrs: &mut [u64]) -> bool {
        let mut index = start_count;
        for flush_addr in flush_addrs.iter_mut() {
            *flush_addr = self.buffer[index.0 % FLUSH_GVA_LIST_SIZE].load(Ordering::Relaxed);
            index += 1;
        }

        // Check to see whether any additional entries have been added
        // that would have caused a wraparound. If so, the local list is
        // incomplete and the copy has failed.
        if (Wrapping(self.in_progress_count.load(Ordering::Relaxed)) - start_count).0
            > FLUSH_GVA_LIST_SIZE
        {
            return false;
        }
        true
    }
}

impl AtomicTlbRingBufferWriteGuard<'_> {
    pub fn extend(&self, items: impl ExactSizeIterator<Item = u64>) {
        debug_assert_eq!(
            self.buf.in_progress_count.load(Ordering::Relaxed),
            self.buf.gva_list_count.load(Ordering::Relaxed)
        );
        // Adding a new item to the buffer must be done in three steps:
        // 1. Indicate that an entry is about to be added so that any flush
        //    code executing simultaneously will know that it might lose an
        //    entry that it is expecting to see.
        // 2. Add the entry.
        // 3. Increment the valid entry count so that any flush code executing
        //    simultaneously will know it is valid.
        let len = items.len();
        let count = self.buf.in_progress_count.fetch_add(len, Ordering::Relaxed);
        for (i, v) in items.enumerate() {
            self.buf.buffer[(count + i) % FLUSH_GVA_LIST_SIZE].store(v, Ordering::Relaxed);
        }
        self.buf.gva_list_count.fetch_add(len, Ordering::Relaxed);
    }
}
