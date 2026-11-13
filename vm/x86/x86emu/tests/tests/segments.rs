// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common::CpuState;
use super::common::SingleCellCpu;
use super::common::TestCpu;
use futures::FutureExt;
use iced_x86::code_asm::*;
use test_with_tracing::test;
use x86defs::SegmentAttributes;
use x86defs::SegmentRegister;
use x86defs::cpuid::Vendor;
use x86emu::Cpu;
use x86emu::Emulator;
use x86emu::Gp;
use x86emu::Segment;

#[test]
fn compatibility_mode_16_bit_decoding() {
    let mut cpu = protected_cpu();
    cpu.state.efer = x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME;
    cpu.state.segs[Segment::CS as usize]
        .attributes
        .set_default(false);
    cpu.state.segs[Segment::DS as usize] = SegmentRegister {
        base: 0,
        limit: 0xffff,
        selector: 8,
        attributes: SegmentAttributes::new()
            .with_present(true)
            .with_non_system_segment(true)
            .with_segment_type(2),
    };
    cpu.state.gps[Gp::RBX as usize] = 0x100;
    cpu.valid_gva = 0x100;
    cpu.mem_val = 0x1234;
    let before = cpu.state.gps[Gp::RAX as usize];
    let mut asm = CodeAssembler::new(16).unwrap();
    asm.mov(ax, word_ptr(bx)).unwrap();
    let bytes = asm.assemble(0).unwrap();
    Emulator::new(&mut cpu, Vendor::INTEL, &bytes)
        .run()
        .now_or_never()
        .unwrap()
        .unwrap();
    assert_eq!(cpu.state.gps[Gp::RAX as usize], (before & !0xffff) | 0x1234);
    assert_eq!(cpu.state.rip, bytes.len() as u64);
}

#[test]
fn protected_16_bit_segment_checks() {
    for compatibility in [false, true] {
        for segment in [Segment::CS, Segment::DS] {
            let mut cpu = protected_cpu();
            if compatibility {
                cpu.state.efer = x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME;
            }
            cpu.state.segs[Segment::CS as usize]
                .attributes
                .set_default(false);
            cpu.state.segs[segment as usize] = SegmentRegister {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: SegmentAttributes::new()
                    .with_present(true)
                    .with_non_system_segment(true)
                    .with_segment_type(2),
            };
            let mut emu = Emulator::new(&mut cpu, Vendor::INTEL, &[]);
            let error = emu
                .write_memory(segment, 0x100, x86emu::AlignmentMode::Standard, &[0; 2])
                .now_or_never()
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                error,
                x86emu::Error::InstructionException(
                    x86defs::Exception::GENERAL_PROTECTION_FAULT,
                    Some(0),
                    _
                ),
            ));
            assert_eq!(cpu.mem_val, 0);
            assert_eq!(cpu.state.rip, 0);
        }
    }
}

#[test]
fn virtual_8086_code_segment_write() {
    let mut cpu = protected_cpu();
    cpu.state.rflags.set_virtual_8086_mode(true);
    cpu.state.segs[Segment::CS as usize]
        .attributes
        .set_default(false);
    cpu.state.segs[Segment::CS as usize].limit = 0xffff;
    cpu.valid_gva = 0x100;
    Emulator::new(&mut cpu, Vendor::INTEL, &[])
        .write_memory(
            Segment::CS,
            0x100,
            x86emu::AlignmentMode::Standard,
            &[0x34, 0x12],
        )
        .now_or_never()
        .unwrap()
        .unwrap();
    assert_eq!(cpu.mem_val, 0x1234);
}

