// Copyright (C) Microsoft Corporation. All rights reserved.

//! TLB lock infrastructure support for hardware-isolated partitions.

use crate::HardwareIsolatedBacking;
use crate::UhProcessor;
use hcl::GuestVtl;
use hvdef::Vtl;
use std::sync::atomic::Ordering;

impl<'a, B: HardwareIsolatedBacking> UhProcessor<'a, B> {
    /// Causes the specified VTL on the current VP to wait on all TLB locks.
    /// This is typically used to synchronize VTL permission changes with
    /// concurrent instruction emulation.
    pub fn set_wait_for_tlb_locks(&mut self, target_vtl: GuestVtl) {
        // Capture the set of VPs that are currently holding the TLB lock. Only
        // those VPs that hold the lock at this point can block progress, because
        // any VP that acquires the lock after this point is guaranteed to see
        // state that this VP has already flushed.
        let self_index = self.vp_index().index() as usize;
        let self_lock = &self.inner.tlb_lock_info[target_vtl];
        for vp in self.partition.cvm.as_ref().unwrap().tlb_locked_vps[target_vtl]
            .clone()
            .iter_ones()
        {
            // Never wait on the current VP, since the current VP will always
            // release its locks correctly when returning to the target VTL.
            if vp == self_index {
                continue;
            }

            // First record that this VP will be waiting on the target VP.
            // Because the lock may already have been requested by a lower VTL,
            // the current VP may already be waiting for this target, and if so,
            // it should not count as an additional blocking VP.
            if self_lock.blocking_vps.set_aliased(vp, true) {
                continue;
            }
            self_lock.blocking_vp_count.fetch_add(1, Ordering::Relaxed);

            // Now advise the target VP that it is blocking this VP.
            // Because the wait by the current VP on the target VP is known to
            // be new, this bit should not already be set.
            let other_lock_blocked = &self.partition.vps[vp].tlb_lock_info[target_vtl].blocked_vps;
            let _was_other_lock_blocked = other_lock_blocked.set_aliased(self_index, true);
            debug_assert!(!_was_other_lock_blocked);

            // It is possible that the target VP released the TLB lock before
            // the current VP was added to its blocked set. Check again to
            // see whether the TLB lock is still held, and if not, remove the
            // block.
            if !self.partition.cvm.as_ref().unwrap().tlb_locked_vps[target_vtl][vp] {
                other_lock_blocked.set_aliased(self_index, false);
                if self_lock.blocking_vps.set_aliased(vp, false) {
                    self_lock.blocking_vp_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }

        // Mark the target VTL as waiting for TLB locks.
        self.vtls_tlb_waiting[target_vtl] = true;
    }

    /// Lock the TLB of the target VTL on the current VP.
    pub fn set_tlb_lock(&mut self, requesting_vtl: Vtl, target_vtl: GuestVtl) {
        debug_assert!(requesting_vtl > Vtl::from(target_vtl));

        self.partition.cvm.as_ref().unwrap().tlb_locked_vps[target_vtl]
            .set_aliased(self.vp_index().index() as usize, true);
        self.vtls_tlb_locked.set(requesting_vtl, target_vtl, true);
    }

    /// Unlocks the TLBs of all lower VTLs as required upon VTL exit.
    pub fn unlock_tlb_lock(&mut self, unlocking_vtl: Vtl) {
        debug_assert!(unlocking_vtl != Vtl::Vtl0);
        let self_index = self.vp_index().index() as usize;
        for &target_vtl in &[GuestVtl::Vtl1, GuestVtl::Vtl0][(2 - unlocking_vtl as usize)..] {
            // If this VP hasn't taken a lock, no need to do anything.
            if self.vtls_tlb_locked.get(unlocking_vtl, target_vtl) {
                self.vtls_tlb_locked.set(unlocking_vtl, target_vtl, false);
                // A memory fence is required after indicating that the target VTL is no
                // longer locked, because other VPs will make decisions about how to
                // handle blocking based on this information, and the loop below relies on
                // those processors having an accurate view of the lock state.
                std::sync::atomic::fence(Ordering::SeqCst);

                // If the lock for VTL 0 is being released by VTL 2, then check
                // to see whether VTL 1 also holds a lock for VTL 0. If so, no
                // wait can be unblocked until VTL 1 also releases its lock.
                if unlocking_vtl == Vtl::Vtl2
                    && target_vtl == GuestVtl::Vtl0
                    && self.vtls_tlb_locked.get(Vtl::Vtl1, GuestVtl::Vtl0)
                {
                    return;
                }

                // Now we can remove ourselves from the global TLB lock.
                self.partition.cvm.as_ref().unwrap().tlb_locked_vps[target_vtl]
                    .set_aliased(self_index, false);

                // Check to see whether any other VPs are waiting for this VP to release
                // the TLB lock. Note that other processors may be in the process of
                // inserting themselves into this set because they may have observed that
                // the TLB lock was still held on the current processor, but they will
                // take responsibility for removing themselves after insertion because
                // they will once again observe the TLB lock as not held. Because the set
                // of blocked VPs may be changing, it must be captured locally, since the
                // VP set scan below cannot safely be performed on a VP set that may be
                // changing.
                for blocked_vp in self.inner.tlb_lock_info[target_vtl]
                    .blocked_vps
                    .clone()
                    .iter_ones()
                {
                    self.inner.tlb_lock_info[target_vtl]
                        .blocked_vps
                        .set_aliased(blocked_vp, false);

                    // Mark the target VP as no longer blocked by the current VP.
                    // Note that the target VP may have already marked itself as not
                    // blocked if is has already noticed that the lock has already
                    // been released on the current VP.
                    let other_lock = &self.partition.vps[blocked_vp].tlb_lock_info[target_vtl];
                    if other_lock.blocking_vps.set_aliased(self_index, false) {
                        let other_old_count =
                            other_lock.blocking_vp_count.fetch_sub(1, Ordering::Relaxed);

                        if other_old_count == 1 {
                            // The current VP was the last one to be removed from the
                            // blocking set of the target VP. If it is asleep, it must
                            // be woken now. Sending an IPI is sufficient to cause it to
                            // reevaluate the blocking state. It is not necessary to
                            // synchronize with its sleep state as a spurious IPI is not
                            // harmful.
                            if other_lock.sleeping.load(Ordering::SeqCst) {
                                self.partition.vps[blocked_vp].wake_vtl2();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Returns whether the VP should halt to wait for the TLB lock of the specified VTL.
    pub fn should_halt_for_tlb_unlock(&mut self, target_vtl: GuestVtl) -> bool {
        // No wait is required if this VP is not blocked on the TLB lock.
        if self.vtls_tlb_waiting[target_vtl] {
            // No wait is required unless this VP is blocked on another VP that
            // holds the TLB flush lock.
            let self_lock = &self.inner.tlb_lock_info[target_vtl];
            if self_lock.blocking_vp_count.load(Ordering::Relaxed) != 0 {
                self_lock.sleeping.store(true, Ordering::Relaxed);
                // Now that this VP has been marked as sleeping, check to see
                // whether it is still blocked. If not, no sleep should be
                // attempted.
                if self_lock.blocking_vp_count.load(Ordering::SeqCst) != 0 {
                    return true;
                }

                self_lock.sleeping.store(false, Ordering::Relaxed);
            }
            self.vtls_tlb_waiting[target_vtl] = false;
        } else {
            debug_assert_eq!(
                self.inner.tlb_lock_info[target_vtl]
                    .blocking_vp_count
                    .load(Ordering::Relaxed),
                0
            );
        }

        false
    }
}
