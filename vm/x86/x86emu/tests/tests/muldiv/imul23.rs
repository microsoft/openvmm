// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::CpuState;

const RFLAGS_MUL_MASK: RFlags = RFlags::new().with_carry(true).with_overflow(true);

#[test]
fn imul2() {
    let variations = [
        (0x0, 0x0, 0x0, 0x0),
        (0x64, 0x64, 0x2710, 0x0),
        (0x0, 0x1, 0x0, 0x0),
        (0x1, 0x0, 0x0, 0x0),
        (0xffffffffffffffff, 0x0, 0x0, 0x0),
        (0xffffffffffffffff, 0xffffffff, 0x1, 0x0),
        (0xffffffff, 0xffffffffffffffff, 0x1, 0x0),
        (0xffffffff, 0xffffffff, 0x1, 0x0),
        (0x7fffffffffffffff, 0x0, 0x0, 0x0),
        (0x7fffffff, 0x0, 0x0, 0x0),
        (0x7fffffff, 0x2, 0xfffffffe, 0x801),
        (0x0, 0x7fffffff, 0x0, 0x0),
        (0x80000000, 0x7fffffff, 0x80000000, 0x801),
        (0x7fffffff, 0x80000000, 0x80000000, 0x801),
        (0x8000000000000000, 0x7fffffff, 0x0, 0x0),
        (0x7fffffff, 0x8000000000000000, 0x0, 0x0),
        (0x7fffffffffffffff, 0x7fffffffffffffff, 0x1, 0x0),
        (0x8000000000000000, 0x7fffffffffffffff, 0x0, 0x0),
        (0x8000000000000000, 0x8000000000000000, 0x0, 0x0),
    ];

    for (left, right, result, rflags) in variations {
        let (state, _cpu) = run_test(
            RFLAGS_MUL_MASK,
            |asm| asm.imul_2(eax, dword_ptr(rax + 0x10)),
            |state, cpu| {
                state.gps[CpuState::RAX] = left;
                cpu.valid_gva = state.gps[CpuState::RAX].wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(state.gps[CpuState::RAX], result);
        assert_eq!(state.rflags & RFLAGS_MUL_MASK, rflags.into());
    }
}

#[test]
fn imul3() {
    let variations = [
        (0x0u32, 0x0, 0x0, 0x0),
        (0x64, 0x64, 0x2710, 0x0),
        (0x0, 0x1, 0x0, 0x0),
        (0x1, 0x0, 0x0, 0x0),
        (0xffffffff, 0x0, 0x0, 0x0),
        (0xffffffff, 0xffffffff, 0x1, 0x0),
        (0xffffffff, 0xffffffffffffffff, 0x1, 0x0),
        (0xffffffff, 0xffffffff, 0x1, 0x0),
        (0x7fffffff, 0x0, 0x0, 0x0),
        (0x7fffffff, 0x0, 0x0, 0x0),
        (0x7fffffff, 0x2, 0xfffffffe, 0x801),
        (0x0, 0x7fffffff, 0x0, 0x0),
        (0x80000000, 0x7fffffff, 0x80000000, 0x801),
        (0x7fffffff, 0x80000000, 0x80000000, 0x801),
        (0x7fffffff, 0x8000000000000000, 0x0, 0x0),
        (0x7fffffff, 0x7fffffffffffffff, 0x80000001, 0x0),
        (0x80000000, 0x7fffffffffffffff, 0x80000000, 0x801),
        (0x80000000, 0x8000000000000000, 0x0, 0x0),
    ];

    for (left, right, result, rflags) in variations {
        let (state, _cpu) = run_test(
            RFLAGS_MUL_MASK,
            |asm| asm.imul_3(eax, dword_ptr(rax + 0x10), left),
            |state, cpu| {
                cpu.valid_gva = state.gps[CpuState::RAX].wrapping_add(0x10);
                cpu.mem_val = right;
            },
        );

        assert_eq!(state.gps[CpuState::RAX], result);
        assert_eq!(state.rflags & RFLAGS_MUL_MASK, rflags.into());
    }
}
