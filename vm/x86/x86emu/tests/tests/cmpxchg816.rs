// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::CpuState;

const RFLAGS_CMPXCHG816B_MASK: RFlags = RFlags::new().with_zero(true);

#[test]
fn cmpxchg8b_equal() {
    let (state, cpu) = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Fail,
        |asm| asm.cmpxchg8b(ptr(0x100)),
        |state, cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0x1020304050607080u64;

            state.gps[CpuState::RDX] = 0x10203040;
            state.gps[CpuState::RAX] = 0x50607080;

            state.gps[CpuState::RCX] = 0x90a0b0c0;
            state.gps[CpuState::RBX] = 0xd0e0f000;
        },
    );

    assert_eq!(cpu.mem_val, 0x90a0b0c0d0e0f000);
    assert_eq!(state.gps[CpuState::RDX], 0x10203040);
    assert_eq!(state.gps[CpuState::RAX], 0x50607080);
    assert_eq!(
        state.rflags & RFLAGS_CMPXCHG816B_MASK,
        RFLAGS_CMPXCHG816B_MASK
    );
}

#[test]
fn cmpxchg8b_not_equal() {
    let (state, cpu) = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Succeed,
        |asm| asm.cmpxchg8b(ptr(0x100)),
        |state, cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0xf000f000f000f000u64;

            state.gps[CpuState::RDX] = 0x10203040;
            state.gps[CpuState::RAX] = 0x50607080;

            state.gps[CpuState::RCX] = 0x90a0b0c0;
            state.gps[CpuState::RBX] = 0xd0e0f000;
        },
    );

    assert_eq!(cpu.mem_val, 0xf000f000f000f000);
    assert_eq!(state.gps[CpuState::RDX], 0xf000f000);
    assert_eq!(state.gps[CpuState::RAX], 0xf000f000);
    assert_eq!(state.rflags & RFLAGS_CMPXCHG816B_MASK, RFlags::new());
}

#[test]
fn cmpxchg16b_equal() {
    let (state, cpu) = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Fail,
        |asm| asm.cmpxchg16b(ptr(0x100)),
        |state, cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0x102030405060708090a0b0c0d0e0f000u128;

            state.gps[CpuState::RDX] = 0x1020304050607080;
            state.gps[CpuState::RAX] = 0x90a0b0c0d0e0f000;

            state.gps[CpuState::RCX] = 0x0102030405060708;
            state.gps[CpuState::RBX] = 0x090a0b0c0d0e0f00;
        },
    );

    assert_eq!(cpu.mem_val, 0x0102030405060708090a0b0c0d0e0f00);
    assert_eq!(state.gps[CpuState::RDX], 0x1020304050607080);
    assert_eq!(state.gps[CpuState::RAX], 0x90a0b0c0d0e0f000);
    assert_eq!(
        state.rflags & RFLAGS_CMPXCHG816B_MASK,
        RFLAGS_CMPXCHG816B_MASK
    );
}

#[test]
fn cmpxchg16b_not_equal() {
    let (state, cpu) = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Succeed,
        |asm| asm.cmpxchg16b(ptr(0x100)),
        |state, cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0xf000f000f000f000f000f000f000f000u128;

            state.gps[CpuState::RDX] = 0x1020304050607080;
            state.gps[CpuState::RAX] = 0x90a0b0c0d0e0f000;

            state.gps[CpuState::RCX] = 0x0102030405060708;
            state.gps[CpuState::RBX] = 0x090a0b0c0d0e0f00;
        },
    );

    assert_eq!(cpu.mem_val, 0xf000f000f000f000f000f000f000f000);
    assert_eq!(state.gps[CpuState::RDX], 0xf000f000f000f000);
    assert_eq!(state.gps[CpuState::RAX], 0xf000f000f000f000);
    assert_eq!(state.rflags & RFLAGS_CMPXCHG816B_MASK, RFlags::new());
}
