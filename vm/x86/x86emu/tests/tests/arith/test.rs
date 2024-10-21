// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use crate::tests::common::RFLAGS_LOGIC_MASK;
use iced_x86::code_asm::*;
use x86emu::CpuState;

#[test]
fn test_regvalue_to_memory() {
    let variations = [
        (0x0, 0x0, 0x44),
        (0x64, 0x64, 0x0),
        (0x0, 0x1, 0x44),
        (0x1, 0x0, 0x44),
        (0xffffffffffffffff, 0x0, 0x44),
        (0xffffffffffffffff, 0xffffffff, 0x84),
        (0xffffffff, 0xffffffffffffffff, 0x84),
        (0xffffffff, 0xffffffff, 0x84),
        (0x7fffffffffffffff, 0x0, 0x44),
        (0x7fffffff, 0x0, 0x44),
        (0x0, 0x7fffffff, 0x44),
        (0x80000000, 0x7fffffff, 0x44),
        (0x7fffffff, 0x80000000, 0x44),
        (0x8000000000000000, 0x7fffffff, 0x44),
        (0x7fffffff, 0x8000000000000000, 0x44),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0x84),
        (0x8000000000000000, 0x7fffffffffffffff, 0x44),
        (0x8000000000000000, 0x8000000000000000, 0x44),
    ];

    for (left, right, rflags) in variations {
        let (state, _cpu) = run_test(
            RFLAGS_LOGIC_MASK,
            |asm| asm.and(eax, dword_ptr(rax + 0x10)),
            |state, cpu| {
                state.gps[CpuState::RAX] = left;
                cpu.valid_gva = state.gps[CpuState::RAX].wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(state.rflags & RFLAGS_LOGIC_MASK, rflags.into());
    }
}
