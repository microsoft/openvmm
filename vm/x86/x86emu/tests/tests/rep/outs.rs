// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::CpuState;
use x86emu::MAX_REP_LOOPS;

#[test]
fn outs() {
    const START_GVA: u64 = 0x100;

    let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, _, u64); 3] = [
        (&CodeAssembler::outsd, vec![0xAA, 0xAA, 0xAA, 0xAA], 4),
        (&CodeAssembler::outsw, vec![0xAA, 0xAA], 2),
        (&CodeAssembler::outsb, vec![0xAA], 1),
    ];

    for (instruction, value, size) in variations.into_iter() {
        for direction in [false, true] {
            let (state, cpu) = run_wide_test(
                RFlags::new(),
                true,
                |asm| instruction(asm),
                |state, cpu| {
                    let port = 0x3f9;
                    cpu.valid_io_port = port;
                    state.gps[CpuState::RDX] = port.into();

                    state.rflags.set_direction(direction);

                    state.gps[CpuState::RSI] = START_GVA;
                    cpu.valid_gva = START_GVA;
                    cpu.mem_val = vec![0xAA, 0xAA, 0xAA, 0xAA];
                },
            );

            assert_eq!(cpu.io_val, value);
            assert_eq!(
                state.gps[CpuState::RSI],
                START_GVA.wrapping_add(if direction { size.wrapping_neg() } else { size })
            );
        }
    }
}

#[test]
fn rep_outs() {
    const PORT: u16 = 0x3f9;

    for len in [0, 1, MAX_REP_LOOPS / 2, MAX_REP_LOOPS, MAX_REP_LOOPS + 1] {
        let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, _); 3] = [
            (&CodeAssembler::outsb, 1),
            (&CodeAssembler::outsw, 2),
            (&CodeAssembler::outsd, 4),
        ];

        for (instr, width) in variations {
            let mut input_vec = vec![0; (len * width) as usize];
            for (index, val) in input_vec.iter_mut().enumerate() {
                *val = (index as u8).wrapping_mul(index as u8); // Create a decently random pattern
            }

            let (state, cpu) = run_wide_test(
                RFlags::new(),
                len <= MAX_REP_LOOPS,
                |asm| instr(asm.rep()),
                |state, cpu| {
                    cpu.valid_gva = state.gps[CpuState::RSI];
                    cpu.mem_val.clone_from(&input_vec);
                    cpu.valid_io_port = PORT;
                    state.gps[CpuState::RDX] = PORT.into();
                    state.gps[CpuState::RCX] = len;
                    state.rflags.set_direction(false);
                },
            );

            assert_eq!(state.gps[CpuState::RCX], len.saturating_sub(MAX_REP_LOOPS));
            assert_eq!(
                cpu.io_val.len() as u64,
                std::cmp::min(len, MAX_REP_LOOPS) * width
            );
            assert_eq!(input_vec[..cpu.io_val.len()], cpu.io_val);
        }
    }
}
