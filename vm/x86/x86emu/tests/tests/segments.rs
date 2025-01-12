// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common::CpuState;
use super::common::SingleCellCpu;
use super::common::TestCpu;
use futures::FutureExt;
use iced_x86::code_asm::*;
use x86defs::cpuid::Vendor;
use x86defs::SegmentAttributes;
use x86defs::SegmentRegister;
use x86emu::Cpu;
use x86emu::Emulator;
use x86emu::Gp;
use x86emu::Segment;

fn protected_cpu() -> SingleCellCpu<u64> {
    let seg = SegmentRegister {
        base: 0,
        limit: 0,
        attributes: SegmentAttributes::new().with_default(true),
        selector: 0,
    };
    let state = CpuState {
        gps: [0xbadc0ffee0ddf00d; 16],
        segs: [seg; 6],
        rip: 0,
        rflags: 0.into(),
        cr0: x86defs::X64_CR0_PE,
        efer: 0,
    };
    SingleCellCpu {
        valid_gva: 0,
        mem_val: 0,
        valid_io_port: 0,
        io_val: 0,
        xmm: [0; 16],
        invert_after_read: false,
        state,
    }
}

fn do_data_segment_test(modify_state: impl FnOnce(&mut SingleCellCpu<u64>)) {
    let mut cpu = protected_cpu();

    let mut asm = CodeAssembler::new(32).unwrap();
    asm.mov(ptr(edx).ds(), ebx).unwrap();

    cpu.set_gp(Gp::RBX.into(), 12345678);

    cpu.valid_gva = 0x300;
    cpu.set_gp(Gp::RDX.into(), 0x100);

    let mut emu_ds = cpu.segment(Segment::DS);
    emu_ds.base = 0x200;

    emu_ds.limit = 0x104;
    emu_ds.attributes.set_non_system_segment(true);
    emu_ds.attributes.set_segment_type(0b0010);
    emu_ds.attributes.set_present(true);
    emu_ds.attributes.set_descriptor_privilege_level(2);
    emu_ds.selector = 0x1002;

    cpu.set_segment(Segment::DS, emu_ds);

    let mut emu_ss = cpu.segment(Segment::SS);

    // Set CPL
    emu_ss.attributes.set_descriptor_privilege_level(2);

    cpu.set_segment(Segment::SS, emu_ss);

    modify_state(&mut cpu);

    let code = asm.assemble(cpu.rip()).unwrap();
    Emulator::new(&mut cpu, Vendor::INTEL, &code)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();

    assert_eq!(cpu.mem_val, 12345678);
}

#[test]
fn clean_mov() {
    do_data_segment_test(|_| {});
}

#[test]
#[should_panic(expected = "SEGMENT_NOT_PRESENT")]
fn not_present() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.attributes.set_present(false);
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn null_selector() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.selector = 2;
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(3)")]
fn rpl() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.selector = 0x1003;
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(3)")]
fn cpl() {
    do_data_segment_test(|cpu| {
        let mut emu_ss = cpu.segment(Segment::SS);
        emu_ss.attributes.set_descriptor_privilege_level(3);
        cpu.set_segment(Segment::SS, emu_ss);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(3)")]
fn system_segment() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.attributes.set_non_system_segment(false);
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn not_writeable() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.attributes.set_segment_type(0);
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn above_limit() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.limit = 0x101;
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn below_expand_down_limit() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.limit = 0x1000;
        emu_ds.attributes.set_segment_type(0b0110);
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

fn do_code_segment_test(modify_state: impl FnOnce(&mut SingleCellCpu<u64>)) {
    let mut cpu = protected_cpu();
    let mut asm = CodeAssembler::new(32).unwrap();
    asm.mov(ebx, ptr(edx).cs()).unwrap();

    cpu.mem_val = 12345678;
    cpu.valid_gva = 0x300;
    cpu.set_gp(Gp::RDX.into(), 0x100);

    let mut emu_cs = cpu.segment(Segment::CS);
    emu_cs.base = 0x200;
    emu_cs.limit = 0x104;
    emu_cs.attributes.set_segment_type(0b0010);
    cpu.set_segment(Segment::CS, emu_cs);

    modify_state(&mut cpu);

    let code = asm.assemble(cpu.rip()).unwrap();
    Emulator::new(&mut cpu, Vendor::INTEL, &code)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();

    assert_eq!(cpu.gp(Gp::RBX.into()), 12345678);
}

#[test]
fn clean_mov_cs() {
    do_code_segment_test(|_| {});
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn not_readable() {
    do_code_segment_test(|cpu| {
        let mut emu_cs = cpu.segment(Segment::CS);
        emu_cs.attributes.set_segment_type(0);
        cpu.set_segment(Segment::CS, emu_cs);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn above_limit_cs() {
    do_code_segment_test(|cpu| {
        let mut emu_cs = cpu.segment(Segment::CS);
        emu_cs.limit = 0x101;
        cpu.set_segment(Segment::CS, emu_cs);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(0)")]
fn cant_write_cs() {
    let mut cpu = protected_cpu();

    let mut asm = CodeAssembler::new(32).unwrap();
    asm.mov(ptr(edx).cs(), ebx).unwrap();

    cpu.mem_val = 12345678;
    cpu.valid_gva = 0x300;

    cpu.set_gp(Gp::RDX.into(), 0x100);

    let mut emu_cs = cpu.segment(Segment::CS);
    emu_cs.base = 0x200;
    emu_cs.limit = 0x104;
    emu_cs.attributes.set_segment_type(0b0010);
    cpu.set_segment(Segment::CS, emu_cs);

    let code = asm.assemble(cpu.rip()).unwrap();
    Emulator::new(&mut cpu, Vendor::INTEL, &code)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();
}
