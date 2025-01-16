// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

const RFLAGS_DIV_MASK: RFlags = RFlags::new();

fn divide_regvalue_by_memory(
    variations: &[(Option<u64>, u64, u64, u64, u64)],
    div_op: impl Fn(&mut CodeAssembler, AsmMemoryOperand) -> Result<(), IcedError>,
    ptr_op: impl Fn(AsmMemoryOperand) -> AsmMemoryOperand,
) {
    for &(left_high, left_low, right, quotient, remainder) in variations {
        let mut cpu = run_test(
            RFLAGS_DIV_MASK,
            |asm| div_op(asm, ptr_op(rax + 0x10)),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), left_low);
                cpu.set_gp(Gp::RDX.into(), (left_high).unwrap_or(0));
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), quotient);
        assert_eq!(cpu.gp(Gp::RDX.into()), remainder);
    }
}

#[test]
#[should_panic(expected = "DivideByZero")]
fn divide_regvalue_by_zero() {
    let variations = [(None, 0xffffffffffffffff, 0x0, 0x0, 0x0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, byte_ptr);
}

#[test]
fn divide_regvalue_by_byte_memory() {
    let variations = [
        (None, 0x1, 0x1, 0x1, 0),
        (None, 0x64, 0x64, 0x1, 0),
        (None, 0x0, 0x1, 0x0, 0),
        (None, 0x1, 0x2, 0x100, 0),
        (None, 0x0, 0x7fffffff, 0x0, 0),
        (None, 0x80000000, 0x7fffffff, 0x80000000, 0),
        (None, 0x8000000000000000, 0x7fffffff, 0x8000000000000000, 0),
        (
            None,
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            0,
        ),
        (None, 0x0, 0xffffffffffffffff, 0x0, 0),
    ];

    divide_regvalue_by_memory(&variations, CodeAssembler::div, byte_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn divide_regvalue_by_byte_overflow() {
    let variations = [(None, 0xffffffffffffffff, 0xffffffff, 0x0, 0x0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, byte_ptr);
}

#[test]
fn divide_regvalue_by_word_memory() {
    let variations = [
        (Some(0x0), 0x1, 0x1, 0x1, 0x0),
        (Some(0x0), 0x64, 0x64, 0x1, 0x0),
        (Some(0x0), 0x0, 0x1, 0x0, 0x0),
        (Some(0x0), 0x1, 0x2, 0x0, 0x1),
        (Some(0x0), 0x0, 0x7fffffff, 0x0, 0x0),
        (Some(0x8000), 0x0, 0x7fffffff, 0x8000, 0x8000),
        (Some(0x800000000000), 0x0, 0x7fffffff, 0x0, 0x800000000000),
        (
            Some(0x800000000000),
            0x0,
            0x7fffffffffffffff,
            0x0,
            0x800000000000,
        ),
        (Some(0x0), 0x0, 0x7fffffff, 0x0, 0x0),
        (Some(0x0), 0x0, 0xffffffffffffffff, 0x0, 0x0),
    ];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, word_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn divide_regvalue_by_word_overflow() {
    let variations = [(Some(0xffffffffffffffff), 0xffff, 0xffffffff, 0x0, 0x0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, byte_ptr);
}

#[test]
fn divide_regvalue_by_dword_memory() {
    let variations = [
        (Some(0x0), 0x1, 0x1, 0x1, 0x0),
        (Some(0x0), 0x64, 0x64, 0x1, 0x0),
        (Some(0x0), 0x0, 0x1, 0x0, 0x0),
        (Some(0x0), 0x1, 0x2, 0x0, 0x1),
        (Some(0x0), 0x0, 0x7fffffff, 0x0, 0x0),
        (Some(0x0), 0x80000000, 0x7fffffff, 0x1, 0x1),
        (
            Some(0x80000000),
            0x0,
            0x7fffffffffffffff,
            0x80000000,
            0x80000000,
        ),
        (Some(0x0), 0xffffffff, 0xffffffffffffffff, 0x1, 0x0),
        (Some(0x0), 0xffffffff, 0xffffffff, 0x1, 0x0),
        (Some(0x0), 0x7fffffff, 0x80000000, 0x0, 0x7fffffff),
        (
            Some(0x7fffffff),
            0xffffffff,
            0x7fffffffffffffff,
            0x80000000,
            0x7fffffff,
        ),
        (Some(0x0), 0x0, 0x7fffffff, 0x0, 0x0),
        (Some(0x0), 0x0, 0xffffffffffffffff, 0x0, 0x0),
    ];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, dword_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn divide_regvalue_by_dword_overflow() {
    let variations = [(Some(0xffffffff), 0xffffffff, 0xffffffff, 0x0, 0x0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, byte_ptr);
}

#[test]
fn divide_regvalue_by_qword_memory() {
    let variations = [
        (Some(0x0), 0x1, 0x1, 0x1, 0x0),
        (Some(0x0), 0x64, 0x64, 0x1, 0x0),
        (Some(0x0), 0x0, 0x1, 0x0, 0x0),
        (Some(0x0), 0x1, 0x2, 0x0, 0x1),
        (Some(0x0), 0x0, 0x7fffffff, 0x0, 0x0),
        (Some(0x0), 0x80000000, 0x7fffffff, 0x1, 0x1),
        (Some(0x0), 0x8000000000000000, 0x7fffffffffffffff, 0x1, 0x1),
        (Some(0x0), 0x8000000000000000, 0x7fffffff, 0x100000002, 0x2),
        (Some(0x0), 0xffffffffffffffff, 0xffffffff, 0x100000001, 0x0),
        (Some(0x0), 0xffffffff, 0xffffffffffffffff, 0x0, 0xffffffff),
        (Some(0x0), 0xffffffff, 0xffffffff, 0x1, 0x0),
        (Some(0x0), 0x7fffffff, 0x80000000, 0x0, 0x7fffffff),
        (Some(0x0), 0x7fffffff, 0x8000000000000000, 0x0, 0x7fffffff),
        (Some(0x0), 0x7fffffffffffffff, 0x7fffffffffffffff, 0x1, 0x0),
        (Some(0x0), 0x8000000000000000, 0x8000000000000000, 0x1, 0x0),
        (
            Some(0x8000000000000000),
            0x0,
            0xffffffffffffffff,
            0x8000000000000000,
            0x8000000000000000,
        ),
    ];

    divide_regvalue_by_memory(&variations, CodeAssembler::div, qword_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn divide_regvalue_by_qword_overflow() {
    let variations = [(
        Some(0xffffffffffffffff),
        0xffffffffffffffff,
        0xffffffff,
        0x0,
        0x0,
    )];
    divide_regvalue_by_memory(&variations, CodeAssembler::div, byte_ptr);
}

#[test]
fn signed_divide_regvalue_by_byte_memory() {
    let variations = [
        (None, 0x1, 0x1, 0x1, 0),
        (None, 0x64, 0x64, 0x1, 0),
        (None, 0x0, 0x1, 0x0, 0),
        (None, 0x1, 0x2, 0x100, 0),
        (None, 0x0, 0x7fffffff, 0x0, 0),
        (None, 0x80000000, 0x7fffffff, 0x80000000, 0),
        (None, 0x8000000000000000, 0x7fffffff, 0x8000000000000000, 0),
        (
            None,
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            0,
        ),
        (None, 0x0, 0xffffffffffffffff, 0x0, 0),
    ];

    divide_regvalue_by_memory(&variations, CodeAssembler::idiv, byte_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn signed_divide_regvalue_by_byte_overflow() {
    let variations = [(None, 0xffffffffffffff80, 0xffffffffffffffff, 0, 0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::idiv, byte_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn signed_divide_regvalue_by_byte_underflow() {
    let variations = [(None, 0xffffffffffffff00, 1, 0, 0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::idiv, byte_ptr);
}

#[test]
fn signed_divide_regvalue_by_qword_memory() {
    let variations = [
        (Some(0x0), 0x1, 0x1, 0x1, 0x0),
        (Some(0x0), 0x64, 0x64, 0x1, 0x0),
        (Some(0x0), 0x0, 0x1, 0x0, 0x0),
        (Some(0x0), 0x1, 0x2, 0x0, 0x1),
        (Some(0x0), 0x0, 0x7fffffff, 0x0, 0x0),
        (Some(0x0), 0x80000000, 0x7fffffff, 0x1, 0x1),
        (Some(0x0), 0x8000000000000000, 0x7fffffffffffffff, 0x1, 0x1),
        (Some(0x0), 0x8000000000000000, 0x7fffffff, 0x100000002, 0x2),
        (Some(0x0), 0xffffffffffffffff, 0xffffffff, 0x100000001, 0x0),
        (
            Some(0x0),
            0xffffffff,
            0xffffffffffffffff,
            0xffffffff00000001,
            0x0,
        ),
        (Some(0x0), 0xffffffff, 0xffffffff, 0x1, 0x0),
        (Some(0x0), 0x7fffffff, 0x80000000, 0x0, 0x7fffffff),
        (Some(0x0), 0x7fffffff, 0x8000000000000000, 0x0, 0x7fffffff),
        (Some(0x0), 0x7fffffffffffffff, 0x7fffffffffffffff, 0x1, 0x0),
        (
            Some(0x0),
            0x8000000000000000,
            0x8000000000000000,
            0xffffffffffffffff,
            0x0,
        ),
    ];

    divide_regvalue_by_memory(&variations, CodeAssembler::idiv, qword_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn signed_divide_regvalue_by_qword_overflow() {
    let variations = [(
        Some(0xffffffffffffffff),
        0x8000000000000000,
        0xffffffffffffffff,
        0,
        0,
    )];
    divide_regvalue_by_memory(&variations, CodeAssembler::idiv, qword_ptr);
}

#[test]
#[should_panic(expected = "DivideOverflow")]
fn signed_divide_regvalue_by_qword_underflow() {
    let variations = [(Some(0xffffffffffffffff), 0, 1, 0, 0)];
    divide_regvalue_by_memory(&variations, CodeAssembler::idiv, qword_ptr);
}
