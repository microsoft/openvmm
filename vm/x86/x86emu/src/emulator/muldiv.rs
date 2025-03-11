// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Emulator;
use super::Error;
use super::InternalError;
use crate::Cpu;
use iced_x86::Instruction;
use iced_x86::Register;

impl<T: Cpu> Emulator<'_, T> {
    // MUL rm instructions
    pub(super) async fn unary_mul(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let right = self.op_value(instr, 0).await? as u128;
        self.do_unary_mul(instr, |left, operand_bit_size| {
            let prod = left * right;
            let flag = prod & (u128::MAX << operand_bit_size) != 0;
            (prod, flag)
        })
    }

    // IMUL rm instructions
    pub(super) async fn unary_imul(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let right = self.op_value_sign_extend(instr, 0).await? as i128;
        self.do_unary_mul(instr, |left, operand_bit_size| {
            let sign_shift = 128 - operand_bit_size;
            let left = ((left as i128) << sign_shift) >> sign_shift;
            let prod = left * right;
            let flag = prod != (prod << sign_shift) >> sign_shift;
            (prod as u128, flag)
        })
    }

    fn do_unary_mul(
        &mut self,
        instr: &Instruction,
        do_multiply: impl Fn(u128, usize) -> (u128, bool),
    ) -> Result<(), InternalError<T::Error>> {
        let operand_bit_size = instr.memory_size().size() * 8;

        let (high_register, low_register) = unary_register_pair(operand_bit_size);

        let left = self.cpu.gp(low_register.into()) as u128;

        let (product, flag) = do_multiply(left, operand_bit_size);

        let high_mask = u128::MAX << operand_bit_size;
        let product_high = ((product & high_mask) >> operand_bit_size) as u64;
        let product_low = (!high_mask & product) as u64;

        self.cpu.set_gp(low_register.into(), product_low);
        self.cpu.set_gp(high_register.into(), product_high);

        let mut rflags = self.cpu.rflags();
        rflags.set_carry(flag);
        rflags.set_overflow(flag);
        self.cpu.set_rflags(rflags);

        Ok(())
    }

    // IMUL r, rm(, imm) instructions
    pub(super) async fn imul(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let left_op = match instr.op_count() {
            2 => 0,
            3 => 2,
            _ => unreachable!(),
        };
        let left = self.op_value_sign_extend(instr, left_op).await?;
        let right = self.op_value_sign_extend(instr, 1).await?;

        let (result, overflow) = left.overflowing_mul(right);

        let sign_shift = 64 - (instr.memory_size().size() * 8);
        let smaller_overflow = result != (result << sign_shift) >> sign_shift;

        let flag = overflow || smaller_overflow;

        self.write_op_0(instr, result as u64).await?;
        let mut rflags = self.cpu.rflags();
        rflags.set_carry(flag);
        rflags.set_overflow(flag);
        self.cpu.set_rflags(rflags);

        Ok(())
    }

    // DIV rm instructions
    pub(super) async fn unary_div(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let right = self.op_value(instr, 0).await? as u128;

        self.do_unary_div(instr, right, |left, right, operand_bit_size| {
            let quotient = left / right;
            let remainder = left % right;

            let max_quotient = u128::MAX >> (128 - operand_bit_size);
            if quotient > max_quotient {
                return Err(());
            }

            Ok((quotient as u64, remainder as u64))
        })
    }

    // IDIV rm instructions
    pub(super) async fn unary_idiv(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let right = self.op_value_sign_extend(instr, 0).await? as i128;

        self.do_unary_div(instr, right, |left, right, operand_bit_size| {
            let sign_shift = 128 - (2 * operand_bit_size);
            let left = ((left as i128) << sign_shift) >> sign_shift;

            if left == i128::MIN && right == -1 {
                return Err(());
            }

            let quotient = left / right;
            let remainder = left % right;
            let max_quotient = i128::MAX >> (128 - operand_bit_size);
            let min_quotient = i128::MIN >> (128 - operand_bit_size);

            if quotient > max_quotient || quotient < min_quotient {
                return Err(());
            }

            Ok((quotient as u64, remainder as u64))
        })
    }

    fn do_unary_div<R: From<u8> + PartialEq<R>>(
        &mut self,
        instr: &Instruction,
        right: R,
        do_division: impl Fn(u128, R, usize) -> Result<(u64, u64), ()>,
    ) -> Result<(), InternalError<T::Error>> {
        if right == R::from(0) {
            Err(Error::InstructionException(
                x86defs::Exception::DIVIDE_ERROR,
                None,
                super::ExceptionCause::DivideByZero,
            ))?;
        }

        let operand_bit_size = instr.memory_size().size() * 8;

        let (high_register, low_register) = unary_register_pair(operand_bit_size);

        let left_high_bits = self.cpu.gp(high_register.into()) as u128;
        let left_low_bits = self.cpu.gp(low_register.into()) as u128;
        let left = (left_high_bits << operand_bit_size) | left_low_bits;

        let (quotient, remainder) = do_division(left, right, operand_bit_size).map_err(|_| {
            Error::InstructionException(
                x86defs::Exception::DIVIDE_ERROR,
                None,
                super::ExceptionCause::DivideOverflow,
            )
        })?;

        self.cpu.set_gp(low_register.into(), quotient);
        self.cpu.set_gp(high_register.into(), remainder);

        // flags are undefined
        Ok(())
    }
}

fn unary_register_pair(operand_bit_size: usize) -> (Register, Register) {
    match operand_bit_size {
        8 => (Register::AH, Register::AL),
        16 => (Register::DX, Register::AX),
        32 => (Register::EDX, Register::EAX),
        64 => (Register::RDX, Register::RAX),
        _ => unreachable!(),
    }
}
