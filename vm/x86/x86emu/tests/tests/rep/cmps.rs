// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::Cpu;
use x86emu::Gp;

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
                let mut cpu = run_wide_test(
                    RFLAGS_ARITH_MASK,
                    true,
                    |asm| instruction(asm),
                    |cpu| {
                        let mut rflags = cpu.rflags();
                        rflags.set_direction(direction);
                        cpu.set_rflags(rflags);
                        cpu.set_gp(Gp::RSI.into(), START_GVA);
                        cpu.set_gp(Gp::RDI.into(), START_GVA + size as u64);
                        cpu.valid_gva = START_GVA;
                        cpu.mem_val = SCAN_VALUE.to_le_bytes()[..size]
                            .iter()
                            .copied()
                            .chain(value.to_le_bytes()[..size].iter().copied())
                            .collect();
                    },
                );

                assert_eq!(
                    cpu.gp(Gp::RSI.into()),
                    START_GVA
                        .wrapping_add(if direction { size.wrapping_neg() } else { size } as u64)
                );
                assert_eq!(
                    cpu.gp(Gp::RDI.into()),
                    (START_GVA + size as u64).wrapping_add(if direction {
                        size.wrapping_neg()
                    } else {
                        size
                    } as u64)
                );
                assert_eq!(cpu.rflags() & RFLAGS_ARITH_MASK, flags.into());
            }
        }
    }
}
