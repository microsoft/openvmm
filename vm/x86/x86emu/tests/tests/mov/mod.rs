// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use crate::tests::common::TestCpu;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;
use x86emu::Segment;

mod others;
mod sse;
mod xchg;

/// The mask of flags that are changed by mov operations.
const RFLAGS_MOV_MASK: RFlags = RFlags::new();

#[test]
fn mov_regvalue_to_memory() {
    let cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax), esi),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x456);
            cpu.set_gp(Gp::RSI.into(), 0xcccc12345678);
            cpu.valid_gva = cpu.gp(Gp::RAX.into());
        },
    );

    assert_eq!(cpu.mem_val, 0x12345678);
}

#[test]
fn mov_regvalue_to_memory_8bit() {
    let cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(byte_ptr(rax), dh),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x456);
            cpu.set_gp(Gp::RDX.into(), 0x1234);
            cpu.valid_gva = cpu.gp(Gp::RAX.into());
            cpu.mem_val = 0xcc00;
        },
    );

    assert_eq!(cpu.mem_val, 0xcc12);
}

#[test]
fn mov_regvalue_to_memory_imm32() {
    let cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax * 1 + 1), edi),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x456);
            cpu.set_gp(Gp::RDI.into(), 0x123);
            cpu.valid_gva = cpu.gp(Gp::RAX.into()) + 1;
            cpu.mem_val = 0xcc00;
        },
    );

    assert_eq!(cpu.mem_val, 0x123);
}

#[test]
fn mov_memory_to_regvalue() {
    let mut cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(eax, dword_ptr(rax + 0x10)),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x12345678ffffffff);
            cpu.valid_gva = cpu.gp(Gp::RAX.into()) + 0x10;
            cpu.mem_val = 0x123;
        },
    );

    assert_eq!(cpu.gp(Gp::RAX.into()), 0x123);
}

#[test]
fn mov_memory_to_regvalue_8bit() {
    let mut cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(al, byte_ptr(rax + 0x10)),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x12345678abcdefff);
            cpu.valid_gva = cpu.gp(Gp::RAX.into()) + 0x10;
            cpu.mem_val = 0xffffffffffffff12;
        },
    );

    assert_eq!(cpu.gp(Gp::RAX.into()), 0x12345678abcdef12);
}

#[test]
fn mov_memory_to_regvalue64_two_indices() {
    let mut cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(rax, ptr(rax + rax)),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x1234);
            cpu.valid_gva = cpu.gp(Gp::RAX.into()) * 2;
            cpu.mem_val = 0x33333;
        },
    );

    assert_eq!(cpu.gp(Gp::RAX.into()), 0x33333);
}

#[test]
fn mov_memory_to_rax() {
    let mut cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(eax, ptr(0x123456789abcdef0i64)),
        |cpu| {
            cpu.valid_gva = 0x123456789abcdef0;
            cpu.mem_val = 0x33333;
        },
    );

    assert_eq!(cpu.gp(Gp::RAX.into()), 0x33333);
}

#[test]
fn mov_alignment_check() {
    let cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax), rsi),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x100);
            cpu.set_gp(Gp::RSI.into(), 0x1234567890abcdef);
            cpu.valid_gva = cpu.gp(Gp::RAX.into());

            let mut rflags = cpu.rflags();
            rflags.set_alignment_check(true);
            cpu.set_rflags(rflags);

            let am = cpu.cr0() | x86defs::X64_CR0_AM;
            cpu.set_cr0(am);

            let mut um = cpu.segment(Segment::SS);
            um.attributes
                .set_descriptor_privilege_level(x86defs::USER_MODE_DPL);

            cpu.set_segment(Segment::SS, um);
        },
    );

    assert_eq!(cpu.mem_val, 0x1234567890abcdef);
}

#[test]
#[should_panic(expected = "AlignmentCheck")]
fn mov_alignment_check_fail() {
    let _cpu = run_test(
        RFLAGS_MOV_MASK,
        |asm| asm.mov(dword_ptr(rax), rsi),
        |cpu| {
            cpu.set_gp(Gp::RAX.into(), 0x101);
            cpu.set_gp(Gp::RSI.into(), 0x1234567890abcdef);
            cpu.valid_gva = cpu.gp(Gp::RAX.into());

            let mut rflags = cpu.rflags();
            rflags.set_alignment_check(true);
            cpu.set_rflags(rflags);

            let am = cpu.cr0() | x86defs::X64_CR0_AM;
            cpu.set_cr0(am);

            let mut um = cpu.segment(Segment::SS);
            um.attributes
                .set_descriptor_privilege_level(x86defs::USER_MODE_DPL);

            cpu.set_segment(Segment::SS, um);
        },
    );
}
