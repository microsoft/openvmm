// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::CpuState;

mod others;
mod sse;
mod xchg;

/// The mask of flags that are changed by mov operations.
const RFLAGS_MOV_MASK: RFlags = RFlags::new();

#[test]
fn mov_regvalue_to_memory() {
    let (_state, cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax), esi),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x456;
            state.gps[CpuState::RSI] = 0xcccc12345678;
            cpu.valid_gva = state.gps[CpuState::RAX];
        },
    );

    assert_eq!(cpu.mem_val, 0x12345678);
}

#[test]
fn mov_regvalue_to_memory_8bit() {
    let (_state, cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(byte_ptr(rax), dh),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x456;
            state.gps[CpuState::RDX] = 0x1234;
            cpu.valid_gva = state.gps[CpuState::RAX];
            cpu.mem_val = 0xcc00;
        },
    );

    assert_eq!(cpu.mem_val, 0xcc12);
}

#[test]
fn mov_regvalue_to_memory_imm32() {
    let (_state, cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax * 1 + 1), edi),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x456;
            state.gps[CpuState::RDI] = 0x123;
            cpu.valid_gva = state.gps[CpuState::RAX] + 1;
            cpu.mem_val = 0xcc00;
        },
    );

    assert_eq!(cpu.mem_val, 0x123);
}

#[test]
fn mov_memory_to_regvalue() {
    let (state, _cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(eax, dword_ptr(rax + 0x10)),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x12345678ffffffff;
            cpu.valid_gva = state.gps[CpuState::RAX] + 0x10;
            cpu.mem_val = 0x123;
        },
    );

    assert_eq!(state.gps[CpuState::RAX], 0x123);
}

#[test]
fn mov_memory_to_regvalue_8bit() {
    let (state, _cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(al, byte_ptr(rax + 0x10)),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x12345678abcdefff;
            cpu.valid_gva = state.gps[CpuState::RAX] + 0x10;
            cpu.mem_val = 0xffffffffffffff12;
        },
    );

    assert_eq!(state.gps[CpuState::RAX], 0x12345678abcdef12);
}

#[test]
fn mov_memory_to_regvalue64_two_indices() {
    let (state, _cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(rax, ptr(rax + rax)),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x1234;
            cpu.valid_gva = state.gps[CpuState::RAX] * 2;
            cpu.mem_val = 0x33333;
        },
    );

    assert_eq!(state.gps[CpuState::RAX], 0x33333);
}

#[test]
fn mov_memory_to_rax() {
    let (state, _cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(eax, ptr(0x123456789abcdef0i64)),
        |_state, cpu| {
            cpu.valid_gva = 0x123456789abcdef0;
            cpu.mem_val = 0x33333;
        },
    );

    assert_eq!(state.gps[CpuState::RAX], 0x33333);
}

#[test]
fn mov_alignment_check() {
    let (_state, cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax), rsi),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x100;
            state.gps[CpuState::RSI] = 0x1234567890abcdef;
            cpu.valid_gva = state.gps[CpuState::RAX];
            state.rflags.set_alignment_check(true);
            state.cr0 |= x86defs::X64_CR0_AM;
            state.segs[CpuState::SS]
                .attributes
                .set_descriptor_privilege_level(x86defs::USER_MODE_DPL);
        },
    );

    assert_eq!(cpu.mem_val, 0x1234567890abcdef);
}

#[test]
#[should_panic(expected = "AlignmentCheck")]
fn mov_alignment_check_fail() {
    let (_state, _cpu) = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax), rsi),
        |state, cpu| {
            state.gps[CpuState::RAX] = 0x101;
            state.gps[CpuState::RSI] = 0x1234567890abcdef;
            cpu.valid_gva = state.gps[CpuState::RAX];
            state.rflags.set_alignment_check(true);
            state.cr0 |= x86defs::X64_CR0_AM;
            state.segs[CpuState::SS]
                .attributes
                .set_descriptor_privilege_level(x86defs::USER_MODE_DPL);
        },
    );
}
