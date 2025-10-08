// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;
use x86emu::MAX_REP_LOOPS;
use zerocopy::IntoBytes;

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
            let mut cpu = run_wide_test(
                RFlags::new(),
                true,
                |asm| instruction(asm),
                |cpu| {
                    let mut rflags = cpu.rflags();
                    rflags.set_direction(direction);
                    cpu.set_rflags(rflags);

                    cpu.set_gp(Gp::RSI.into(), START_GVA);
                    cpu.valid_gva = START_GVA;
                    cpu.mem_val = vec![0xAA, 0xAA, 0xAA, 0xAA];
                },
            );

            assert_eq!(
                cpu.gp(Gp::RAX.into()).as_bytes()[..size],
                value.as_bytes()[..size]
            );
            assert_eq!(
                cpu.gp(Gp::RSI.into()),
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

            let mut cpu = run_wide_test(
                RFlags::new(),
                len <= MAX_REP_LOOPS,
                |asm| instr(asm.rep()),
                |cpu| {
                    cpu.valid_gva = cpu.gp(Gp::RSI.into());
                    cpu.mem_val.clone_from(&input_vec);
                    cpu.set_gp(Gp::RCX.into(), len);
                    let mut rflags = cpu.rflags();
                    rflags.set_direction(false);
                    cpu.set_rflags(rflags);
                },
            );

            assert_eq!(cpu.gp(Gp::RCX.into()), len.saturating_sub(MAX_REP_LOOPS));
            assert_eq!(
                input_vec[std::cmp::min(input_vec.len(), MAX_REP_LOOPS as usize * width) - width..]
                    [..width],
                cpu.gp(Gp::RAX.into()).as_bytes()[..width]
            );
        }
    }
}
