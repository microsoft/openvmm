// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A minimal target for debugging 16-bit and 32-bit early boot code

use super::ArchError;
use super::TargetArch;
use crate::gdb::arch::x86::reg::X86CoreRegs;
use crate::gdb::arch::x86::reg::X86SegmentRegs;
use vmm_core_defs::debug_rpc::DebuggerVpState;

impl TargetArch for crate::gdb::arch::x86::I8086 {
    type Address = u32;

    fn register(
        _state: &DebuggerVpState,
        _reg_id: Self::RegId,
        _buf: &mut [u8],
    ) -> Result<usize, ArchError> {
        Err(ArchError)
    }

    fn registers(state: &DebuggerVpState, regs: &mut Self::Registers) -> Result<(), ArchError> {
        let DebuggerVpState::X86_64(state) = state else {
            return Err(ArchError);
        };
        let [eax, ecx, edx, ebx, esp, ebp, esi, edi] = {
            let [rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, ..] = state.gp;
            [
                rax as u32, rcx as u32, rdx as u32, rbx as u32, rsp as u32, rbp as u32, rsi as u32,
                rdi as u32,
            ]
        };

        *regs = X86CoreRegs {
            eax,
            ecx,
            edx,
            ebx,
            esp,
            ebp,
            esi,
            edi,
            eflags: state
                .rflags
                .try_into()
                .expect("high 32 bits of rflags are non-zero"),
            eip: state.rip as u32,
            segments: X86SegmentRegs {
                cs: state.cs.selector.into(),
                ss: state.ss.selector.into(),
                ds: state.ds.selector.into(),
                es: state.es.selector.into(),
                fs: state.fs.selector.into(),
                gs: state.gs.selector.into(),
            },

            // TODO from xsave
            st: Default::default(),
            fpu: Default::default(),
            xmm: [0; 8],
            mxcsr: 0,
        };

        Ok(())
    }

    fn update_registers(
        _state: &mut DebuggerVpState,
        _regs: &Self::Registers,
    ) -> Result<(), ArchError> {
        Err(ArchError)
    }

    fn update_register(
        _state: &mut DebuggerVpState,
        _reg_id: Self::RegId,
        _val: &[u8],
    ) -> Result<(), ArchError> {
        Err(ArchError)
    }
}
