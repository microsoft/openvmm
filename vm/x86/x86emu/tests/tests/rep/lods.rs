// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::CpuState;
use x86emu::MAX_REP_LOOPS;
use zerocopy::AsBytes;

#[test]
fn lods() {
    const START_GVA: u64 = 0x100;

    let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, u64, _); 3] = [
        (&CodeAssembler::lodsd, 0xAAAAAAAA, 4),
        (&CodeAssembler::lodsw, 0xAAAA, 2),
        (&CodeAssembler::lodsb, 0xAA, 1),
    ];

    for (instruction, value, size) in variations.into_iter() {
        for direction in [false, true] {
            let (state, _cpu) = run_wide_test(
                RFlags::new(),
                true,
                |asm| instruction(asm),
                |state, cpu| {
                    state.rflags.set_direction(direction);
                    state.gps[CpuState::RSI] = START_GVA;
                    cpu.valid_gva = START_GVA;
                    cpu.mem_val = vec![0xAA, 0xAA, 0xAA, 0xAA];
                },
            );

            assert_eq!(
                state.gps[CpuState::RAX].as_bytes()[..size],
                value.as_bytes()[..size]
            );
            assert_eq!(
                state.gps[CpuState::RSI],
                START_GVA.wrapping_add(if direction { size.wrapping_neg() } else { size } as u64)
            );
        }
    }
}

#[test]
fn rep_lods() {
    for len in [1, MAX_REP_LOOPS / 2, MAX_REP_LOOPS, MAX_REP_LOOPS + 1] {
        let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, _); 3] = [
            (&CodeAssembler::lodsd, 4),
            (&CodeAssembler::lodsw, 2),
            (&CodeAssembler::lodsb, 1),
        ];

        for (instr, width) in variations {
            let mut input_vec = vec![0; len as usize * width];
            for (index, val) in input_vec.iter_mut().enumerate() {
                *val = (index as u8).wrapping_mul(index as u8); // Create a decently random pattern
            }

            let (state, _cpu) = run_wide_test(
                RFlags::new(),
                len <= MAX_REP_LOOPS,
                |asm| instr(asm.rep()),
                |state, cpu| {
                    cpu.valid_gva = state.gps[CpuState::RSI];
                    cpu.mem_val.clone_from(&input_vec);
                    state.gps[CpuState::RCX] = len;
                    state.rflags.set_direction(false);
                },
            );

            assert_eq!(state.gps[CpuState::RCX], len.saturating_sub(MAX_REP_LOOPS));
            assert_eq!(
                input_vec[std::cmp::min(input_vec.len(), MAX_REP_LOOPS as usize * width) - width..]
                    [..width],
                state.gps[CpuState::RAX].as_bytes()[..width]
            );
        }
    }
}
