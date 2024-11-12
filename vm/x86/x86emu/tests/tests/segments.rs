// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common::SingleCellCpu;
use futures::FutureExt;
use iced_x86::code_asm::*;
use x86defs::cpuid::Vendor;
use x86defs::SegmentAttributes;
use x86defs::SegmentRegister;
use x86emu::CpuState;
use x86emu::Emulator;

fn protected_state() -> CpuState {
    let seg = SegmentRegister {
        base: 0,
        limit: 0,
        attributes: SegmentAttributes::new().with_default(true),
        selector: 0,
    };
    CpuState {
        gps: [0xbadc0ffee0ddf00d; 16],
        segs: [seg; 6],
        rip: 0,
        rflags: 0.into(),
        cr0: x86defs::X64_CR0_PE,
        efer: 0,
    }
}

fn do_data_segment_test(modify_state: impl FnOnce(&mut CpuState, &mut SingleCellCpu<u64>)) {
    let mut state = protected_state();
    let mut cpu = SingleCellCpu::<u64>::default();
    let mut asm = CodeAssembler::new(32).unwrap();
    asm.mov(ptr(edx).ds(), ebx).unwrap();

    state.gps[CpuState::RBX] = 12345678;

    cpu.valid_gva = 0x300;
    state.gps[CpuState::RDX] = 0x100;
    state.segs[CpuState::DS].base = 0x200;
    state.segs[CpuState::DS].limit = 0x104;
    state.segs[CpuState::DS]
        .attributes
        .set_non_system_segment(true);
    state.segs[CpuState::DS].attributes.set_segment_type(0b0010);
    state.segs[CpuState::DS].attributes.set_present(true);
    state.segs[CpuState::DS]
        .attributes
        .set_descriptor_privilege_level(2);
    state.segs[CpuState::DS].selector = 0x1002;

    // Set CPL
    state.segs[CpuState::SS]
        .attributes
        .set_descriptor_privilege_level(2);

    modify_state(&mut state, &mut cpu);

    let code = asm.assemble(state.rip).unwrap();
    Emulator::new(&mut cpu, &mut state, Vendor::INTEL, &code)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();

    assert_eq!(cpu.mem_val, 12345678);
}

#[test]
fn clean_mov() {
    do_data_segment_test(|_, _| {});
}

#[test]
#[should_panic(expected = "SEGMENT_NOT_PRESENT")]
fn not_present() {
    do_data_segment_test(|state, _| state.segs[CpuState::DS].attributes.set_present(false));
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn null_selector() {
    do_data_segment_test(|state, _| state.segs[CpuState::DS].selector = 2);
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(3)")]
fn rpl() {
    do_data_segment_test(|state, _| state.segs[CpuState::DS].selector = 0x1003);
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(3)")]
fn cpl() {
    do_data_segment_test(|state, _| {
        state.segs[CpuState::SS]
            .attributes
            .set_descriptor_privilege_level(3)
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(3)")]
fn system_segment() {
    do_data_segment_test(|state, _| {
        state.segs[CpuState::DS]
            .attributes
            .set_non_system_segment(false)
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn not_writeable() {
    do_data_segment_test(|state, _| {
        state.segs[CpuState::DS].attributes.set_segment_type(0);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn above_limit() {
    do_data_segment_test(|state, _| {
        state.segs[CpuState::DS].limit = 0x101;
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn below_expand_down_limit() {
    do_data_segment_test(|state, _| {
        state.segs[CpuState::DS].limit = 0x1000;
        state.segs[CpuState::DS].attributes.set_segment_type(0b0110);
    });
}

fn do_code_segment_test(modify_state: impl FnOnce(&mut CpuState, &mut SingleCellCpu<u64>)) {
    let mut state = protected_state();
    let mut cpu = SingleCellCpu::<u64>::default();
    let mut asm = CodeAssembler::new(32).unwrap();
    asm.mov(ebx, ptr(edx).cs()).unwrap();

    cpu.mem_val = 12345678;
    cpu.valid_gva = 0x300;
    state.gps[CpuState::RDX] = 0x100;
    state.segs[CpuState::CS].base = 0x200;
    state.segs[CpuState::CS].limit = 0x104;
    state.segs[CpuState::CS].attributes.set_segment_type(0b0010);

    modify_state(&mut state, &mut cpu);

    let code = asm.assemble(state.rip).unwrap();
    Emulator::new(&mut cpu, &mut state, Vendor::INTEL, &code)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();

    assert_eq!(state.gps[CpuState::RBX], 12345678);
}

#[test]
fn clean_mov_cs() {
    do_code_segment_test(|_, _| {});
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn not_readable() {
    do_code_segment_test(|state, _| {
        state.segs[CpuState::CS].attributes.set_segment_type(0);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn above_limit_cs() {
    do_code_segment_test(|state, _| {
        state.segs[CpuState::CS].limit = 0x101;
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn cant_write_cs() {
    let mut state = protected_state();
    let mut cpu = SingleCellCpu::<u64>::default();
    let mut asm = CodeAssembler::new(32).unwrap();
    asm.mov(ptr(edx).cs(), ebx).unwrap();

    cpu.mem_val = 12345678;
    cpu.valid_gva = 0x300;
    state.gps[CpuState::RDX] = 0x100;
    state.segs[CpuState::CS].base = 0x200;
    state.segs[CpuState::CS].limit = 0x104;
    state.segs[CpuState::CS].attributes.set_segment_type(0b0010);

    let code = asm.assemble(state.rip).unwrap();
    Emulator::new(&mut cpu, &mut state, Vendor::INTEL, &code)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();
}
