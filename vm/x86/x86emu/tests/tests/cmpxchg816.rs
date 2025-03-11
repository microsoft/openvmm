// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

const RFLAGS_CMPXCHG816B_MASK: RFlags = RFlags::new().with_zero(true);

#[test]
fn cmpxchg8b_equal() {
    let mut cpu = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Fail,
        |asm| asm.cmpxchg8b(ptr(0x100)),
        |cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0x1020304050607080u64;

            cpu.set_gp(Gp::RDX.into(), 0x10203040);
            cpu.set_gp(Gp::RAX.into(), 0x50607080);

            cpu.set_gp(Gp::RCX.into(), 0x90a0b0c0);
            cpu.set_gp(Gp::RBX.into(), 0xd0e0f000);
        },
    );

    assert_eq!(cpu.mem_val, 0x90a0b0c0d0e0f000);
    assert_eq!(cpu.gp(Gp::RDX.into()), 0x10203040);
    assert_eq!(cpu.gp(Gp::RAX.into()), 0x50607080);
    assert_eq!(
        cpu.rflags() & RFLAGS_CMPXCHG816B_MASK,
        RFLAGS_CMPXCHG816B_MASK
    );
}

#[test]
fn cmpxchg8b_not_equal() {
    let mut cpu = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Succeed,
        |asm| asm.cmpxchg8b(ptr(0x100)),
        |cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0xf000f000f000f000u64;

            cpu.set_gp(Gp::RDX.into(), 0x10203040);
            cpu.set_gp(Gp::RAX.into(), 0x50607080);

            cpu.set_gp(Gp::RCX.into(), 0x90a0b0c0);
            cpu.set_gp(Gp::RBX.into(), 0xd0e0f000);
        },
    );

    assert_eq!(cpu.mem_val, 0xf000f000f000f000);
    assert_eq!(cpu.gp(Gp::RDX.into()), 0xf000f000);
    assert_eq!(cpu.gp(Gp::RAX.into()), 0xf000f000);
    assert_eq!(cpu.rflags() & RFLAGS_CMPXCHG816B_MASK, RFlags::new());
}

#[test]
fn cmpxchg16b_equal() {
    let mut cpu = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Fail,
        |asm| asm.cmpxchg16b(ptr(0x100)),
        |cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0x102030405060708090a0b0c0d0e0f000u128;

            cpu.set_gp(Gp::RDX.into(), 0x1020304050607080);
            cpu.set_gp(Gp::RAX.into(), 0x90a0b0c0d0e0f000);

            cpu.set_gp(Gp::RCX.into(), 0x0102030405060708);
            cpu.set_gp(Gp::RBX.into(), 0x090a0b0c0d0e0f00);
        },
    );

    assert_eq!(cpu.mem_val, 0x0102030405060708090a0b0c0d0e0f00);
    assert_eq!(cpu.gp(Gp::RDX.into()), 0x1020304050607080);
    assert_eq!(cpu.gp(Gp::RAX.into()), 0x90a0b0c0d0e0f000);
    assert_eq!(
        cpu.rflags() & RFLAGS_CMPXCHG816B_MASK,
        RFLAGS_CMPXCHG816B_MASK
    );
}

#[test]
fn cmpxchg16b_not_equal() {
    let mut cpu = run_lockable_test(
        RFLAGS_CMPXCHG816B_MASK,
        LockTestBehavior::Succeed,
        |asm| asm.cmpxchg16b(ptr(0x100)),
        |cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0xf000f000f000f000f000f000f000f000u128;

            cpu.set_gp(Gp::RDX.into(), 0x1020304050607080);
            cpu.set_gp(Gp::RAX.into(), 0x90a0b0c0d0e0f000);

            cpu.set_gp(Gp::RCX.into(), 0x0102030405060708);
            cpu.set_gp(Gp::RBX.into(), 0x090a0b0c0d0e0f00);
        },
    );

    assert_eq!(cpu.mem_val, 0xf000f000f000f000f000f000f000f000);
    assert_eq!(cpu.gp(Gp::RDX.into()), 0xf000f000f000f000);
    assert_eq!(cpu.gp(Gp::RAX.into()), 0xf000f000f000f000);
    assert_eq!(cpu.rflags() & RFLAGS_CMPXCHG816B_MASK, RFlags::new());
}
