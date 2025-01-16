// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::Cpu;
use x86emu::Gp;

fn sbb_regvalue_to_memory(variations: &[(u64, u64, u64, u64)], carry: bool) {
    for &(left, right, result, rflags) in variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.sbb(eax, dword_ptr(rax + 0x10)),
            |cpu| {
                let mut rflags = cpu.rflags();
                rflags.set_carry(carry);
                cpu.set_rflags(rflags);
                cpu.set_gp(Gp::RAX.into(), left);
                cpu.valid_gva = cpu.gp(Gp::RAX.into()).wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), result);
        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn sbb_without_carry() {
    let variations = [
        (0x0, 0x0, 0x0, 0x44),
        (0x64, 0x64, 0x0, 0x44),
        (0x0, 0x1, 0xffffffff, 0x95),
        (0x1, 0x0, 0x1, 0x0),
        (0xffffffffffffffff, 0x0, 0xffffffff, 0x84),
        (0xffffffffffffffff, 0xffffffff, 0x0, 0x44),
        (0xffffffff, 0xffffffffffffffff, 0x0, 0x44),
        (0xffffffff, 0xffffffff, 0x0, 0x44),
        (0x7fffffffffffffff, 0x0, 0xffffffff, 0x84),
        (0x7fffffff, 0x0, 0x7fffffff, 0x4),
        (0x0, 0x7fffffff, 0x80000001, 0x91),
        (0x80000000, 0x7fffffff, 0x1, 0x810),
        (0x7fffffff, 0x80000000, 0xffffffff, 0x885),
        (0x8000000000000000, 0x7fffffff, 0x80000001, 0x91),
        (0x7fffffff, 0x8000000000000000, 0x7fffffff, 0x4),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0x0, 0x44),
        (0x8000000000000000, 0x7fffffffffffffff, 0x1, 0x11),
        (0x8000000000000000, 0x8000000000000000, 0x0, 0x44),
    ];

    sbb_regvalue_to_memory(&variations, false);
}

#[test]
fn sbb_with_carry() {
    let variations = [
        (0x0, 0x0, 0xffffffff, 0x95),
        (0x64, 0x64, 0xffffffff, 0x95),
        (0x0, 0x1, 0xfffffffe, 0x91),
        (0x1, 0x0, 0x0, 0x44),
        (0xffffffffffffffff, 0x0, 0xfffffffe, 0x80),
        (0xffffffffffffffff, 0xffffffff, 0xffffffff, 0x95),
        (0xffffffff, 0xffffffffffffffff, 0xffffffff, 0x95),
        (0xffffffff, 0xffffffff, 0xffffffff, 0x95),
        (0x7fffffffffffffff, 0x0, 0xfffffffe, 0x80),
        (0x7fffffff, 0x0, 0x7ffffffe, 0x0),
        (0x0, 0x7fffffff, 0x80000000, 0x95),
        (0x80000000, 0x7fffffff, 0x0, 0x854),
        (0x7fffffff, 0x80000000, 0xfffffffe, 0x881),
        (0x8000000000000000, 0x7fffffff, 0x80000000, 0x95),
        (0x7fffffff, 0x8000000000000000, 0x7ffffffe, 0x0),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0xffffffff, 0x95),
        (0x8000000000000000, 0x7fffffffffffffff, 0x0, 0x55),
        (0x8000000000000000, 0x8000000000000000, 0xffffffff, 0x95),
    ];

    sbb_regvalue_to_memory(&variations, true);
}
