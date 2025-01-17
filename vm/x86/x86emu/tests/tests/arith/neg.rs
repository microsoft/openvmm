// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::Cpu;

#[test]
fn neg_memory() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x1, 0xffffffff, 0x95),
        (0x64, 0xffffff9c, 0x95),
        (0x7fffffff, 0x80000001, 0x91),
        (0x7fffffffffffffff, 0x7fffffff00000001, 0x11),
        (0x80000000, 0x80000000, 0x885),
        (0x8000000000000000, 0x8000000000000000, 0x44),
        (0xffffffff, 0x1, 0x11),
        (0xffffffffffffffff, 0xffffffff00000001, 0x11),
    ];

    for (left, result, rflags) in variations {
        let mut cpu = run_lockable_test::<u64>(
            RFLAGS_ARITH_MASK,
            LockTestBehavior::Fail,
            |asm| asm.neg(dword_ptr(0x100)),
            |cpu| {
                cpu.valid_gva = 0x100;
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.mem_val, result);
        assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, rflags.into());
    }
}