#[test]
fn virtual_8086_data_segment_selectors() {
    for selector in [0, 1, 2, 3, 0x1000, 0x1001, 0x1002, 0x1003] {
        for write in [false, true] {
            let mut cpu = protected_cpu();
            cpu.state.rflags.set_virtual_8086_mode(true);
            cpu.state.segs[Segment::CS as usize]
                .attributes
                .set_default(false);
            cpu.state.segs[Segment::SS as usize]
                .attributes
                .set_descriptor_privilege_level(3);
            cpu.state.segs[Segment::DS as usize] = SegmentRegister {
                base: u64::from(selector) << 4,
                limit: 0xffff,
                selector,
                attributes: SegmentAttributes::new()
                    .with_present(true)
                    .with_non_system_segment(true)
                    .with_segment_type(3)
                    .with_descriptor_privilege_level(3),
            };
            cpu.state.gps[Gp::RBX as usize] = 0x100;
            cpu.state.gps[Gp::RAX as usize] = 0x1234_5678_9abc_0123;
            cpu.valid_gva = (u64::from(selector) << 4) + 0x100;
            cpu.mem_val = 0xa55a;
            let mut asm = CodeAssembler::new(16).unwrap();
            if write {
                asm.mov(word_ptr(bx), ax).unwrap();
            } else {
                asm.mov(ax, word_ptr(bx)).unwrap();
            }
            let bytes = asm.assemble(0).unwrap();
            Emulator::new(&mut cpu, Vendor::INTEL, &bytes)
                .run()
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(cpu.state.rip, bytes.len() as u64);
            assert!(cpu.state.rflags.virtual_8086_mode());
            assert_eq!(cpu.mem_val, if write { 0x123 } else { 0xa55a });
            assert_eq!(
                cpu.state.gps[Gp::RAX as usize],
                if write {
                    0x1234_5678_9abc_0123
                } else {
                    0x1234_5678_9abc_a55a
                },
            );
            assert_eq!(
                cpu.memory_reads,
                if write {
                    vec![]
                } else {
                    vec![(cpu.valid_gva, 2, false)]
                }
            );
        }
    }
}

#[test]
fn stack_limit_fault_is_ss() {
    let mut cpu = protected_cpu();
    cpu.state.segs[Segment::SS as usize] = SegmentRegister {
        base: 0,
        limit: 0xff,
        selector: 8,
        attributes: SegmentAttributes::new()
            .with_present(true)
            .with_non_system_segment(true)
            .with_segment_type(2),
    };
    let before = cpu.state.clone();
    let mut emu = Emulator::new(&mut cpu, Vendor::INTEL, &[]);
    let err = emu
        .write_memory(Segment::SS, 0x100, x86emu::AlignmentMode::Standard, &[0; 4])
        .now_or_never()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        err,
        x86emu::Error::InstructionException(x86defs::Exception::STACK_SEGMENT_FAULT, Some(0), _),
    ));
    assert_eq!(cpu.state, before);
    assert_eq!(cpu.mem_val, 0);
}

#[test]
fn alignment_check_has_zero_error_code() {
    let mut cpu = SingleCellCpu::<u64>::new(0.into());
    cpu.state.cr0 |= x86defs::X64_CR0_AM;
    cpu.state.rflags.set_alignment_check(true);
    cpu.state.segs[Segment::SS as usize]
        .attributes
        .set_descriptor_privilege_level(3);
    cpu.valid_gva = 0x101;
    cpu.mem_val = 0x1234;
    let before = cpu.state.clone();
    let mut asm = CodeAssembler::new(64).unwrap();
    asm.mov(rax, qword_ptr(0x101)).unwrap();
    let bytes = asm.assemble(0).unwrap();
    let result = Emulator::new(&mut cpu, Vendor::INTEL, &bytes)
        .run()
        .now_or_never()
        .unwrap();
    assert!(matches!(
        *result.unwrap_err(),
        x86emu::Error::InstructionException(x86defs::Exception::ALIGNMENT_CHECK, Some(0), _),
    ));
    assert_eq!(cpu.state, before);
}

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
        cr4: 0,
        efer: 0,
    };
    SingleCellCpu {
        valid_gva: 0,
        mem_val: 0,
        valid_io_port: 0,
        io_val: 0,
        xmm: [0; 16],
        invert_after_read: false,
        memory_reads: Vec::new(),
        fail_writes: false,
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
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(4099)")]
fn rpl() {
    do_data_segment_test(|cpu| {
        let mut emu_ds = cpu.segment(Segment::DS);
        emu_ds.selector = 0x1003;
        cpu.set_segment(Segment::DS, emu_ds);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(4098)")]
fn cpl() {
    do_data_segment_test(|cpu| {
        let mut emu_ss = cpu.segment(Segment::SS);
        emu_ss.attributes.set_descriptor_privilege_level(3);
        cpu.set_segment(Segment::SS, emu_ss);
    });
}

#[test]
#[should_panic(expected = "GENERAL_PROTECTION_FAULT, Some(4098)")]
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
