// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::WhpPartition;
use crate::WhpPartitionInner;
use hvdef::Vtl;

impl virt::PartitionAccessState for WhpPartition {
    type StateAccess<'a> = PartitionStateAccess<'a>;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        PartitionStateAccess {
            inner: &self.inner,
            vtl,
        }
    }
}

pub struct PartitionStateAccess<'a> {
    inner: &'a WhpPartitionInner,
    vtl: Vtl,
}

#[cfg(guest_arch = "x86_64")]
mod x86 {
    use super::PartitionStateAccess;
    use crate::Error;
    use crate::WhpResultExt;
    use virt::vm;
    use virt::vm::AccessVmState;

    impl AccessVmState for PartitionStateAccess<'_> {
        type Error = Error;

        fn caps(&self) -> &virt::PartitionCapabilities {
            &self.inner.caps
        }

        fn commit(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn hypercall(&mut self) -> Result<vm::HypercallMsrs, Self::Error> {
            // TODO: handle the case where the hypervisor enlightenments are
            // implemented locally
            self.inner.bsp().get_register_state(self.vtl)
        }

        fn set_hypercall(&mut self, value: &vm::HypercallMsrs) -> Result<(), Self::Error> {
            // TODO: handle the case where the hypervisor enlightenments are
            // implemented locally
            self.inner.bsp().set_register_state(self.vtl, value)
        }

        fn reftime(&mut self) -> Result<vm::ReferenceTime, Self::Error> {
            // TODO: handle the case where the hypervisor enlightenments are
            // implemented locally
            Ok(vm::ReferenceTime {
                value: self
                    .inner
                    .vtlp(self.vtl)
                    .whp
                    .reference_time()
                    .for_op("get reference time")?,
            })
        }

        fn set_reftime(&mut self, value: &vm::ReferenceTime) -> Result<(), Self::Error> {
            // TODO: handle the case where the hypervisor enlightenments are
            // implemented locally
            self.inner
                .vtlp(self.vtl)
                .whp
                .set_property(whp::PartitionProperty::ReferenceTime(value.value))
                .for_op("set reference time")?;
            Ok(())
        }

        fn reference_tsc_page(&mut self) -> Result<vm::ReferenceTscPage, Self::Error> {
            // TODO: handle the case where the hypervisor enlightenments are
            // implemented locally
            self.inner.bsp().get_register_state(self.vtl)
        }

        fn set_reference_tsc_page(
            &mut self,
            value: &vm::ReferenceTscPage,
        ) -> Result<(), Self::Error> {
            // TODO: handle the case where the hypervisor enlightenments are
            // implemented locally
            self.inner.bsp().set_register_state(self.vtl, value)
        }
    }
}

#[cfg(guest_arch = "aarch64")]
mod aarch64 {
    use super::PartitionStateAccess;
    use crate::Error;
    use virt::aarch64::vm::AccessVmState;

    impl AccessVmState for PartitionStateAccess<'_> {
        type Error = Error;

        fn caps(&self) -> &virt::PartitionCapabilities {
            &self.inner.caps
        }

        fn commit(&mut self) -> Result<(), Self::Error> {
            // Reference this to avoid dead code warning.
            let _ = self.vtl;
            Ok(())
        }
    }
}
