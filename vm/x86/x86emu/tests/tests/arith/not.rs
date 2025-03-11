// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use iced_x86::code_asm::*;
use x86defs::RFlags;

#[test]
fn not_memory() {
    let variations = [
        (0x0, 0xffffffff),
        (0x1, 0xfffffffe),
        (0x64, 0xffffff9b),
        (0x7fffffff, 0x80000000),
        (0x7fffffffffffffff, 0x7fffffff00000000),
        (0x80000000, 0x7fffffff),
        (0x8000000000000000, 0x80000000ffffffff),
        (0xffffffff, 0x0),
        (0xffffffffffffffff, 0xffffffff00000000),
    ];

    for (left, result) in variations {
        let cpu = run_lockable_test::<u64>(
            RFlags::new(),
            LockTestBehavior::Fail,
            |asm| asm.not(dword_ptr(0x100)),
            |cpu| {
                cpu.valid_gva = 0x100;
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.mem_val, result);
    }
}
