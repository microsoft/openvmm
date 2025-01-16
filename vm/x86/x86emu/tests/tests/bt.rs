// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use iced_x86::code_asm::asm_traits::*;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

/// The mask of flags that are changed by bt(x) operations.
const RFLAGS_BT_MASK: RFlags = RFlags::new().with_carry(true);

trait BtInstr {
    const CAN_LOCK: bool = true;

    fn op(&mut self, value: u64) -> u64;

    fn instr<T, U>(&self, asm: &mut CodeAssembler, op0: T, op1: U) -> Result<(), IcedError>
    where
        CodeAssembler: CodeAsmBt<T, U> + CodeAsmBtr<T, U> + CodeAsmBtc<T, U> + CodeAsmBts<T, U>;
}

fn btx<T: BtInstr>(mut instr: T) {
    let variations: &[(u64, u64, bool, i32, u64, bool)] = &[
        // Register form only (offset out of immediate range):
        (
            0xfb6c6c6c,
            0xf3f3f3f3f3f3f3f3,
            false,
            -76780436,
            0xfad9fa08,
            true,
        ),
        (0xffff, 0b10, true, -1, 0x1000b, false),
        // Both forms:
        (0xffff, 0b10, true, 2, 0x1000f, false),
        (0x7fffffffffff, 0b0110, true, 2, 0x80000000000f, true),
        (0x7ffffffffffff, 0b1010, true, 35, 0x8000000000013, true),
        (0xffff, 0b10, true, 130, 0x1001f, false),
    ];

    let behavior = if T::CAN_LOCK {
        LockTestBehavior::Fail
    } else {
        LockTestBehavior::DecodeError
    };

    // Immediate.
    for &(base, value, initial_carry, bit, _reg_gva, carry) in &variations[2..] {
        let mut cpu = run_lockable_test(
            RFLAGS_BT_MASK,
            behavior,
            |asm| instr.instr(asm, dword_ptr(rax + 0x10), bit),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), base);
                let mut rflags = cpu.rflags();
                rflags.set_carry(initial_carry);
                cpu.set_rflags(rflags);
                // Note the % and regular division when using the imm form of these instructions
                cpu.valid_gva = base
                    .wrapping_add(0x10)
                    .wrapping_add(u64::try_from(bit).unwrap() % 32 / 8);
                cpu.mem_val = value;
            },
        );

        let mask = 1 << (bit % 8);
        assert_eq!(cpu.rflags().carry(), carry);
        assert_eq!(cpu.mem_val, (instr.op(value) & mask) | (value & !mask));
    }

    // Register.
    for &(base, value, initial_carry, bit, reg_gva, carry) in variations {
        let mut cpu = run_lockable_test(
            RFLAGS_BT_MASK,
            behavior,
            |asm| instr.instr(asm, dword_ptr(rax + 0x10), ebx),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), base);
                cpu.set_gp(Gp::RBX.into(), bit as u64);
                let mut rflags = cpu.rflags();
                rflags.set_carry(initial_carry);
                cpu.set_rflags(rflags);
                cpu.valid_gva = reg_gva;
                cpu.mem_val = value;
            },
        );

        let mask = 1 << (bit.rem_euclid(32));
        assert_eq!(cpu.rflags().carry(), carry);
        assert_eq!(cpu.mem_val, (instr.op(value) & mask) | (value & !mask));
    }
}

#[test]
fn bt() {
    struct Bt;
    impl BtInstr for Bt {
        const CAN_LOCK: bool = false;

        fn op(&mut self, value: u64) -> u64 {
            value
        }

        fn instr<T, U>(&self, asm: &mut CodeAssembler, op0: T, op1: U) -> Result<(), IcedError>
        where
            CodeAssembler: CodeAsmBt<T, U>,
        {
            asm.bt(op0, op1)
        }
    }

    btx(Bt);
}

#[test]
fn btc() {
    struct Btc;
    impl BtInstr for Btc {
        fn op(&mut self, value: u64) -> u64 {
            !value
        }

        fn instr<T, U>(&self, asm: &mut CodeAssembler, op0: T, op1: U) -> Result<(), IcedError>
        where
            CodeAssembler: CodeAsmBtc<T, U>,
        {
            asm.btc(op0, op1)
        }
    }

    btx(Btc);
}

#[test]
fn bts() {
    struct Bts;
    impl BtInstr for Bts {
        fn op(&mut self, _value: u64) -> u64 {
            !0
        }

        fn instr<T, U>(&self, asm: &mut CodeAssembler, op0: T, op1: U) -> Result<(), IcedError>
        where
            CodeAssembler: CodeAsmBts<T, U>,
        {
            asm.bts(op0, op1)
        }
    }

    btx(Bts);
}

#[test]
fn btr() {
    struct Btr;
    impl BtInstr for Btr {
        fn op(&mut self, _value: u64) -> u64 {
            0
        }

        fn instr<T, U>(&self, asm: &mut CodeAssembler, op0: T, op1: U) -> Result<(), IcedError>
        where
            CodeAssembler: CodeAsmBtr<T, U>,
        {
            asm.btr(op0, op1)
        }
    }

    btx(Btr);
}
