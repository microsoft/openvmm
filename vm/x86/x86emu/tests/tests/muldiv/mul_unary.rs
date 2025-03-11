// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

const RFLAGS_MUL_MASK: RFlags = RFlags::new().with_carry(true).with_overflow(true);

fn multiply_regvalue_to_memory(
    variations: &[(u64, u64, u64, Option<u64>, u64)],
    mul_op: impl Fn(&mut CodeAssembler, AsmMemoryOperand) -> Result<(), IcedError>,
    ptr_op: fn(AsmMemoryOperand) -> AsmMemoryOperand,
) {
    for &(left, right, low, high, rflags) in variations {
        let mut cpu = run_test(
            RFLAGS_MUL_MASK,
            |asm| mul_op(asm, ptr_op(rax + 0x10)),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), left);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), low);
        if let Some(high_bits) = high {
            assert_eq!(cpu.gp(Gp::RDX.into()), high_bits);
        }
        assert_eq!(cpu.rflags() & RFLAGS_MUL_MASK, rflags.into());
    }
}

#[test]
fn multiply_regvalue_to_byte_memory() {
    let variations = [
        (0x1, 0x0, 0x0, None, 0x0),
        (0x1, 0x2, 0x2, None, 0x0),
        (0xffffffffffffffff, 0x0, 0xffffffffffff0000, None, 0x0),
        (
            0xffffffffffffffff,
            0xffffffff,
            0xfffffffffffffe01,
            None,
            0x801,
        ),
        (0xffffffff, 0xffffffffffffffff, 0xfffffe01, None, 0x801),
        (0xffffffff, 0xffffffff, 0xfffffe01, None, 0x801),
        (0x7fffffffffffffff, 0x0, 0x7fffffffffff0000, None, 0x0),
        (0x7fffffff, 0x0, 0x7fff0000, None, 0x0),
        (0x0, 0x7fffffff, 0x0, None, 0x0),
        (0x80000000, 0x7fffffff, 0x80000000, None, 0x0),
        (0x7fffffff, 0x80000000, 0x7fff0000, None, 0x0),
        (
            0x8000000000000000,
            0x7fffffff,
            0x8000000000000000,
            None,
            0x0,
        ),
        (0x7fffffff, 0x8000000000000000, 0x7fff0000, None, 0x0),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x7ffffffffffffe01,
            None,
            0x801,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            None,
            0x0,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            0x8000000000000000,
            None,
            0x0,
        ),
    ];

    multiply_regvalue_to_memory(&variations, CodeAssembler::mul, byte_ptr);
}

