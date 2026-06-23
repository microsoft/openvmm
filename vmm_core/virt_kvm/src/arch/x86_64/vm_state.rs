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
        super::vp_state::get_msrs_state(&self.inner.vp_kvm(VpIndex::BSP))
    }

    fn set_reftime(&mut self, _value: &vm::ReferenceTime) -> Result<(), Self::Error> {
        // TODO: KVM doesn't allow setting the reference time, since it's
        // computed from the kvm clock. Figure out what we can do instead.
        Ok(())
    }

    fn reference_tsc_page(&mut self) -> Result<vm::ReferenceTscPage, Self::Error> {
        super::vp_state::get_msrs_state(&self.inner.vp_kvm(VpIndex::BSP))
    }

    fn set_reference_tsc_page(&mut self, value: &vm::ReferenceTscPage) -> Result<(), Self::Error> {
        super::vp_state::set_msrs_state(&self.inner.vp_kvm(VpIndex::BSP), value)
    }
}
