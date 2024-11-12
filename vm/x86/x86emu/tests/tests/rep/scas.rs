// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use crate::tests::common::MultipleCellCpu;
use crate::tests::common::RFLAGS_ARITH_MASK;
use futures::FutureExt;
use iced_x86::code_asm::*;
use x86defs::cpuid::Vendor;
use x86emu::CpuState;
use x86emu::Emulator;

#[test]
fn scas() {
    const START_GVA: u64 = 0x100;
    const SCAN_VALUE: u64 = 0x1234567890abcdef;

    let variations: &[(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, usize)] = &[
        (&CodeAssembler::scasq, 8),
        (&CodeAssembler::scasd, 4),
        (&CodeAssembler::scasw, 2),
        (&CodeAssembler::scasb, 1),
    ];

    for &(instruction, size) in variations {
        for direction in [false, true] {
            for (value, flags) in [
                (SCAN_VALUE, 0x44),
                (SCAN_VALUE + 1, 0x85),
                (SCAN_VALUE - 1, 0),
            ] {
                let (state, _cpu) = run_wide_test(
                    RFLAGS_ARITH_MASK,
                    true,
                    |asm| instruction(asm),
                    |state, cpu| {
                        state.rflags.set_direction(direction);
                        state.gps[CpuState::RAX] = SCAN_VALUE;
                        state.gps[CpuState::RDI] = START_GVA;
                        cpu.valid_gva = START_GVA;
                        cpu.mem_val = value.to_le_bytes().into();
                    },
                );

                assert_eq!(
                    state.gps[CpuState::RDI],
                    START_GVA
                        .wrapping_add(if direction { size.wrapping_neg() } else { size } as u64)
                );
                assert_eq!(state.rflags & RFLAGS_ARITH_MASK, flags.into());
            }
        }
    }
}

#[test]
fn rep_scas() {
    let variations: &[(
        &dyn Fn(&mut CodeAssembler) -> &mut CodeAssembler,
        _,
        _,
        _,
        _,
    )] = &[
        (&CodeAssembler::repne, vec![0, 0, 6, 0, 0], 6, 5, 3),
        (&CodeAssembler::repne, vec![0, 0, 0, 0, 0], 6, 4, 4),
        (&CodeAssembler::repe, vec![3, 3, 3, 7, 3, 3], 3, 6, 4),
        (&CodeAssembler::repe, vec![0, 0, 0, 0, 0, 0], 0, 5, 5),
    ];

    for &(rep, ref range, value, len, result) in variations {
        let (state, _cpu) = run_wide_test(
            RFLAGS_ARITH_MASK,
            true,
            |asm| rep(asm).scasb(),
            |state, cpu| {
                state.rflags.set_direction(false);
                state.gps[CpuState::RAX] = value;
                state.gps[CpuState::RCX] = len;
                state.gps[CpuState::RDI] = 0;
                cpu.valid_gva = 0;
                cpu.mem_val.clone_from(range);
            },
        );

        assert_eq!(state.gps[CpuState::RDI], result);
        assert_eq!(state.gps[CpuState::RCX], len - result);
    }
}

#[test]
fn rep_scas_unchanging_rflags() {
    let variations: &[(&dyn Fn(&mut CodeAssembler) -> &mut CodeAssembler, _, _)] = &[
        (&CodeAssembler::repne, 6, 3),
        (&CodeAssembler::repne, 6, 0),
        (&CodeAssembler::repe, 0, 3),
        (&CodeAssembler::repe, 0, 0),
    ];

    for &(rep, value, len) in variations {
        let mut state = crate::tests::common::initial_state(0.into());
        let mut cpu = MultipleCellCpu::default();
        state.gps[CpuState::RAX] = value;
        state.gps[CpuState::RCX] = len;
        state.gps[CpuState::RDI] = 0;
        cpu.valid_gva = 0;
        cpu.mem_val = vec![0, 0];
        let mut assembler = CodeAssembler::new(64).unwrap();
        rep(&mut assembler).scasb().unwrap();
        let bytes = assembler.assemble(0).unwrap();
        let _might_fail = Emulator::new(&mut cpu, &mut state, Vendor::INTEL, &bytes)
            .run()
            .now_or_never()
            .unwrap();

        assert_eq!(state.rflags, 0.into()); // rflags should not change on fault or count of 0
    }
}
