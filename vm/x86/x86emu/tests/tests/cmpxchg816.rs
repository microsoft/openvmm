// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::LockTestBehavior;
use crate::tests::common::run_lockable_test;
use crate::tests::common::{SingleCellCpu, TestCpu, TestCpuError};
use futures::FutureExt;
use iced_x86::code_asm::*;
use test_with_tracing::test;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

const RFLAGS_CMPXCHG816B_MASK: RFlags = RFlags::new().with_zero(true);

#[test]
fn unlocked_mismatch_write_fault_preserves_state() {
    let instructions: [(fn(&mut CodeAssembler) -> Result<(), IcedError>, usize); 3] = [
        (|a| a.cmpxchg(qword_ptr(0x100), rbx), 8),
        (|a| a.cmpxchg8b(qword_ptr(0x100)), 8),
        (|a| a.cmpxchg16b(xmmword_ptr(0x100)), 16),
    ];
    for (instruction, len) in instructions {
        let mut cpu = SingleCellCpu::<u128>::new(0x8d5.into());
        cpu.valid_gva = 0x100;
        cpu.mem_val = 0x1234;
        cpu.fail_writes = true;
        cpu.state.gps[Gp::RAX as usize] = 0;
        cpu.state.gps[Gp::RDX as usize] = 0;
        let initial = cpu.state.clone();
        let mut asm = CodeAssembler::new(64).unwrap();
        instruction(&mut asm).unwrap();
        let bytes = asm.assemble(0).unwrap();
        let result = x86emu::Emulator::new(&mut cpu, x86defs::cpuid::Vendor::INTEL, &bytes)
            .run()
            .now_or_never()
            .unwrap();
        assert!(matches!(
            *result.unwrap_err(),
            x86emu::Error::MemoryAccess(0x100, _, TestCpuError::WriteFault),
        ));
        assert_eq!(cpu.state, initial);
        assert_eq!(cpu.mem_val, 0x1234);
        assert_eq!(cpu.memory_reads, [(0x100, len, true)]);
    }
}

#[test]
fn cmpxchg16b_requires_alignment() {
    for locked in [false, true] {
        let mut cpu = SingleCellCpu::<u128>::new(0.into());
        cpu.valid_gva = 0x108;
        cpu.mem_val = 0x1234;
        let initial = cpu.state.clone();
        let mut asm = CodeAssembler::new(64).unwrap();
        let assembler = if locked { asm.lock() } else { &mut asm };
        assembler.cmpxchg16b(xmmword_ptr(0x108)).unwrap();
        let bytes = asm.assemble(0).unwrap();
        let result = x86emu::Emulator::new(&mut cpu, x86defs::cpuid::Vendor::INTEL, &bytes)
            .run()
            .now_or_never()
            .unwrap();
        assert!(matches!(
            *result.unwrap_err(),
            x86emu::Error::InstructionException(
                x86defs::Exception::GENERAL_PROTECTION_FAULT,
                Some(0),
                _
            ),
        ));
        assert_eq!(cpu.state, initial);
        assert_eq!(cpu.mem_val, 0x1234);
    }
}

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
        LockTestBehavior::Fail,
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
        LockTestBehavior::Fail,
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
