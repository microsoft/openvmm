// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

#[test]
fn setcc() {
    let variations: &[(
        &dyn Fn(&mut CodeAssembler, AsmMemoryOperand) -> Result<(), IcedError>,
        _,
        _,
    )] = &[
        (&CodeAssembler::seta, false, true),
        (&CodeAssembler::setae, false, true),
        (&CodeAssembler::setb, true, false),
        (&CodeAssembler::setbe, true, false),
        (&CodeAssembler::sete, true, false),
        (&CodeAssembler::setg, false, true),
        (&CodeAssembler::setge, true, true),
        (&CodeAssembler::setl, false, false),
        (&CodeAssembler::setle, true, false),
        (&CodeAssembler::setne, false, true),
        (&CodeAssembler::setno, false, true),
        (&CodeAssembler::setnp, false, true),
        (&CodeAssembler::setns, false, true),
        (&CodeAssembler::seto, true, false),
        (&CodeAssembler::setp, true, false),
        (&CodeAssembler::sets, true, false),
    ];

    for &(instruction, true_value, false_value) in variations {
        for flag_val in [true, false] {
            let cpu = run_test(
                RFlags::new(),
                |asm| instruction(asm, ptr(0x200)),
                |cpu| {
                    cpu.valid_gva = 0x200;
                    let mut rflags = cpu.rflags();
                    rflags = rflags
                        .with_carry(flag_val)
                        .with_overflow(flag_val)
                        .with_parity(flag_val)
                        .with_sign(flag_val)
                        .with_zero(flag_val);
                    cpu.set_rflags(rflags);
                },
            );

            if flag_val {
                assert_eq!(cpu.mem_val == 1, true_value);
            } else {
                assert_eq!(cpu.mem_val == 1, false_value);
            }
        }
    }
}

#[test]
fn cmovcc() {
    let variations: &[(
        &dyn Fn(&mut CodeAssembler, AsmRegister64, AsmMemoryOperand) -> Result<(), IcedError>,
        _,
        _,
    )] = &[
        (&CodeAssembler::cmova, false, true),
        (&CodeAssembler::cmovae, false, true),
        (&CodeAssembler::cmovb, true, false),
        (&CodeAssembler::cmovbe, true, false),
        (&CodeAssembler::cmove, true, false),
        (&CodeAssembler::cmovg, false, true),
        (&CodeAssembler::cmovge, true, true),
        (&CodeAssembler::cmovl, false, false),
        (&CodeAssembler::cmovle, true, false),
        (&CodeAssembler::cmovne, false, true),
        (&CodeAssembler::cmovno, false, true),
        (&CodeAssembler::cmovnp, false, true),
        (&CodeAssembler::cmovns, false, true),
        (&CodeAssembler::cmovo, true, false),
        (&CodeAssembler::cmovp, true, false),
        (&CodeAssembler::cmovs, true, false),
    ];

    for &(instruction, true_value, false_value) in variations {
        for flag_val in [true, false] {
            let mut cpu = run_test(
                RFlags::new(),
                |asm| instruction(asm, rax, ptr(0x200)),
                |cpu| {
                    cpu.valid_gva = 0x200;
                    cpu.mem_val = 84;
                    let mut rflags = cpu.rflags();
                    rflags = rflags
                        .with_carry(flag_val)
                        .with_overflow(flag_val)
                        .with_parity(flag_val)
                        .with_sign(flag_val)
                        .with_zero(flag_val);
                    cpu.set_rflags(rflags);
                },
            );

            if flag_val {
                assert_eq!(cpu.gp(Gp::RAX.into()) == 84, true_value);
            } else {
                assert_eq!(cpu.gp(Gp::RAX.into()) == 84, false_value);
            }
        }
    }
}

#[test]
fn cmov_false_truncation() {
    let mut cpu = run_test(
        RFlags::new(),
        |asm| asm.cmovo(r8d, dword_ptr(rax)),
        |cpu| {
            cpu.set_gp(Gp::R8.into(), 0x1234567890abcdef);
            let mut rflags = cpu.rflags();
            rflags.set_overflow(false);
            cpu.set_rflags(rflags)
        },
    );

    assert_eq!(cpu.gp(Gp::R8.into()), 0x90abcdef);
}
