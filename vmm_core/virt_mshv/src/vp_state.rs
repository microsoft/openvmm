// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Error;
use super::VcpuFdExt;
use crate::MshvProcessor;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvRegisterAssoc;
use mshv_bindings::LapicState;
use mshv_bindings::MSHV_VP_STATE_SIEFP;
use mshv_bindings::MSHV_VP_STATE_SIMP;
use mshv_bindings::MSHV_VP_STATE_SYNTHETIC_TIMERS;
use mshv_bindings::mshv_get_set_vp_state;
use virt::state::HvRegisterState;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

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

        self.runner
            .vcpufd
            .set_hvdef_regs(&assoc[..])
            .map_err(Error::Register)?;

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

        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc[..])
            .map_err(Error::Register)?;

        regs.set_values(assoc.iter().map(|assoc| assoc.value));
        Ok(regs)
    }
}

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
        let mut activity: vp::Activity = self.get_register_state()?;
        // The NMI pending bit is not part of the register state; it lives
        // in the APIC page.
        let lapic = self.runner.vcpufd.get_lapic().map_err(Error::VpState)?;
        let page: [u8; 1024] = lapic.regs.map(|b| b as u8);
        activity.nmi_pending = vp::hv_apic_nmi_pending(&page);
        Ok(activity)
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        self.set_register_state(value)?;
        // The NMI pending bit is not part of the register state; it must
        // be set via the APIC page.
        let mut lapic = self.runner.vcpufd.get_lapic().map_err(Error::VpState)?;
        let mut page: [u8; 1024] = lapic.regs.map(|b| b as u8);
        vp::set_hv_apic_nmi_pending(&mut page, value.nmi_pending);
        lapic.regs = page.map(|b| b as std::os::raw::c_char);
        self.runner
            .vcpufd
            .set_lapic(&lapic)
            .map_err(Error::VpState)?;
        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        let xsave = self.runner.vcpufd.get_xsave().map_err(Error::VpState)?;
        Ok(vp::Xsave::from_compact(&xsave.buffer, &self.partition.caps))
    }

    fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
        let data = value.compact();
        let vp_state = mshv_get_set_vp_state {
            type_: mshv_bindings::MSHV_VP_STATE_XSAVE as u8,
            buf_sz: data.len() as u32,
            buf_ptr: data.as_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .set_vp_state_ioctl(&vp_state)
            .map_err(Error::VpState)
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        // Get the APIC base register.
        let mut assoc = [HvRegisterAssoc {
            name: HvX64RegisterName::ApicBase.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        }];
        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc)
            .map_err(Error::Register)?;
        let apic_base = assoc[0].value.as_u64();

        // Get the LAPIC state page.
        let lapic = self.runner.vcpufd.get_lapic().map_err(Error::VpState)?;
        let mut page: [u8; 1024] = lapic.regs.map(|b| b as u8);

        // Clear the non-architectural NMI pending bit.
        vp::set_hv_apic_nmi_pending(&mut page, false);

        Ok(vp::Apic::from_page(apic_base, &page))
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        // Set the APIC base register first to set the APIC mode before
        // updating the APIC register state.
        self.runner
            .vcpufd
            .set_hvdef_regs(&[HvRegisterAssoc::from((
                HvX64RegisterName::ApicBase,
                value.apic_base,
            ))])
            .map_err(Error::Register)?;

        // Preserve the current NMI pending state across the restore.
        let current_lapic = self.runner.vcpufd.get_lapic().map_err(Error::VpState)?;
        let current_page: [u8; 1024] = current_lapic.regs.map(|b| b as u8);
        let nmi_pending = vp::hv_apic_nmi_pending(&current_page);

        // Set the LAPIC state page, restoring the NMI pending bit.
        let mut page = value.as_page();
        vp::set_hv_apic_nmi_pending(&mut page, nmi_pending);
        let lapic = LapicState {
            regs: page.map(|b| b as std::os::raw::c_char),
        };
        self.runner
            .vcpufd
            .set_lapic(&lapic)
            .map_err(Error::VpState)?;

        Ok(())
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
        let mut state = hvdef::HvSyntheticTimersState::new_zeroed();
        let mut vp_state = mshv_get_set_vp_state {
            type_: MSHV_VP_STATE_SYNTHETIC_TIMERS as u8,
            buf_sz: size_of_val(&state) as u32,
            buf_ptr: state.as_mut_bytes().as_mut_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .get_vp_state_ioctl(&mut vp_state)
            .map_err(Error::VpState)?;
        Ok(vp::SynicTimers::from_hv(state))
    }

    fn set_synic_timers(&mut self, value: &vp::SynicTimers) -> Result<(), Self::Error> {
        let state = value.as_hv();
        let vp_state = mshv_get_set_vp_state {
            type_: MSHV_VP_STATE_SYNTHETIC_TIMERS as u8,
            buf_sz: size_of_val(&state) as u32,
            buf_ptr: state.as_bytes().as_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .set_vp_state_ioctl(&vp_state)
            .map_err(Error::VpState)
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Ok(self.inner.message_queues.save())
    }

    fn set_synic_message_queues(
        &mut self,
        value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        self.inner.message_queues.restore(value);
        Ok(())
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        let mut state = vp::SynicMessagePage { data: [0; 4096] };
        let mut vp_state = mshv_get_set_vp_state {
            type_: MSHV_VP_STATE_SIMP as u8,
            buf_sz: size_of_val(&state.data) as u32,
            buf_ptr: state.data.as_mut_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .get_vp_state_ioctl(&mut vp_state)
            .map_err(Error::VpState)?;
        Ok(state)
    }

    fn set_synic_message_page(&mut self, value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        let vp_state = mshv_get_set_vp_state {
            type_: MSHV_VP_STATE_SIMP as u8,
            buf_sz: size_of_val(&value.data) as u32,
            buf_ptr: value.data.as_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .set_vp_state_ioctl(&vp_state)
            .map_err(Error::VpState)
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        let mut state = vp::SynicEventFlagsPage { data: [0; 4096] };
        let mut vp_state = mshv_get_set_vp_state {
            type_: MSHV_VP_STATE_SIEFP as u8,
            buf_sz: size_of_val(&state.data) as u32,
            buf_ptr: state.data.as_mut_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .get_vp_state_ioctl(&mut vp_state)
            .map_err(Error::VpState)?;
        Ok(state)
    }

    fn set_synic_event_flags_page(
        &mut self,
        value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        let vp_state = mshv_get_set_vp_state {
            type_: MSHV_VP_STATE_SIEFP as u8,
            buf_sz: size_of_val(&value.data) as u32,
            buf_ptr: value.data.as_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .set_vp_state_ioctl(&vp_state)
            .map_err(Error::VpState)
    }
}
