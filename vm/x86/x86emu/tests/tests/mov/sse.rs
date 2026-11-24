// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_u128_test;
use crate::tests::common::{SingleCellCpu, TestCpu};
use futures::FutureExt;
use iced_x86::code_asm::*;
use test_with_tracing::test;
use x86defs::RFlags;
use x86emu::Cpu;

#[test]
fn non_temporal_stores_require_alignment() {
    for instruction in [
        CodeAssembler::movntdq,
        CodeAssembler::movntps,
        CodeAssembler::movntpd,
    ] {
        let mut cpu = SingleCellCpu::<u128>::new(0.into());
        cpu.valid_gva = 0x205;
        cpu.mem_val = 0x1234;
        cpu.set_xmm(0, 0x5678);
        let mut asm = CodeAssembler::new(64).unwrap();
        instruction(&mut asm, xmmword_ptr(0x205), xmm0).unwrap();
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
        assert_eq!(cpu.mem_val, 0x1234);
        assert_eq!(cpu.state.rip, 0);
    }
}

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
