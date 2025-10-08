// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_wide_test;
use futures::FutureExt;
use iced_x86::code_asm::*;
use x86defs::RFlags;
use x86emu::Cpu;
use x86emu::Gp;

#[test]
fn movs() {
    const START_GVA: u64 = 0x100;

    let variations: [(&dyn Fn(&mut CodeAssembler) -> Result<(), IcedError>, _, u64); 3] = [
        (&CodeAssembler::movsd, vec![0xAA, 0xAA, 0xAA, 0xAA], 4),
        (&CodeAssembler::movsw, vec![0xAA, 0xAA], 2),
        (&CodeAssembler::movsb, vec![0xAA], 1),
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
                    cpu.set_gp(Gp::RDI.into(), START_GVA + size);
                    cpu.valid_gva = START_GVA;
                    cpu.write_memory(START_GVA, &value, false)
                        .now_or_never()
                        .unwrap()
                        .unwrap();
                },
            );

            // Most of the behavior being tested here is verified by how the fake cpu handles memory.
            assert_eq!(cpu.mem_val, value);
            assert_eq!(
                cpu.gp(Gp::RSI.into()),
                START_GVA.wrapping_add(if direction { size.wrapping_neg() } else { size })
            );
            assert_eq!(
                cpu.gp(Gp::RDI.into()),
                (START_GVA + size).wrapping_add(if direction { size.wrapping_neg() } else { size })
            );
        }
    }
}
