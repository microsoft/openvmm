// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_u128_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;

#[test]
fn mov_regvalue_to_memory_sse() {
    let variations: &[&dyn Fn(
        &mut CodeAssembler,
        AsmMemoryOperand,
        AsmRegisterXmm,
    ) -> Result<(), IcedError>] = &[
        &CodeAssembler::movaps,
        &CodeAssembler::movapd,
        &CodeAssembler::movups,
        &CodeAssembler::movupd,
        &CodeAssembler::movdqa,
        &CodeAssembler::movdqu,
        &CodeAssembler::movntdq,
        &CodeAssembler::movntps,
        &CodeAssembler::movntpd,
    ];

    for instr in variations {
        let cpu = run_u128_test(
            RFlags::new(),
            |asm| instr(asm, xmmword_ptr(0x200), xmm15),
            |cpu| {
                cpu.valid_gva = 0x200;
                cpu.set_xmm(15, 0x1234567890abcdef13579ace24680bdf);
            },
        );

        assert_eq!(cpu.mem_val, 0x1234567890abcdef13579ace24680bdfu128);
    }
}

#[test]
fn mov_memory_to_regvalue_sse() {
    let variations: &[&dyn Fn(
        &mut CodeAssembler,
        AsmRegisterXmm,
        AsmMemoryOperand,
    ) -> Result<(), IcedError>] = &[
        &CodeAssembler::movaps,
        &CodeAssembler::movapd,
        &CodeAssembler::movups,
        &CodeAssembler::movupd,
        &CodeAssembler::movdqa,
        &CodeAssembler::movdqu,
    ];

    for instr in variations {
        let mut cpu = run_u128_test(
            RFlags::new(),
            |asm| instr(asm, xmm15, xmmword_ptr(0x200)),
            |cpu| {
                cpu.valid_gva = 0x200;
                cpu.mem_val = 0x1234567890abcdef13579ace24680bdfu128;
            },
        );

        assert_eq!(cpu.xmm(15), 0x1234567890abcdef13579ace24680bdf);
    }
}

#[test]
#[should_panic(expected = "MandatoryAlignment")]
fn movaps_unaligned() {
    run_u128_test(
        RFlags::new(),
        |asm| asm.movaps(xmmword_ptr(0x205), xmm15),
        |cpu| {
            cpu.valid_gva = 0x205;
            cpu.set_xmm(15, 0x1234567890abcdef13579ace24680bdf);
        },
    );
}

#[test]
#[should_panic(expected = "MandatoryAlignment")]
fn movapd_unaligned() {
    run_u128_test(
        RFlags::new(),
        |asm| asm.movapd(xmmword_ptr(0x205), xmm15),
        |cpu| {
            cpu.valid_gva = 0x205;
            cpu.set_xmm(15, 0x1234567890abcdef13579ace24680bdf);
        },
    );
}

#[test]
#[should_panic(expected = "MandatoryAlignment")]
fn movdqa_unaligned() {
    run_u128_test(
        RFlags::new(),
        |asm| asm.movdqa(xmmword_ptr(0x205), xmm15),
        |cpu| {
            cpu.valid_gva = 0x205;
            cpu.set_xmm(15, 0x1234567890abcdef13579ace24680bdf);
        },
    );
}
