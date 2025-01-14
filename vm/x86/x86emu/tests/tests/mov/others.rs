// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::RFLAGS_MOV_MASK;
use crate::tests::common::run_test;
use crate::tests::common::run_wide_test;
use iced_x86::code_asm::*;
use iced_x86::Register;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

#[test]
fn mov_memory_to_regvalue_others() {
    let variations: &[&dyn Fn(
        &mut CodeAssembler,
        AsmMemoryOperand,
        AsmRegister64,
    ) -> Result<(), IcedError>] = &[&CodeAssembler::movdiri, &CodeAssembler::movnti];

    for instr in variations {
        let cpu = run_test(
            RFLAGS_MOV_MASK,
            |asm| instr(asm, ptr(0x200), rax),
            |cpu| {
                cpu.valid_gva = 0x200;
                cpu.set_gp(Gp::RAX.into(), 0x0123456789abcdef);
            },
        );

        assert_eq!(cpu.mem_val, 0x0123456789abcdef);
    }
}

#[test]
fn movzx_memory_to_regvalue() {
    const MEM_ADDR: usize = 0x200;
    let variations = &[
        (Register::RAX, word_ptr(MEM_ADDR), 0x9999),
        (Register::EAX, word_ptr(MEM_ADDR), 0x9999),
        (Register::RAX, byte_ptr(MEM_ADDR), 0x99),
        (Register::EAX, byte_ptr(MEM_ADDR), 0x99),
        (Register::AX, byte_ptr(MEM_ADDR), 0xffffffffffff0099),
    ];

    for &(dst_reg, src, result) in variations {
        let mut cpu = run_test(
            RFLAGS_MOV_MASK,
            |asm| {
                // work around iced limitations
                if let Some(dst) = get_gpr64(dst_reg) {
                    asm.movzx(dst, src)
                } else if let Some(dst) = get_gpr32(dst_reg) {
                    asm.movzx(dst, src)
                } else if let Some(dst) = get_gpr16(dst_reg) {
                    asm.movzx(dst, src)
                } else {
                    unreachable!()
                }
            },
            |cpu| {
                cpu.valid_gva = 0x200;
                cpu.mem_val = 0x9999;
                cpu.set_gp(Gp::RAX.into(), 0xffffffffffffffff);
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), result);
    }
}

#[test]
fn movsx_memory_to_regvalue() {
    const MEM_ADDR: usize = 0x200;
    let variations = &[
        (Register::RAX, word_ptr(MEM_ADDR), 0xffffffffffffaaaa),
        (Register::EAX, word_ptr(MEM_ADDR), 0xffffaaaa),
        (Register::RAX, byte_ptr(MEM_ADDR), 0xffffffffffffffaa),
        (Register::EAX, byte_ptr(MEM_ADDR), 0xffffffaa),
        (Register::AX, byte_ptr(MEM_ADDR), 0x333333333333ffaa),
    ];

    for &(dst_reg, src, result) in variations {
        let mut cpu = run_test(
            RFLAGS_MOV_MASK,
            |asm| {
                // Please ignore these really silly type shenanigans to work around iced limitations
                if let Some(dst) = get_gpr64(dst_reg) {
                    asm.movsx(dst, src)
                } else if let Some(dst) = get_gpr32(dst_reg) {
                    asm.movsx(dst, src)
                } else if let Some(dst) = get_gpr16(dst_reg) {
                    asm.movsx(dst, src)
                } else {
                    unreachable!()
                }
            },
            |cpu| {
                cpu.valid_gva = 0x200;
                cpu.mem_val = 0xaaaa;
                cpu.set_gp(Gp::RAX.into(), 0x3333333333333333);
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), result);
    }
}

#[test]
fn movsxd_memory_to_regvalue() {
    const MEM_ADDR: usize = 0x200;
    let variations = &[
        (Register::RAX, dword_ptr(MEM_ADDR), 0xffffffffaaaaaaaa),
        (Register::EAX, dword_ptr(MEM_ADDR), 0xaaaaaaaa),
        (Register::AX, word_ptr(MEM_ADDR), 0x333333333333aaaa),
    ];

    for &(dst_reg, src, result) in variations {
        let mut cpu = run_test(
            RFLAGS_MOV_MASK,
            |asm| {
                // Please ignore these really silly type shenanigans to work around iced limitations
                if let Some(dst) = get_gpr64(dst_reg) {
                    asm.movsxd(dst, src)
                } else if let Some(dst) = get_gpr32(dst_reg) {
                    asm.movsxd(dst, src)
                } else if let Some(dst) = get_gpr16(dst_reg) {
                    asm.movsxd(dst, src)
                } else {
                    unreachable!()
                }
            },
            |cpu| {
                cpu.valid_gva = 0x200;
                cpu.mem_val = 0xaaaaaaaaaaaaaaaa;
                cpu.set_gp(Gp::RAX.into(), 0x3333333333333333);
            },
        );

        assert_eq!(cpu.gp(Gp::RAX.into()), result);
    }
}

#[test]
fn movdir64b() {
    let values: Vec<u8> = (0..64).collect();
    let cpu = run_wide_test(
        RFlags::new(),
        true,
        |asm| asm.movdir64b(r8, ptr(0x200)),
        |cpu| {
            cpu.valid_gva = 0x200;
            cpu.mem_val.clone_from(&values);
            cpu.write_mem_offset = 0x1000;
            cpu.set_gp(Gp::R8.into(), 0x1200);
        },
    );

    assert_eq!(cpu.mem_val, values);
}

#[test]
#[should_panic(expected = "MandatoryAlignment")]
fn movdir64b_unaligned() {
    let values: Vec<u8> = (0..64).collect();
    let _cpu = run_wide_test(
        RFlags::new(),
        true,
        |asm| asm.movdir64b(r8, ptr(0x200)),
        |cpu| {
            cpu.valid_gva = 0x200;
            cpu.mem_val.clone_from(&values);
            cpu.write_mem_offset = 0x1001;
            cpu.set_gp(Gp::R8.into(), 0x1201);
        },
    );
}
