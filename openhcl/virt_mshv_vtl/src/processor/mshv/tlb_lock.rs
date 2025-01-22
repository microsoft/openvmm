// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLB lock infrastructure support for Microsoft hypervisor-backed partitions.

use crate::HypervisorBacked;
use crate::UhProcessor;
use hcl::GuestVtl;
use hvdef::hypercall::HvInputVtl;
use hvdef::HvAllArchRegisterName;
use hvdef::Vtl;

impl UhProcessor<'_, HypervisorBacked> {
    /// Causes the specified VTL on the current VP to wait on all TLB locks.
    /// This is typically used to synchronize VTL permission changes with
    /// concurrent instruction emulation.
    pub fn set_wait_for_tlb_locks(&mut self, target_vtl: Vtl) {
        let reg = [(
            HvAllArchRegisterName::VsmVpWaitForTlbLock,
            u64::from(hvdef::HvRegisterVsmWpWaitForTlbLock::new().with_wait(true)),
        )];
        self.runner
            .set_vp_registers_hvcall(target_vtl, reg)
            .expect("set_vp_registers hypercall for waiting for tlb lock should not fail");
    }

    /// Lock the TLB of the target VTL on the current VP.
    pub fn set_tlb_lock(&mut self, requesting_vtl: Vtl, target_vtl: GuestVtl) {
        debug_assert_eq!(requesting_vtl, Vtl::Vtl2);

        if self.is_tlb_locked(requesting_vtl, target_vtl) {
            return;
        }

        let reg = [(
            HvAllArchRegisterName(
                HvAllArchRegisterName::VsmVpSecureConfigVtl0.0 + target_vtl as u32,
            ),
            u64::from(hvdef::HvRegisterVsmVpSecureVtlConfig::new().with_tlb_locked(true)),
        )];
        self.runner
            .set_vp_registers_hvcall(requesting_vtl, reg)
            .expect("set_vp_registers hypercall for setting tlb lock should not fail");

        self.vtls_tlb_locked.set(requesting_vtl, target_vtl, true);
    }

    /// Check the status of the TLB lock of the target VTL on the current VP.
    pub fn is_tlb_locked(&mut self, requesting_vtl: Vtl, target_vtl: GuestVtl) -> bool {
        debug_assert_eq!(requesting_vtl, Vtl::Vtl2);
        let local_status = self.vtls_tlb_locked.get(requesting_vtl, target_vtl);
        // The hypervisor may lock the TLB without us knowing, but the inverse should never happen.
        if local_status {
            debug_assert!(self.is_tlb_locked_in_hypervisor(target_vtl))
        };
        local_status
    }

    fn is_tlb_locked_in_hypervisor(&self, target_vtl: GuestVtl) -> bool {
        let name = HvAllArchRegisterName(
            HvAllArchRegisterName::VsmVpSecureConfigVtl0.0 + target_vtl as u32,
        );
        let result = self
            .partition
            .hcl
            .get_vp_register(name, HvInputVtl::CURRENT_VTL)
            .expect("failure is a misconfiguration");
        let config = hvdef::HvRegisterVsmVpSecureVtlConfig::from(result.as_u64());
        config.tlb_locked()
    }

    /// Marks the TLBs of all lower VTLs as unlocked.
    /// The hypervisor does the actual unlocking required upon VTL exit.
    pub fn unlock_tlb_lock(&mut self, unlocking_vtl: Vtl) {
        debug_assert_eq!(unlocking_vtl, Vtl::Vtl2);
        self.vtls_tlb_locked.fill(unlocking_vtl, false);
    }
}
