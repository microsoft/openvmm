// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Error;
use crate::MshvProcessor;
use hvdef::hypercall::HvRegisterAssoc;
use hvdef::HvX64RegisterName;
use mshv_bindings::hv_register_assoc;
use static_assertions::assert_eq_size;
use std::mem::offset_of;
use virt::state::HvRegisterState;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;
use zerocopy::FromZeros;

impl MshvProcessor<'_> {
    pub(crate) fn set_register_state<T, const N: usize>(&self, regs: &T) -> Result<(), Error>
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
            .vcpufd
            .set_reg(hvdef_to_mshv(&assoc[..]))
            .map_err(|err| Error::Register(Box::new(err)))?;

        Ok(())
    }

    pub(crate) fn get_register_state<T, const N: usize>(&self) -> Result<T, Error>
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
            .vcpufd
            .get_reg(hvdef_to_mshv_mut(&mut assoc[..]))
            .map_err(|err| Error::Register(Box::new(err)))?;

        regs.set_values(assoc.iter().map(|assoc| assoc.value));
        Ok(regs)
    }
}

fn hvdef_to_mshv(regs: &[HvRegisterAssoc]) -> &[hv_register_assoc] {
    assert_eq_size!(HvRegisterAssoc, hv_register_assoc);
    assert_eq!(
        offset_of!(HvRegisterAssoc, name),
        offset_of!(hv_register_assoc, name)
    );
    assert_eq!(
        offset_of!(HvRegisterAssoc, value),
        offset_of!(hv_register_assoc, value)
    );
    // SAFETY: HvRegisterAssoc and hv_register_assoc have compatible definitions.
    unsafe { std::mem::transmute(regs) }
}

fn hvdef_to_mshv_mut(regs: &mut [HvRegisterAssoc]) -> &mut [hv_register_assoc] {
    assert_eq_size!(HvRegisterAssoc, hv_register_assoc);
    assert_eq!(
        offset_of!(HvRegisterAssoc, name),
        offset_of!(hv_register_assoc, name)
    );
    assert_eq!(
        offset_of!(HvRegisterAssoc, value),
        offset_of!(hv_register_assoc, value)
    );
    // SAFETY: HvRegisterAssoc and hv_register_assoc have compatible definitions.
    unsafe { std::mem::transmute(regs) }
}

#[allow(unused_variables)]
impl AccessVpState for &'_ mut MshvProcessor<'_> {
    type Error = Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        self.get_register_state()
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        self.get_register_state()
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        todo!()
    }

    fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
        todo!()
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        todo!()
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        todo!()
    }

    fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
        self.get_register_state()
    }

    fn set_xcr(&mut self, value: &vp::Xcr0) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
        self.get_register_state()
    }

    fn set_xss(&mut self, value: &vp::Xss) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn mtrrs(&mut self) -> Result<vp::Mtrrs, Self::Error> {
        self.get_register_state()
    }

    fn set_mtrrs(&mut self, value: &vp::Mtrrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn pat(&mut self) -> Result<vp::Pat, Self::Error> {
        self.get_register_state()
    }

    fn set_pat(&mut self, value: &vp::Pat) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
        self.get_register_state()
    }

    fn set_virtual_msrs(&mut self, value: &vp::VirtualMsrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
        self.get_register_state()
    }

    fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc(&mut self, value: &vp::Tsc) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
        self.get_register_state()
    }

    fn set_cet(&mut self, value: &vp::Cet) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
        self.get_register_state()
    }

    fn set_cet_ss(&mut self, value: &vp::CetSs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn tsc_aux(&mut self) -> Result<vp::TscAux, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc_aux(&mut self, value: &vp::TscAux) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn synic_msrs(&mut self) -> Result<vp::SyntheticMsrs, Self::Error> {
        self.get_register_state()
    }

    fn set_synic_msrs(&mut self, value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        todo!()
    }

    fn set_synic_timers(&mut self, value: &vp::SynicTimers) -> Result<(), Self::Error> {
        todo!()
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        todo!()
    }

    fn set_synic_message_queues(
        &mut self,
        value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        todo!()
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        todo!()
    }

    fn set_synic_message_page(&mut self, value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        todo!()
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        todo!()
    }

    fn set_synic_event_flags_page(
        &mut self,
        value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        todo!()
    }
}
