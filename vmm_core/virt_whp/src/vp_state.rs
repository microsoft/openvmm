// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Error;
use crate::WhpProcessor;
use crate::WhpResultExt;
use crate::WhpVpRef;
use crate::regs::ToWhpRegister;
use crate::regs::hv_register_to_whp;
use hvdef::HvRegisterValue;
use hvdef::Vtl;
use virt::state::HvRegisterState;
use whp::abi::WHV_REGISTER_VALUE;
use zerocopy::FromZeros;

pub struct WhpVpStateAccess<'a, 'b> {
    run: &'a mut WhpProcessor<'b>,
    vtl: Vtl,
}

impl<'a> WhpProcessor<'a> {
    pub(crate) fn access_state(&mut self, vtl: Vtl) -> WhpVpStateAccess<'_, 'a> {
        self.reset_if_requested();
        WhpVpStateAccess { run: self, vtl }
    }
}

impl WhpVpRef<'_> {
    pub(crate) fn set_register_state<T, R: ToWhpRegister, const N: usize>(
        &self,
        vtl: Vtl,
        regs: &T,
    ) -> Result<(), Error>
    where
        T: HvRegisterState<R, N>,
    {
        let names = regs.names().map(|name| hv_register_to_whp(name).unwrap());
        let mut values = [HvRegisterValue::new_zeroed(); N];
        regs.get_values(values.iter_mut());
        self.whp(vtl)
            .set_registers(&names, unsafe {
                std::mem::transmute::<&[HvRegisterValue], &[WHV_REGISTER_VALUE]>(&values[..])
            })
            .for_op("set registers")?;
        Ok(())
    }

    pub(crate) fn get_register_state<T, R: ToWhpRegister, const N: usize>(
        &self,
        vtl: Vtl,
    ) -> Result<T, Error>
    where
        T: HvRegisterState<R, N>,
    {
        let mut regs = T::default();
        let names = regs.names().map(|name| hv_register_to_whp(name).unwrap());
        let mut values = [HvRegisterValue::new_zeroed(); N];
        self.whp(vtl)
            .get_registers(&names, unsafe {
                std::mem::transmute::<&mut [HvRegisterValue], &mut [WHV_REGISTER_VALUE]>(
                    &mut values[..],
                )
            })
            .for_op("get registers")?;
        regs.set_values(values.into_iter());
        Ok(regs)
    }
}

#[cfg(guest_arch = "x86_64")]
mod x86 {
    use super::WhpVpStateAccess;
    use crate::Error;
    use crate::WhpResultExt;
    use virt::state::StateElement;
    use virt::x86::vp;
    use virt::x86::vp::AccessVpState;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    impl AccessVpState for WhpVpStateAccess<'_, '_> {
        type Error = Error;

        fn caps(&self) -> &virt::PartitionCapabilities {
            &self.run.vp.partition.caps
        }

        fn commit(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
            self.run.save_activity(self.vtl)
        }

        fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
            self.run.restore_activity(self.vtl, value)
        }

        fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
            let data = self
                .run
                .vp
                .whp(self.vtl)
                .get_xsave()
                .for_op("get xsave state")?;
            Ok(vp::Xsave::from_compact(&data, &self.run.vp.partition.caps))
        }

        fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
            self.run
                .vp
                .whp(self.vtl)
                .set_xsave(value.compact())
                .for_op("set xsave state")?;
            Ok(())
        }

        fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
            self.run.save_apic(self.vtl)
        }

        fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
            self.run.restore_apic(self.vtl, value)
        }

        fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_xcr(&mut self, value: &vp::Xcr0) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_xss(&mut self, value: &vp::Xss) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn mtrrs(&mut self) -> Result<vp::Mtrrs, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_mtrrs(&mut self, mtrrs: &vp::Mtrrs) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, mtrrs)
        }

        fn pat(&mut self) -> Result<vp::Pat, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_pat(&mut self, value: &vp::Pat) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_virtual_msrs(&mut self, msrs: &vp::VirtualMsrs) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, msrs)
        }

        fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_tsc(&mut self, value: &vp::Tsc) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_cet(&mut self, value: &vp::Cet) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_cet_ss(&mut self, value: &vp::CetSs) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn tsc_aux(&mut self) -> Result<vp::TscAux, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_tsc_aux(&mut self, value: &vp::TscAux) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn synic_msrs(&mut self) -> Result<vp::SyntheticMsrs, Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                    Ok(vp::SyntheticMsrs::at_reset(
                        self.caps(),
                        &self.run.inner.vp_info,
                    ))
                }
                None => self.run.vp.get_register_state(self.vtl),
            }
        }

        fn set_synic_msrs(&mut self, value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                    Ok(())
                }
                None => self.run.vp.set_register_state(self.vtl, value),
            }
        }

        fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                    Ok(vp::SynicTimers::at_reset(
                        self.caps(),
                        &self.run.inner.vp_info,
                    ))
                }
                None => {
                    let mut state = hvdef::HvSyntheticTimersState::new_zeroed();
                    self.run
                        .vp
                        .whp(self.vtl)
                        .get_state(
                            whp::abi::WHvVirtualProcessorStateTypeSynicTimerState,
                            state.as_mut_bytes(),
                        )
                        .for_op("get synic timer state")?;
                    Ok(vp::SynicTimers::from_hv(state))
                }
            }
        }

        fn set_synic_timers(&mut self, value: &vp::SynicTimers) -> Result<(), Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                }
                None => {
                    self.run
                        .vp
                        .whp(self.vtl)
                        .set_state(
                            whp::abi::WHvVirtualProcessorStateTypeSynicTimerState,
                            value.as_hv().as_bytes(),
                        )
                        .for_op("set synic timer state")?;
                }
            }
            Ok(())
        }

        fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
            Ok(self.run.vplc(self.vtl).message_queues.save())
        }

        fn set_synic_message_queues(
            &mut self,
            value: &vp::SynicMessageQueues,
        ) -> Result<(), Self::Error> {
            self.run.vplc(self.vtl).message_queues.restore(value);
            Ok(())
        }

        fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                    Ok(vp::SynicMessagePage::at_reset(
                        self.caps(),
                        &self.run.inner.vp_info,
                    ))
                }
                None => {
                    let mut state = vp::SynicMessagePage { data: [0; 4096] };
                    self.run
                        .vp
                        .whp(self.vtl)
                        .get_state(
                            whp::abi::WHvVirtualProcessorStateTypeSynicMessagePage,
                            state.data.as_mut_slice(),
                        )
                        .for_op("get synic message page")?;
                    Ok(state)
                }
            }
        }

        fn set_synic_message_page(
            &mut self,
            value: &vp::SynicMessagePage,
        ) -> Result<(), Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                }
                None => {
                    self.run
                        .vp
                        .whp(self.vtl)
                        .set_state(
                            whp::abi::WHvVirtualProcessorStateTypeSynicMessagePage,
                            &value.data,
                        )
                        .for_op("set synic message page")?;
                }
            }
            Ok(())
        }

        fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                    Ok(vp::SynicEventFlagsPage::at_reset(
                        self.caps(),
                        &self.run.inner.vp_info,
                    ))
                }
                None => {
                    let mut state = vp::SynicEventFlagsPage { data: [0; 4096] };
                    self.run
                        .vp
                        .whp(self.vtl)
                        .get_state(
                            whp::abi::WHvVirtualProcessorStateTypeSynicEventFlagPage,
                            state.data.as_mut_slice(),
                        )
                        .for_op("get synic event flag page")?;
                    Ok(state)
                }
            }
        }

        fn set_synic_event_flags_page(
            &mut self,
            value: &vp::SynicEventFlagsPage,
        ) -> Result<(), Self::Error> {
            match self.run.state.vtls[self.vtl].hv {
                Some(_) => {
                    // TODO
                }
                None => {
                    self.run
                        .vp
                        .whp(self.vtl)
                        .set_state(
                            whp::abi::WHvVirtualProcessorStateTypeSynicEventFlagPage,
                            &value.data,
                        )
                        .for_op("set synic event flag page")?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(guest_arch = "aarch64")]
mod aarch64 {
    use super::WhpVpStateAccess;
    use crate::Error;
    use virt::aarch64::vp;
    use virt::aarch64::vp::AccessVpState;

    impl AccessVpState for WhpVpStateAccess<'_, '_> {
        type Error = Error;

        fn caps(&self) -> &virt::PartitionCapabilities {
            &self.run.vp.partition.caps
        }

        fn commit(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }

        fn system_registers(&mut self) -> Result<vp::SystemRegisters, Self::Error> {
            self.run.vp.get_register_state(self.vtl)
        }

        fn set_system_registers(&mut self, value: &vp::SystemRegisters) -> Result<(), Self::Error> {
            self.run.vp.set_register_state(self.vtl, value)
        }
    }
}
