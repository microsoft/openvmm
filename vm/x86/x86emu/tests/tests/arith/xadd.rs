// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::Cpu;
use x86emu::Gp;

#[test]
fn xadd_regvalue_to_memory() {
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

    for (left, right, result, rflags) in variations {
        let mut cpu = run_lockable_test::<u64>(
            RFLAGS_ARITH_MASK,
            LockTestBehavior::Fail,
            |asm| asm.xadd(dword_ptr(512), eax),
            |cpu| {
                cpu.set_gp(Gp::RAX.into(), right);
                cpu.valid_gva = 512;
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), left as u32 as u64);
        assert_eq!(cpu.mem_val as u32 as u64, result);
        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}

#[test]
fn xadd_to_same_reg() {
    run_lockable_test(
        RFLAGS_ARITH_MASK,
        LockTestBehavior::Fail,
        |asm| asm.xadd(dword_ptr(rax), eax),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x100);
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0x200u64;
        },
    );
    // The entirety of this test is making sure that we don't attempt to access a different second gva,
    // no asserts are needed.
}
