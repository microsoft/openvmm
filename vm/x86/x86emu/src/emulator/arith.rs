// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::rflags::update_flags_szp;
use super::Emulator;
use super::InternalError;
use crate::Cpu;
use iced_x86::Instruction;
use iced_x86::Register;
use x86defs::RFlags;

impl<T: Cpu> Emulator<'_, T> {
    // <op> rm instructions
    pub(super) async fn unary_arith<Op: UnaryArithOp>(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        let result = Op::op(left);
        self.compare_if_locked_and_write_op_0(instr, left, result)
            .await?;
        let mut rflags = self.cpu.rflags();
        Op::update_flags(&mut rflags, instr.memory_size().size(), result, left);
        self.cpu.set_rflags(rflags);
        Ok(())
    }

    // <op> rm/r, rm/r/imm instructions
    pub(super) async fn arith<Op: ArithOp>(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        let right = self.op_value(instr, 1).await?;
        let mut rflags = self.cpu.rflags();
        let result = Op::op(left, right, rflags);
        if Op::UPDATES_RESULT {
            self.compare_if_locked_and_write_op_0(instr, left, result)
                .await?;
        }
        Op::update_flags(&mut rflags, instr.memory_size().size(), result, left, right);
        self.cpu.set_rflags(rflags);
        Ok(())
    }

    // xadd rm/r, r
    pub(super) async fn xadd(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        let right_reg = instr.op1_register();
        let right = self.cpu.gp(right_reg.into());
        let result = left.wrapping_add(right);

        self.compare_if_locked_and_write_op_0(instr, left, result)
            .await?;
        self.cpu.set_gp(right_reg.into(), left);
        let mut rflags = self.cpu.rflags();
        update_flags_arith(
            &mut rflags,
            true,
            true,
            instr.memory_size().size(),
            result,
            right,
            left,
        );
        self.cpu.set_rflags(rflags);
        Ok(())
    }

    // cmpxchg rm/r, r
    pub(super) async fn cmpxchg(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        let right = self.cpu.gp(instr.op1_register().into());

        let op_size = instr.memory_size().size();
        let cmp_reg = match op_size {
            1 => Register::AL,
            2 => Register::AX,
            4 => Register::EAX,
            8 => Register::RAX,
            _ => unreachable!(),
        };
        let cmp_val = self.cpu.gp(cmp_reg.into());

        let result = CmpOp::op(cmp_val, left, self.cpu.rflags());

        if result == 0 {
            self.compare_if_locked_and_write_op_0(instr, left, right)
                .await?;
        } else {
            self.cpu.set_gp(cmp_reg.into(), left);
        }

        let mut rflags = self.cpu.rflags();
        CmpOp::update_flags(&mut rflags, op_size, result, cmp_val, left);
        self.cpu.set_rflags(rflags);

        Ok(())
    }

    // xchg rm/r, r
    pub(super) async fn xchg(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left = self.op_value(instr, 0).await?;
        let right = self.cpu.gp(instr.op1_register().into());

        self.compare_if_locked_and_write_op_0(instr, left, right)
            .await?;
        self.cpu.set_gp(instr.op1_register().into(), left);
        Ok(())
    }
}

/// Updates rflags after an and, or, xor, etc. operation.
fn update_flags_logic(flags: &mut RFlags, operand_size: usize, result: u64) {
    update_flags_szp(flags, operand_size, result);
    flags.set_carry(false);
    flags.set_overflow(false);
}

/// Updates rflags after an add or subtract (or cmp) operation.
fn update_flags_arith(
    flags: &mut RFlags,
    is_add: bool,
    update_carry: bool,
    operand_size: usize,
    result: u64,
    val1: u64,
    val2: u64,
) {
    // Compute the carry bits of the computation.
    let carry_xor = val1 ^ val2 ^ result;
    // Compute the overflow bits of the computation.
    let overflow_xor = (val1 ^ result) & (val1 ^ val2 ^ if is_add { !0 } else { 0 });
    let op_shift = 64 - operand_size as u32 * 8;
    // Extract the high overflow bit.
    let overflow = ((overflow_xor << op_shift) as i64) < 0;
    // Extract the fifth carry bit.
    let aux_carry = carry_xor & 0x10 != 0;

    update_flags_szp(flags, operand_size, result);
    flags.set_overflow(overflow);
    flags.set_adjust(aux_carry);
    if update_carry {
        // Compute the nth carry bit. For 64-bit values, this is gone, but it
        // can be recomputed as the (n-1)th carry bit ^ (n-1)th overflow bit.
        let carry = (((carry_xor ^ overflow_xor) << op_shift) as i64) < 0;
        flags.set_carry(carry);
    }
}

