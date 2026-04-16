// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM-level state access for mshv.
//!
//! VM-level HV registers (GuestOsId, Hypercall, ReferenceTsc) are accessed
//! via the VP register interface using the BSP's VcpuFd, which is retained
//! in `MshvPartitionInner`. These accessors must only be called while VPs
//! are stopped (e.g., during reset or save/restore).

use super::Error;
use super::VcpuFdExt;
use crate::MshvPartition;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvRegisterAssoc;
use mshv_bindings::hv_partition_property_code_HV_PARTITION_PROPERTY_REFERENCE_TIME;
use virt::state::HvRegisterState;
use virt::x86::vm;
use virt::x86::vm::AccessVmState;
use zerocopy::FromZeros;

impl MshvPartition {
    fn get_vm_register_state<T, const N: usize>(&self) -> Result<T, Error>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        let mut regs = T::default();
        let mut assoc = regs.names().map(|name| HvRegisterAssoc {
            name: name.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        });

        self.inner
            .bsp_vcpufd
            .get_hvdef_regs(&mut assoc[..])
            .map_err(Error::Register)?;

        regs.set_values(assoc.iter().map(|assoc| assoc.value));
        Ok(regs)
    }

    fn set_vm_register_state<T, const N: usize>(&self, regs: &T) -> Result<(), Error>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        let mut assoc = regs.names().map(|name| HvRegisterAssoc {
            name: name.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        });

        regs.get_values(assoc.iter_mut().map(|assoc| &mut assoc.value));

        self.inner
            .bsp_vcpufd
            .set_hvdef_regs(&assoc[..])
            .map_err(Error::Register)
    }
}

impl AccessVmState for &'_ MshvPartition {
    type Error = Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn hypercall(&mut self) -> Result<vm::HypercallMsrs, Self::Error> {
        self.get_vm_register_state()
    }

    fn set_hypercall(&mut self, value: &vm::HypercallMsrs) -> Result<(), Self::Error> {
        self.set_vm_register_state(value)
    }

    fn reftime(&mut self) -> Result<vm::ReferenceTime, Self::Error> {
        let ref_time = self
            .inner
            .vmfd
            .get_partition_property(hv_partition_property_code_HV_PARTITION_PROPERTY_REFERENCE_TIME)
            .map_err(|e| Error::GetPartitionProperty(e.into()))?;
        Ok(vm::ReferenceTime { value: ref_time })
    }

    fn set_reftime(&mut self, value: &vm::ReferenceTime) -> Result<(), Self::Error> {
        self.inner
            .vmfd
            .set_partition_property(
                hv_partition_property_code_HV_PARTITION_PROPERTY_REFERENCE_TIME,
                value.value,
            )
            .map_err(|e| Error::SetPartitionProperty(e.into()))
    }

    fn reference_tsc_page(&mut self) -> Result<vm::ReferenceTscPage, Self::Error> {
        self.get_vm_register_state()
    }

    fn set_reference_tsc_page(&mut self, value: &vm::ReferenceTscPage) -> Result<(), Self::Error> {
        self.set_vm_register_state(value)
    }
}
