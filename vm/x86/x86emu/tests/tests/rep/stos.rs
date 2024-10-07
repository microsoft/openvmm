// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::tests::common::run_wide_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::CpuState;
use x86emu::MAX_REP_LOOPS;

#[test]
fn stos() {
    const START_GVA: u64 = 0x100;

    let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, _, u64); 3] = [
        (&CodeAssembler::stosd, vec![0xAA, 0xAA, 0xAA, 0xAA], 4),
        (&CodeAssembler::stosw, vec![0xAA, 0xAA], 2),
        (&CodeAssembler::stosb, vec![0xAA], 1),
    ];

    for (instruction, value, size) in variations.into_iter() {
        for direction in [false, true] {
            let (state, cpu) = run_wide_test(
                RFlags::new(),
                true,
                |asm| instruction(asm),
                |state, cpu| {
                    state.rflags.set_direction(direction);
                    state.gps[CpuState::RAX] = 0xAAAAAAAAAAAAAAAA;
                    state.gps[CpuState::RDI] = START_GVA;
                    cpu.valid_gva = START_GVA;
                },
            );

            assert_eq!(cpu.mem_val, value);
            assert_eq!(
                state.gps[CpuState::RDI],
                START_GVA.wrapping_add(if direction { size.wrapping_neg() } else { size })
            );
        }
    }
}

#[test]
fn rep_stos() {
    const START_GVA: u64 = 0x100;

    for len in [0, 1, MAX_REP_LOOPS / 2, MAX_REP_LOOPS, MAX_REP_LOOPS + 1] {
        let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, _); 3] = [
            (&CodeAssembler::stosb, 1),
            (&CodeAssembler::stosw, 2),
            (&CodeAssembler::stosd, 4),
        ];

        for (instr, width) in variations {
            let (state, cpu) = run_wide_test(
                RFlags::new(),
                len <= MAX_REP_LOOPS,
                |asm| instr(asm.rep()),
                |state, cpu| {
                    cpu.valid_gva = START_GVA;
                    state.gps[CpuState::RDI] = START_GVA;
                    state.gps[CpuState::RAX] = 0xAAAAAAAAAAAAAAAA;
                    state.gps[CpuState::RCX] = len;
                    state.rflags.set_direction(false);
                },
            );

            assert_eq!(state.gps[CpuState::RCX], len.saturating_sub(MAX_REP_LOOPS));
            assert_eq!(
                cpu.mem_val.len() as u64,
                std::cmp::min(len, MAX_REP_LOOPS) * width
            );
            assert!(cpu.mem_val.iter().all(|&x| x == 0xAA));
        }
    }
}