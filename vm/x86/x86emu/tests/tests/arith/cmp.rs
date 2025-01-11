// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::Cpu;
use x86emu::Gp;

#[test]
fn cmp_memory_to_regvalue() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x64, 0x64, 0x44),
        (0x1, 0x0, 0x0),
        (0x0, 0x1, 0x95),
        (0x0, 0xffffffffffffffff, 0x11),
        (0xffffffff, 0xffffffffffffffff, 0x44),
        (0xffffffff, 0xffffffff, 0x44),
        (0x0, 0x7fffffffffffffff, 0x11),
        (0x0, 0x7fffffff, 0x91),
        (0x7fffffff, 0x80000000, 0x885),
        (0x7fffffff, 0x8000000000000000, 0x4),
    ];

    for &(left, right, rflags) in &variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.cmp(dword_ptr(rax + 0x10), eax),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), right);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn cmp_memory_to_regvalue64() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x64, 0x64, 0x44),
        (0x1, 0x0, 0x0),
        (0x0, 0x1, 0x95),
        (0x0, 0xffffffffffffffff, 0x11),
        (0xffffffff, 0xffffffffffffffff, 0x5),
        (0xffffffff, 0xffffffff, 0x44),
        (0x0, 0x7fffffff, 0x91),
        (0x0, 0x7fffffffffffffff, 0x91),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0x44),
        (0x7fffffffffffffff, 0x8000000000000000, 0x885),
        (0x8000000000000000, 0x8000000000000000, 0x44),
    ];

    for &(left, right, rflags) in &variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.cmp(qword_ptr(rax + 0x10), rax),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), right);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn cmp_memory_to_regvalue_byte() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x64, 0x64, 0x44),
        (0x0, 0x1, 0x95),
        (0x1, 0x0, 0x0),
        (0xff, 0x0, 0x84),
        (0xffff, 0x0, 0x84),
        (0xffffffff, 0x0, 0x84),
        (0xffffffffffffffff, 0x0, 0x84),
        (0x0, 0xff, 0x11),
        (0x0, 0xffff, 0x11),
        (0x0, 0xffffffff, 0x11),
        (0x0, 0xffffffffffffffff, 0x11),
        (0x7f, 0x0, 0x0),
        (0x0, 0x7f, 0x95),
        (0x80, 0x0, 0x80),
        (0x0, 0x80, 0x881),
        (0x7f, 0x80, 0x885),
        (0x80, 0x7f, 0x810),
    ];

    for &(left, right, rflags) in &variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.cmp(al, byte_ptr(rax + 0x10)),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), left);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn cmp_regvalue_to_memory() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x64, 0x64, 0x44),
        (0x0, 0x1, 0x95),
        (0x1, 0x0, 0x0),
        (0xffffffffffffffff, 0x0, 0x84),
        (0xffffffffffffffff, 0xffffffff, 0x44),
        (0xffffffff, 0xffffffffffffffff, 0x44),
        (0xffffffff, 0xffffffff, 0x44),
        (0x7fffffffffffffff, 0x0, 0x84),
        (0x7fffffff, 0x0, 0x4),
        (0x0, 0x7fffffff, 0x91),
        (0x80000000, 0x7fffffff, 0x810),
        (0x7fffffff, 0x80000000, 0x885),
        (0x8000000000000000, 0x7fffffff, 0x91),
        (0x7fffffff, 0x8000000000000000, 0x4),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0x44),
        (0x8000000000000000, 0x7fffffffffffffff, 0x11),
        (0x8000000000000000, 0x8000000000000000, 0x44),
    ];

    for &(left, right, rflags) in &variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.cmp(eax, dword_ptr(rax + 0x10)),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), left);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn cmp_regvalue_to_memory64() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x64, 0x64, 0x44),
        (0x0, 0x1, 0x95),
        (0x1, 0x0, 0x0),
        (0xffffffffffffffff, 0x0, 0x84),
        (0xffffffffffffffff, 0xffffffff, 0x84),
        (0xffffffff, 0xffffffffffffffff, 0x5),
        (0xffffffff, 0xffffffff, 0x44),
        (0x7fffffffffffffff, 0x0, 0x4),
        (0x7fffffff, 0x0, 0x4),
        (0x0, 0x7fffffff, 0x91),
        (0x80000000, 0x7fffffff, 0x10),
        (0x7fffffff, 0x80000000, 0x85),
        (0x8000000000000000, 0x7fffffff, 0x810),
        (0x7fffffff, 0x8000000000000000, 0x885),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0x44),
        (0x8000000000000000, 0x7fffffffffffffff, 0x810),
        (0x8000000000000000, 0x8000000000000000, 0x44),
    ];

    for &(left, right, rflags) in &variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.cmp(rax, qword_ptr(rax + 0x10)),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), left);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn cmp_memory_to_byte() {
    let variations = [
        (0x0, 0x0u32, 0x44),
        (0x64, 0x64, 0x44),
        (0x1, 0x0, 0x0),
        (0x0, 0x1, 0x95),
        (0x0, 0xffffffff, 0x11),
        (0xff, 0xffffffff, 0x5),
        (0xffff, 0xffffffff, 0x5),
        (0xffffffff, 0xffffffff, 0x44),
        (0xffffffffffffffff, 0xffffffff, 0x44),
        (0x0, 0xffffff80, 0x1),
        (0x80, 0xffffff80, 0x5),
        (0x0, 0x7f, 0x95),
        (0x7f, 0x7f, 0x44),
        (0x7f, 0xffffff80, 0x5),
    ];

    for &(left, right, rflags) in &variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.cmp(dword_ptr(rax), right),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), 0x1234);
                cpu.valid_gva = cpu.gp(Gp::RAX.into());
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}