#[test]
fn multiply_regvalue_to_word_memory() {
    let variations = [
        (0x0, 0x0, 0x0, Some(0xbadc0ffee0dd0000), 0x0),
        (0x1, 0x1, 0x1, Some(0xbadc0ffee0dd0000), 0x0),
        (0x64, 0x64, 0x2710, Some(0xbadc0ffee0dd0000), 0x0),
        (0x0, 0x1, 0x0, Some(0xbadc0ffee0dd0000), 0x0),
        (0x1, 0x0, 0x0, Some(0xbadc0ffee0dd0000), 0x0),
        (0x1, 0x2, 0x2, Some(0xbadc0ffee0dd0000), 0x0),
        (
            0xffffffffffffffff,
            0x0,
            0xffffffffffff0000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (
            0xffffffffffffffff,
            0xffffffff,
            0xffffffffffff0001,
            Some(0xbadc0ffee0ddfffe),
            0x801,
        ),
        (
            0xffffffff,
            0xffffffffffffffff,
            0xffff0001,
            Some(0xbadc0ffee0ddfffe),
            0x801,
        ),
        (
            0xffffffff,
            0xffffffff,
            0xffff0001,
            Some(0xbadc0ffee0ddfffe),
            0x801,
        ),
        (
            0x7fffffffffffffff,
            0x0,
            0x7fffffffffff0000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (0x7fffffff, 0x0, 0x7fff0000, Some(0xbadc0ffee0dd0000), 0x0),
        (0x0, 0x7fffffff, 0x0, Some(0xbadc0ffee0dd0000), 0x0),
        (
            0x80000000,
            0x7fffffff,
            0x80000000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (
            0x7fffffff,
            0x80000000,
            0x7fff0000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (
            0x8000000000000000,
            0x7fffffff,
            0x8000000000000000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (
            0x7fffffff,
            0x8000000000000000,
            0x7fff0000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x7fffffffffff0001,
            Some(0xbadc0ffee0ddfffe),
            0x801,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            0x8000000000000000,
            Some(0xbadc0ffee0dd0000),
            0x0,
        ),
    ];

    multiply_regvalue_to_memory(&variations, CodeAssembler::mul, word_ptr);
}

#[test]
fn multiply_regvalue_to_dword_memory() {
    let variations = [
        (0x0, 0x0, 0x0, Some(0x0), 0x0),
        (0x1, 0x1, 0x1, Some(0x0), 0x0),
        (0x64, 0x64, 0x2710, Some(0x0), 0x0),
        (0x0, 0x1, 0x0, Some(0x0), 0x0),
        (0x1, 0x0, 0x0, Some(0x0), 0x0),
        (0x1, 0x2, 0x2, Some(0x0), 0x0),
        (0xffffffffffffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0xffffffffffffffff, 0xffffffff, 0x1, Some(0xfffffffe), 0x801),
        (0xffffffff, 0xffffffffffffffff, 0x1, Some(0xfffffffe), 0x801),
        (0xffffffff, 0xffffffff, 0x1, Some(0xfffffffe), 0x801),
        (0x7fffffffffffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0x7fffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0x0, 0x7fffffff, 0x0, Some(0x0), 0x0),
        (0x80000000, 0x7fffffff, 0x80000000, Some(0x3fffffff), 0x801),
        (0x7fffffff, 0x80000000, 0x80000000, Some(0x3fffffff), 0x801),
        (0x8000000000000000, 0x7fffffff, 0x0, Some(0x0), 0x0),
        (0x7fffffff, 0x8000000000000000, 0x0, Some(0x0), 0x0),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x1,
            Some(0xfffffffe),
            0x801,
        ),
        (0x8000000000000000, 0x7fffffffffffffff, 0x0, Some(0x0), 0x0),
        (0x8000000000000000, 0x8000000000000000, 0x0, Some(0x0), 0x0),
    ];

    multiply_regvalue_to_memory(&variations, CodeAssembler::mul, dword_ptr);
}

#[test]
fn multiply_regvalue_to_qword_memory() {
    let variations = [
        (0x0, 0x0, 0x0, Some(0x0), 0x0),
        (0x1, 0x1, 0x1, Some(0x0), 0x0),
        (0x64, 0x64, 0x2710, Some(0x0), 0x0),
        (0x0, 0x1, 0x0, Some(0x0), 0x0),
        (0x1, 0x0, 0x0, Some(0x0), 0x0),
        (0x1, 0x2, 0x2, Some(0x0), 0x0),
        (0xffffffffffffffff, 0x0, 0x0, Some(0x0), 0x0),
        (
            0xffffffffffffffff,
            0xffffffff,
            0xffffffff00000001,
            Some(0xfffffffe),
            0x801,
        ),
        (
            0xffffffff,
            0xffffffffffffffff,
            0xffffffff00000001,
            Some(0xfffffffe),
            0x801,
        ),
        (0xffffffff, 0xffffffff, 0xfffffffe00000001, Some(0x0), 0x0),
        (0x7fffffffffffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0x7fffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0x0, 0x7fffffff, 0x0, Some(0x0), 0x0),
        (0x80000000, 0x7fffffff, 0x3fffffff80000000, Some(0x0), 0x0),
        (0x7fffffff, 0x80000000, 0x3fffffff80000000, Some(0x0), 0x0),
        (
            0x8000000000000000,
            0x7fffffff,
            0x8000000000000000,
            Some(0x3fffffff),
            0x801,
        ),
        (
            0x7fffffff,
            0x8000000000000000,
            0x8000000000000000,
            Some(0x3fffffff),
            0x801,
        ),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x1,
            Some(0x3fffffffffffffff),
            0x801,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            Some(0x3fffffffffffffff),
            0x801,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            0x0,
            Some(0x4000000000000000),
            0x801,
        ),
    ];

    multiply_regvalue_to_memory(&variations, CodeAssembler::mul, qword_ptr);
}

#[test]
fn signed_multiply_regvalue_to_byte_memory() {
    let variations = [
        (0x0, 0x0, 0x0, None, 0x0),
        (0x1, 0x1, 0x1, None, 0x0),
        (0x64, 0x64, 0x2710, None, 0x801),
        (0x0, 0x1, 0x0, None, 0x0),
        (0x1, 0x0, 0x0, None, 0x0),
        (0x1, 0x2, 0x2, None, 0x0),
        (0xffffffffffffffff, 0x0, 0xffffffffffff0000, None, 0x0),
        (
            0xffffffffffffffff,
            0xffffffff,
            0xffffffffffff0001,
            None,
            0x0,
        ),
        (0xffffffff, 0xffffffffffffffff, 0xffff0001, None, 0x0),
        (0xffffffff, 0xffffffff, 0xffff0001, None, 0x0),
        (0x7fffffffffffffff, 0x0, 0x7fffffffffff0000, None, 0x0),
        (0x7fffffff, 0x0, 0x7fff0000, None, 0x0),
        (0x0, 0x7fffffff, 0x0, None, 0x0),
        (0x80000000, 0x7fffffff, 0x80000000, None, 0x0),
        (0x7fffffff, 0x80000000, 0x7fff0000, None, 0x0),
        (
            0x8000000000000000,
            0x7fffffff,
            0x8000000000000000,
            None,
            0x0,
        ),
        (0x7fffffff, 0x8000000000000000, 0x7fff0000, None, 0x0),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x7fffffffffff0001,
            None,
            0x0,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            None,
            0x0,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            0x8000000000000000,
            None,
            0x0,
        ),
    ];

    multiply_regvalue_to_memory(&variations, CodeAssembler::imul, byte_ptr);
}

#[test]
fn signed_multiply_regvalue_to_qword_memory() {
    let variations = [
        (0x0, 0x0, 0x0, Some(0x0), 0x0),
        (0x1, 0x1, 0x1, Some(0x0), 0x0),
        (0x64, 0x64, 0x2710, Some(0x0), 0x0),
        (0x0, 0x1, 0x0, Some(0x0), 0x0),
        (0x1, 0x0, 0x0, Some(0x0), 0x0),
        (0x1, 0x2, 0x2, Some(0x0), 0x0),
        (0xffffffffffffffff, 0x0, 0x0, Some(0x0), 0x0),
        (
            0xffffffffffffffff,
            0xffffffff,
            0xffffffff00000001,
            Some(0xffffffffffffffff),
            0x0,
        ),
        (
            0xffffffff,
            0xffffffffffffffff,
            0xffffffff00000001,
            Some(0xffffffffffffffff),
            0x0,
        ),
        (0xffffffff, 0xffffffff, 0xfffffffe00000001, Some(0x0), 0x801),
        (0x7fffffffffffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0x7fffffff, 0x0, 0x0, Some(0x0), 0x0),
        (0x0, 0x7fffffff, 0x0, Some(0x0), 0x0),
        (0x80000000, 0x7fffffff, 0x3fffffff80000000, Some(0x0), 0x0),
        (0x7fffffff, 0x80000000, 0x3fffffff80000000, Some(0x0), 0x0),
        (
            0x8000000000000000,
            0x7fffffff,
            0x8000000000000000,
            Some(0xffffffffc0000000),
            0x801,
        ),
        (
            0x7fffffff,
            0x8000000000000000,
            0x8000000000000000,
            Some(0xffffffffc0000000),
            0x801,
        ),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x1,
            Some(0x3fffffffffffffff),
            0x801,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            Some(0xc000000000000000),
            0x801,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            0x0,
            Some(0x4000000000000000),
            0x801,
        ),
    ];

    multiply_regvalue_to_memory(&variations, CodeAssembler::imul, qword_ptr);
}
