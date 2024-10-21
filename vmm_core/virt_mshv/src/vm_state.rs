// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Error;
use crate::MshvPartition;
use virt::x86::vm;
use virt::x86::vm::AccessVmState;

impl AccessVmState for &'_ MshvPartition {
    type Error = Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        todo!()
    }

    fn hypercall(&mut self) -> Result<vm::HypercallMsrs, Self::Error> {
        todo!()
    }

    fn set_hypercall(&mut self, _value: &vm::HypercallMsrs) -> Result<(), Self::Error> {
        todo!()
    }

    fn reftime(&mut self) -> Result<vm::ReferenceTime, Self::Error> {
        todo!()
    }

    fn set_reftime(&mut self, _value: &vm::ReferenceTime) -> Result<(), Self::Error> {
        todo!()
    }

    fn reference_tsc_page(&mut self) -> Result<vm::ReferenceTscPage, Self::Error> {
        todo!()
    }

    fn set_reference_tsc_page(&mut self, _value: &vm::ReferenceTscPage) -> Result<(), Self::Error> {
        todo!()
    }
}