/// Trait for binary arithmetic and comparison ops (add, test, cmp, etc.)
pub(super) trait ArithOp {
    const UPDATES_RESULT: bool;
    fn op(left: u64, right: u64, flags: RFlags) -> u64;
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64, right: u64);
}

/// Trait for unary arithmetic ops (not, neg, etc.)
pub(super) trait UnaryArithOp {
    fn op(left: u64) -> u64;
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64);
}

pub(super) struct CmpOp;
impl ArithOp for CmpOp {
    const UPDATES_RESULT: bool = false;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left.wrapping_sub(right)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64, right: u64) {
        update_flags_arith(flags, false, true, operand_size, result, left, right)
    }
}

pub(super) struct TestOp;
impl ArithOp for TestOp {
    const UPDATES_RESULT: bool = false;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left & right
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, _left: u64, _right: u64) {
        update_flags_logic(flags, operand_size, result)
    }
}

pub(super) struct AndOp;
impl ArithOp for AndOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left & right
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, _left: u64, _right: u64) {
        update_flags_logic(flags, operand_size, result)
    }
}

pub(super) struct OrOp;
impl ArithOp for OrOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left | right
    }

    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, _left: u64, _right: u64) {
        update_flags_logic(flags, operand_size, result)
    }
}

pub(super) struct XorOp;
impl ArithOp for XorOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left ^ right
    }

    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, _left: u64, _right: u64) {
        update_flags_logic(flags, operand_size, result)
    }
}

pub(super) struct AddOp;
impl ArithOp for AddOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left.wrapping_add(right)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64, right: u64) {
        update_flags_arith(flags, true, true, operand_size, result, left, right)
    }
}

pub(super) struct AdcOp;
impl ArithOp for AdcOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, flags: RFlags) -> u64 {
        left.wrapping_add(right).wrapping_add(flags.carry() as u64)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64, right: u64) {
        update_flags_arith(flags, true, true, operand_size, result, left, right)
    }
}

pub(super) struct SubOp;
impl ArithOp for SubOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, _flags: RFlags) -> u64 {
        left.wrapping_sub(right)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64, right: u64) {
        update_flags_arith(flags, false, true, operand_size, result, left, right)
    }
}

pub(super) struct SbbOp;
impl ArithOp for SbbOp {
    const UPDATES_RESULT: bool = true;
    fn op(left: u64, right: u64, flags: RFlags) -> u64 {
        left.wrapping_sub(right.wrapping_add(flags.carry() as u64))
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64, right: u64) {
        update_flags_arith(flags, false, true, operand_size, result, left, right)
    }
}

pub(super) struct IncOp;
impl UnaryArithOp for IncOp {
    fn op(left: u64) -> u64 {
        left.wrapping_add(1)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64) {
        update_flags_arith(flags, true, false, operand_size, result, left, 1)
    }
}

pub(super) struct DecOp;
impl UnaryArithOp for DecOp {
    fn op(left: u64) -> u64 {
        left.wrapping_sub(1)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64) {
        update_flags_arith(flags, false, false, operand_size, result, left, 1)
    }
}

pub(super) struct NegOp;
impl UnaryArithOp for NegOp {
    fn op(left: u64) -> u64 {
        0u64.wrapping_sub(left)
    }
    fn update_flags(flags: &mut RFlags, operand_size: usize, result: u64, left: u64) {
        update_flags_arith(flags, false, true, operand_size, result, 0, left)
    }
}

pub(super) struct NotOp;
impl UnaryArithOp for NotOp {
    fn op(left: u64) -> u64 {
        !left
    }
    fn update_flags(_flags: &mut RFlags, _operand_size: usize, _result: u64, _left: u64) {
        // Flags not affected
    }
}
