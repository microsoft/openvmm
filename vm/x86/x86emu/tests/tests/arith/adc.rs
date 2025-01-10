// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::Cpu;
use x86emu::Gp;

fn adc_regvalue_to_memory(variations: &[(u64, u64, u64, u64)], carry: bool) {
    for &(left, right, result, rflags) in variations {
        let mut cpu = run_test(
            RFLAGS_ARITH_MASK,
            |asm| asm.adc(eax, dword_ptr(rax + 0x10)),
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
fn adc_without_carry() {
    let variations = [
        (0x0, 0x0, 0x0, 0x44),
        (0x64, 0x64, 0xc8, 0x0),
        (0x0, 0x1, 0x1, 0x0),
        (0x1, 0x0, 0x1, 0x0),
        (0xffffffffffffffff, 0x0, 0xffffffff, 0x84),
        (0xffffffffffffffff, 0xffffffff, 0xfffffffe, 0x91),
        (0xffffffff, 0xffffffffffffffff, 0xfffffffe, 0x91),
        (0xffffffff, 0xffffffff, 0xfffffffe, 0x91),
        (0x7fffffffffffffff, 0x0, 0xffffffff, 0x84),
        (0x7fffffff, 0x0, 0x7fffffff, 0x4),
        (0x0, 0x7fffffff, 0x7fffffff, 0x4),
        (0x80000000, 0x7fffffff, 0xffffffff, 0x84),
        (0x7fffffff, 0x80000000, 0xffffffff, 0x84),
        (0x8000000000000000, 0x7fffffff, 0x7fffffff, 0x4),
        (0x7fffffff, 0x8000000000000000, 0x7fffffff, 0x4),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0xfffffffe, 0x91),
        (0x8000000000000000, 0x7fffffffffffffff, 0xffffffff, 0x84),
        (0x8000000000000000, 0x8000000000000000, 0x0, 0x44),
    ];

    adc_regvalue_to_memory(&variations, false);
}

#[test]
fn adc_with_carry() {
    let variations = [
        (0x0, 0x0, 0x1, 0x0),
        (0x64, 0x64, 0xc9, 0x4),
        (0x0, 0x1, 0x2, 0x0),
        (0x1, 0x0, 0x2, 0x0),
        (0xffffffffffffffff, 0x0, 0x0, 0x55),
        (0xffffffffffffffff, 0xffffffff, 0xffffffff, 0x95),
        (0xffffffff, 0xffffffffffffffff, 0xffffffff, 0x95),
        (0xffffffff, 0xffffffff, 0xffffffff, 0x95),
        (0x7fffffffffffffff, 0x0, 0x0, 0x55),
        (0x7fffffff, 0x0, 0x80000000, 0x894),
        (0x0, 0x7fffffff, 0x80000000, 0x894),
        (0x80000000, 0x7fffffff, 0x0, 0x55),
        (0x7fffffff, 0x80000000, 0x0, 0x55),
        (0x8000000000000000, 0x7fffffff, 0x80000000, 0x894),
        (0x7fffffff, 0x8000000000000000, 0x80000000, 0x894),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0xffffffff, 0x95),
        (0x8000000000000000, 0x7fffffffffffffff, 0x0, 0x55),
        (0x8000000000000000, 0x8000000000000000, 0x1, 0x0),
    ];

    adc_regvalue_to_memory(&variations, true);
}
