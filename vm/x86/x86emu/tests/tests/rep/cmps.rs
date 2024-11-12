// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::CpuState;

#[test]
fn cmps() {
    const START_GVA: u64 = 0x100;
    const SCAN_VALUE: u64 = 0x1234567890abcdef;

    let variations: &[(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, usize)] = &[
        (&CodeAssembler::cmpsq, 8),
        (&CodeAssembler::cmpsd, 4),
        (&CodeAssembler::cmpsw, 2),
        (&CodeAssembler::cmpsb, 1),
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
                        state.gps[CpuState::RSI] = START_GVA;
                        state.gps[CpuState::RDI] = START_GVA + size as u64;
                        cpu.valid_gva = START_GVA;
                        cpu.mem_val = SCAN_VALUE.to_le_bytes()[..size]
                            .iter()
                            .copied()
                            .chain(value.to_le_bytes()[..size].iter().copied())
                            .collect();
                    },
                );

                assert_eq!(
                    state.gps[CpuState::RSI],
                    START_GVA
                        .wrapping_add(if direction { size.wrapping_neg() } else { size } as u64)
                );
                assert_eq!(
                    state.gps[CpuState::RDI],
                    (START_GVA + size as u64).wrapping_add(if direction {
                        size.wrapping_neg()
                    } else {
                        size
                    } as u64)
                );
                assert_eq!(state.rflags & RFLAGS_ARITH_MASK, flags.into());
            }
        }
    }
}
