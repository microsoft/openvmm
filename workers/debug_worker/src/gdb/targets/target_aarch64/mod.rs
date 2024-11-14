// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Target implementation for aarch64.

use super::ArchError;
use super::TargetArch;
use aarch64defs::Cpsr64;
use gdbstub_arch::aarch64::reg::AArch64CoreRegs;
use vmm_core_defs::debug_rpc::DebuggerVpState;

impl TargetArch for gdbstub_arch::aarch64::AArch64 {
    type Address = u64;

    fn register(
        state: &DebuggerVpState,
        reg_id: Self::RegId,
        buf: &mut [u8],
    ) -> Result<usize, ArchError> {
        let DebuggerVpState::Aarch64(state) = state else {
            return Err(ArchError);
        };
        let n = match reg_id {
            gdbstub_arch::aarch64::reg::id::AArch64RegId::X(i) => {
                buf[..8].copy_from_slice(&state.x[i as usize].to_le_bytes());
                8
            }
            gdbstub_arch::aarch64::reg::id::AArch64RegId::Sp => {
                let sp = if Cpsr64::from(state.cpsr).el() == 1 {
                    state.sp_el1
                } else {
                    state.sp_el0
                };
                buf[..8].copy_from_slice(&sp.to_le_bytes());
                8
            }
            gdbstub_arch::aarch64::reg::id::AArch64RegId::Pc => {
                buf[..8].copy_from_slice(&state.pc.to_le_bytes());
                8
            }
            gdbstub_arch::aarch64::reg::id::AArch64RegId::Pstate => {
                buf[..4].copy_from_slice(&(state.cpsr as u32).to_le_bytes());
                4
            }
            gdbstub_arch::aarch64::reg::id::AArch64RegId::System(id) => {
                // System Registers encoded as (Op0:2, Op1:3, CRn:4, CRm:4, Op2:2)
                let reg = aarch64defs::SystemReg(
                    aarch64defs::SystemRegEncoding::new()
                        .with_op0((id >> 14) as u8)
                        .with_op1(((id >> 11) & 0b111) as u8)
                        .with_crn(((id >> 7) & 0b1111) as u8)
                        .with_crm(((id >> 3) & 0b1111) as u8)
                        .with_op2((id & 0b11) as u8),
                );
                tracing::warn!(?reg, "unsupported system register");
                return Err(ArchError);
            }
            _ => return Err(ArchError),
        };
        Ok(n)
    }

    fn registers(state: &DebuggerVpState, regs: &mut Self::Registers) -> Result<(), ArchError> {
        let DebuggerVpState::Aarch64(state) = state else {
            return Err(ArchError);
        };
        *regs = AArch64CoreRegs {
            x: state.x,
            sp: if Cpsr64::from(state.cpsr).el() == 1 {
                state.sp_el1
            } else {
                state.sp_el0
            },
            pc: state.pc,
            cpsr: state.cpsr as u32,
            v: [0; 32], // TODO: plumb floating point register state
            fpcr: 0,
            fpsr: 0,
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
