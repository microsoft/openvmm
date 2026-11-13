// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common::{SingleCellCpu, TestCpu};
use futures::FutureExt;
use iced_x86::code_asm::*;
use test_with_tracing::test;
use x86defs::cpuid::Vendor;
use x86emu::{Emulator, Gp};

mod adc;
mod add;
mod and;
mod cmp;
mod cmpxchg;
mod incdec;
mod neg;
mod not;
mod or;
mod sbb;
mod sub;
mod test;
mod xadd;
mod xor;

#[test]
fn memory_access_intent() {
    let cases: &[(
        &str,
        fn(&mut CodeAssembler) -> Result<(), IcedError>,
        bool,
        u64,
    )] = &[
        ("add", |a| a.add(qword_ptr(0x100), rbx), true, 4),
        ("or", |a| a.or(qword_ptr(0x100), rbx), true, 3),
        ("inc", |a| a.inc(qword_ptr(0x100)), true, 4),
        (
            "neg",
            |a| a.neg(qword_ptr(0x100)),
            true,
            3u64.wrapping_neg(),
        ),
        ("not", |a| a.not(qword_ptr(0x100)), true, !3u64),
        ("xadd", |a| a.xadd(qword_ptr(0x100), rbx), true, 4),
        ("xchg", |a| a.xchg(qword_ptr(0x100), rbx), true, 1),
        ("cmpxchg", |a| a.cmpxchg(qword_ptr(0x100), rbx), true, 3),
        ("shl", |a| a.shl(qword_ptr(0x100), 1), true, 6),
        ("sar", |a| a.sar(qword_ptr(0x100), 1), true, 1),
        ("shld", |a| a.shld(qword_ptr(0x100), rbx, 1), true, 6),
        (
            "shrd",
            |a| a.shrd(qword_ptr(0x100), rbx, 1),
            true,
            0x8000_0000_0000_0001,
        ),
        ("bts", |a| a.bts(qword_ptr(0x100), 2), true, 7),
        ("btr", |a| a.btr(qword_ptr(0x100), 0), true, 2),
        ("btc", |a| a.btc(qword_ptr(0x100), 0), true, 2),
        ("cmp", |a| a.cmp(qword_ptr(0x100), rbx), false, 3),
        ("test", |a| a.test(qword_ptr(0x100), rbx), false, 3),
        ("bt", |a| a.bt(qword_ptr(0x100), 1), false, 3),
        ("mov", |a| a.mov(rax, qword_ptr(0x100)), false, 3),
    ];
    for &(name, instruction, is_rmw, expected_memory) in cases {
        let mut cpu = SingleCellCpu::<u64>::new(0.into());
        cpu.valid_gva = 0x100;
        cpu.mem_val = 3;
        cpu.state.gps[Gp::RBX as usize] = 1;
        let mut asm = CodeAssembler::new(64).unwrap();
        instruction(&mut asm).unwrap();
        let bytes = asm.assemble(0).unwrap();
        Emulator::new(&mut cpu, Vendor::INTEL, &bytes)
            .run()
            .now_or_never()
            .unwrap()
            .unwrap();
        assert_eq!(cpu.memory_reads, [(0x100, 8, is_rmw)], "{name}");
        assert_eq!(cpu.mem_val, expected_memory, "{name}");
        assert_eq!(cpu.state.rip, bytes.len() as u64, "{name}");
    }
}
