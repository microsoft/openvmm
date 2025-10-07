// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Target implementation that matches the x86_64 target defined in QEMU (for
//! compatibility with ExdiGdbSrv)

use super::ArchError;
use super::TargetArch;
use crate::gdb::arch::x86::reg::X86_64CoreRegs;
use crate::gdb::arch::x86::reg::X86SegmentRegs;
use crate::gdb::arch::x86::reg::id::X86_64CoreRegId;
use crate::gdb::arch::x86::reg::id::X86SegmentRegId;
use vmm_core_defs::debug_rpc::DebuggerVpState;

mod target_xml;

impl TargetArch for crate::gdb::arch::x86::X86_64_QEMU {
    type Address = u64;

    fn register(
        state: &DebuggerVpState,
        reg_id: Self::RegId,
        buf: &mut [u8],
    ) -> Result<usize, ArchError> {
        let DebuggerVpState::X86_64(state) = state else {
            return Err(ArchError);
        };
        match reg_id {
            X86_64CoreRegId::Segment(X86SegmentRegId::CS) => {
                buf[..4].copy_from_slice(&(state.cs.selector as u32).to_le_bytes());
                Ok(4)
            }
            X86_64CoreRegId::Efer => {
                buf[..8].copy_from_slice(&state.efer.to_le_bytes());
                Ok(8)
            }
            _ => Err(ArchError),
        }
    }

    fn registers(state: &DebuggerVpState, regs: &mut Self::Registers) -> Result<(), ArchError> {
        let DebuggerVpState::X86_64(state) = state else {
            return Err(ArchError);
        };
        let gp_regs = {
            let [
                rax,
                rcx,
                rdx,
                rbx,
                rsp,
                rbp,
                rsi,
                rdi,
                r8,
                r9,
                r10,
                r11,
                r12,
                r13,
                r14,
                r15,
            ] = state.gp;
            [
                rax, rbx, rcx, rdx, rsi, rdi, rbp, rsp, r8, r9, r10, r11, r12, r13, r14, r15,
            ]
        };

        *regs = X86_64CoreRegs {
            regs: gp_regs,
            eflags: state
                .rflags
                .try_into()
                .expect("high 32 bits of rflags are non-zero"),
            rip: state.rip,
            segments: X86SegmentRegs {
                cs: state.cs.selector.into(),
                ss: state.ss.selector.into(),
                ds: state.ds.selector.into(),
                es: state.es.selector.into(),
                fs: state.fs.selector.into(),
                gs: state.gs.selector.into(),
            },

            fs_base: state.fs.base,
            gs_base: state.fs.base,
            k_gs_base: state.kernel_gs_base,
            cr0: state.cr0,
            cr2: state.cr2,
            cr3: state.cr3,
            cr4: state.cr4,
            cr8: state.cr8,
            efer: state.efer,

            // TODO from xsave
            st: Default::default(),
            fpu: Default::default(),
            xmm: [0; 16],
            mxcsr: 0,
        };

        Ok(())
    }

    fn update_registers(
        state: &mut DebuggerVpState,
        regs: &Self::Registers,
    ) -> Result<(), ArchError> {
        let DebuggerVpState::X86_64(state) = state else {
            return Err(ArchError);
        };
        state.gp = {
            let [
                rax,
                rbx,
                rcx,
                rdx,
                rsi,
                rdi,
                rbp,
                rsp,
                r8,
                r9,
                r10,
                r11,
                r12,
                r13,
                r14,
                r15,
            ] = regs.regs;
            [
                rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15,
            ]
        };
        state.rflags = regs.eflags.into();
        state.rip = regs.rip;

        // TODO: more registers
        tracing::warn!("[incomplete] write_registers will only update GP regs, rip, and rflags");
        Ok(())
    }

    fn update_register(
        state: &mut DebuggerVpState,
        reg_id: Self::RegId,
        val: &[u8],
    ) -> Result<(), ArchError> {
        let DebuggerVpState::X86_64(state) = state else {
            return Err(ArchError);
        };
        // Works for the set of supported registers. Some registers are
        // greater than 8 bytes, and would need a different handler.
        fn to_u64_le(val: &[u8]) -> u64 {
            let mut buf = [0; 8];
            buf[..val.len().min(8)].copy_from_slice(val);
            u64::from_le_bytes(buf)
        }

        match reg_id {
            X86_64CoreRegId::Gpr(idx) => state.gp[idx as usize] = to_u64_le(val),
            X86_64CoreRegId::Rip => state.rip = to_u64_le(val),
            X86_64CoreRegId::Eflags => state.rflags = to_u64_le(val),
            _ => {
                // TODO: more registers
                // Windbg bulk updates registers and we don't need to error on each update.
                tracelimit::error_ratelimited!(
                    "[incomplete] write_register supports GPRs, rip, and rflags. Cannot update {:?}",
                    reg_id
                );
            }
        }

        Ok(())
    }

    fn support_target_description_xml_override<'a, 'b>(
        target: &'a mut super::VmTarget<'b, Self>,
    ) -> Option<
        gdbstub::target::ext::target_description_xml_override::TargetDescriptionXmlOverrideOps<
            'a,
            super::VmTarget<'b, Self>,
        >,
    > {
        Some(target)
    }
}
