// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::TargetArch;
use super::VmTarget;
use crate::gdb::targets::ToTargetResult;
use futures::executor::block_on;
use gdbstub::common::Signal;
use gdbstub::common::Tid;
use gdbstub::target::TargetResult;
use gdbstub::target::ext::base::multithread::MultiThreadBase;
use gdbstub::target::ext::base::multithread::MultiThreadResume;
use gdbstub::target::ext::base::multithread::MultiThreadResumeOps;
use gdbstub::target::ext::base::multithread::MultiThreadSingleStep;
use gdbstub::target::ext::base::multithread::MultiThreadSingleStepOps;
use gdbstub::target::ext::base::single_register_access::SingleRegisterAccess;
use mesh::rpc::RpcSend;
use vmm_core_defs::debug_rpc::DebugRequest;
use vmm_core_defs::debug_rpc::DebugState;

impl<T: TargetArch> MultiThreadBase for VmTarget<'_, T> {
    fn read_registers(&mut self, regs: &mut T::Registers, tid: Tid) -> TargetResult<(), Self> {
        let vp_index = self.0.tid_to_vp(tid).fatal()?;

        let state = block_on(
            self.0
                .req_chan
                .call_failable(DebugRequest::GetVpState, vp_index),
        )
        .nonfatal()?;

        T::registers(&state, regs)?;
        Ok(())
    }

    fn write_registers(&mut self, regs: &T::Registers, tid: Tid) -> TargetResult<(), Self> {
        let vp_index = self.0.tid_to_vp(tid).fatal()?;

        let mut state = block_on(
            self.0
                .req_chan
                .call_failable(DebugRequest::GetVpState, vp_index),
        )
        .nonfatal()?;

        T::update_registers(&mut state, regs)?;

        block_on(
            self.0
                .req_chan
                .call_failable(DebugRequest::SetVpState, (vp_index, state)),
        )
        .nonfatal()?;

        Ok(())
    }

    fn read_addrs(
        &mut self,
        start_addr: T::Usize,
        data: &mut [u8],
        tid: Tid,
    ) -> TargetResult<(), Self> {
        self.0
            .read_guest_virtual_memory(self.0.tid_to_vp(tid).fatal()?, start_addr.into(), data)
            .nonfatal()?;
        Ok(())
    }

    fn write_addrs(
        &mut self,
        start_addr: T::Usize,
        data: &[u8],
        tid: Tid,
    ) -> TargetResult<(), Self> {
        self.0
            .write_guest_virtual_memory(self.0.tid_to_vp(tid).fatal()?, start_addr.into(), data)
            .nonfatal()?;
        Ok(())
    }

    fn list_active_threads(
        &mut self,
        thread_is_active: &mut dyn FnMut(Tid),
    ) -> Result<(), Self::Error> {
        for i in 0..self.0.vps.len() as u32 {
            thread_is_active(self.0.vp_to_tid(i));
        }
        Ok(())
    }

    #[inline(always)]
    fn support_resume(&mut self) -> Option<MultiThreadResumeOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_single_register_access(
        &mut self,
    ) -> Option<
        gdbstub::target::ext::base::single_register_access::SingleRegisterAccessOps<'_, Tid, Self>,
    > {
        Some(self)
    }
}

impl<T: TargetArch> SingleRegisterAccess<Tid> for VmTarget<'_, T> {
    fn read_register(
        &mut self,
        tid: Tid,
        reg_id: T::RegId,
        buf: &mut [u8],
    ) -> TargetResult<usize, Self> {
        let vp_index = self.0.tid_to_vp(tid).fatal()?;

        let state = block_on(
            self.0
                .req_chan
                .call_failable(DebugRequest::GetVpState, vp_index),
        )
        .nonfatal()?;

        Ok(T::register(&state, reg_id, buf)?)
    }

    fn write_register(&mut self, tid: Tid, reg_id: T::RegId, val: &[u8]) -> TargetResult<(), Self> {
        let vp_index = self.0.tid_to_vp(tid).fatal()?;

        let mut state = block_on(
            self.0
                .req_chan
                .call_failable(DebugRequest::GetVpState, vp_index),
        )
        .nonfatal()?;

        T::update_register(&mut state, reg_id, val)?;

        block_on(
            self.0
                .req_chan
                .call_failable(DebugRequest::SetVpState, (vp_index, state)),
        )
        .nonfatal()?;

        Ok(())
    }
}

impl<T: TargetArch> MultiThreadResume for VmTarget<'_, T> {
    fn resume(&mut self) -> Result<(), Self::Error> {
        for (vp_index, vp) in self.0.vps.iter().enumerate() {
            let state = DebugState {
                single_step: vp.single_step,
                breakpoints: self.breakpoints,
            };
            tracing::debug!("resume: vp_index: {}, debug_state: {:?}", vp_index, state);
            self.0.req_chan.send(DebugRequest::SetDebugState {
                vp: vp_index as u32,
                state,
            });
        }

        let (send, recv) = mesh::oneshot();
        self.0
            .req_chan
            .send(DebugRequest::Resume { response: send });
        self.0.stop_chan = Some(recv);
        Ok(())
    }

    fn clear_resume_actions(&mut self) -> Result<(), Self::Error> {
        for vp in self.0.vps.as_mut() {
            vp.single_step = false;
        }
        Ok(())
    }

    fn set_resume_action_continue(
        &mut self,
        tid: Tid,
        _signal: Option<Signal>,
    ) -> Result<(), Self::Error> {
        let vp_index = self.0.tid_to_vp(tid)?;
        self.0.vps[vp_index as usize].single_step = false;
        Ok(())
    }

    #[inline(always)]
    fn support_single_step(&mut self) -> Option<MultiThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl<T: TargetArch> MultiThreadSingleStep for VmTarget<'_, T> {
    fn set_resume_action_step(
        &mut self,
        tid: Tid,
        _signal: Option<Signal>,
    ) -> Result<(), Self::Error> {
        let vp_index = self.0.tid_to_vp(tid)?;
        self.0.vps[vp_index as usize].single_step = true;
        Ok(())
    }
}
