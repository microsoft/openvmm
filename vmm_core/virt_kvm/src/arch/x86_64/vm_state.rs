// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::KvmError;
use crate::KvmPartition;
use virt::VpIndex;
use virt::state::HvRegisterState;
use virt::x86::vm;
use virt::x86::vm::AccessVmState;

impl AccessVmState for &'_ KvmPartition {
    type Error = KvmError;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn hypercall(&mut self) -> Result<vm::HypercallMsrs, Self::Error> {
        super::vp_state::get_msrs_state(&self.inner.vp_kvm(VpIndex::BSP))
    }

    fn set_hypercall(&mut self, value: &vm::HypercallMsrs) -> Result<(), Self::Error> {
        // Work around a KVM bug that prevents setting the hypercall value when
        // the guest OS ID is not set.
        assert_eq!(value.names().len(), 2);
        self.inner.vp_kvm(VpIndex::BSP).set_msrs(&[
            (hvdef::HV_X64_MSR_GUEST_OS_ID, 1),
            (hvdef::HV_X64_MSR_HYPERCALL, value.hypercall),
            (hvdef::HV_X64_MSR_GUEST_OS_ID, value.guest_os_id),
        ])?;
        Ok(())
    }

    fn reftime(&mut self) -> Result<vm::ReferenceTime, Self::Error> {
        // The reference time counter is computed from the kvm clock, and the
        // kvm clock is the only thing that can be set (see `set_reftime`), so
        // query it here as well to keep save/restore self-consistent.
        //
        // Round up so that restoring this value never moves the kvm clock
        // backwards, since the guest can observe the clock at nanosecond
        // granularity.
        let clock = self.inner.kvm.get_clock()?;
        Ok(vm::ReferenceTime {
            value: clock.clock_ns.div_ceil(100),
        })
    }

    fn set_reftime(&mut self, value: &vm::ReferenceTime) -> Result<(), Self::Error> {
        // KVM does not allow setting the reference time counter directly, but
        // it is computed from the kvm clock, so set that instead. This also
        // updates the reference TSC page parameters, so that the guest's view
        // of the reference time is consistent however it queries it.
        self.inner
            .kvm
            .set_clock_ns(value.value.saturating_mul(100))?;

        // Moving the clock leaves the guest counter where it was, so put the counter back
        // on it. A restore writes this element before any vp's `Tsc`, and each of those
        // writes then reapplies the offset recorded here (see `set_tsc`). Storing the saved
        // counters instead would leave the restored pair off by the difference between how
        // far apart save read and restore wrote the clock and the counter, and each save
        // would record that error for the next restore to add to. On a machine reset this
        // runs inside `reset_all`, and `ResetPartition::reset` then aligns again, which it
        // must also do for a partition with no reference time element.
        let alignment = self.inner.align_guest_tsc()?;
        tracelimit::info_ratelimited!(
            reference_time = value.value,
            guest_tsc_before = alignment.map(|a| a.before),
            guest_tsc_after = alignment.map(|a| a.after),
            tsc_pairing = alignment.map(|a| a.pairing),
            tsc_pairing_error_ns = alignment.map(|a| a.pairing_error_ns),
            tsc_requested_lead_ns = alignment.map(|a| a.requested_lead_ns),
            tsc_achieved_lead_ns = alignment.map(|a| a.achieved_lead_ns),
            "reference clock set"
        );
        Ok(())
    }

    fn reference_tsc_page(&mut self) -> Result<vm::ReferenceTscPage, Self::Error> {
        super::vp_state::get_msrs_state(&self.inner.vp_kvm(VpIndex::BSP))
    }

    fn set_reference_tsc_page(&mut self, value: &vm::ReferenceTscPage) -> Result<(), Self::Error> {
        super::vp_state::set_msrs_state(&self.inner.vp_kvm(VpIndex::BSP), value)
    }
}
