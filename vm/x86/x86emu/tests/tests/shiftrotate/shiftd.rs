// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::tests::common::run_test;
use crate::tests::common::RFLAGS_LOGIC_MASK;
use iced_x86::code_asm::*;
use x86emu::CpuState;

fn shiftd_test(
    variations: &[(u64, u64, u32, u64, u64)],
    shift_op: impl Fn(&mut CodeAssembler, AsmMemoryOperand, AsmRegister64, u32) -> Result<(), IcedError>,
) {
    for &(left, right, count, result, rflags) in variations {
        let flags = if count == 0 {
            x86defs::RFlags::new()
        } else if count != 1 {
            RFLAGS_LOGIC_MASK.with_overflow(false)
        } else {
            RFLAGS_LOGIC_MASK
        };

        let (state, cpu) = run_test(
            flags,
            |asm| shift_op(asm, ptr(0x100), rax, count),
            |state, cpu| {
                state.gps[CpuState::RAX] = right;
                cpu.valid_gva = 0x100;
                cpu.mem_val = left;
            },
        );

        assert_eq!(cpu.mem_val, result);
        assert_eq!(state.rflags & flags, rflags.into());
    }
}

#[test]
fn shld() {
    let variations = &[
        (0x0, 0x0, 0, 0x0, 0x0),
        (0x7fffffffffffffff, 0x0, 1, 0xfffffffffffffffe, 0x880),
        (0x0, 0x0, 1, 0x0, 0x44),
        (0x64, 0x64, 1, 0xc8, 0x0),
        (0x0, 0x1, 2, 0x0, 0x44),
        (0x1, 0x0, 3, 0x8, 0x0),
        (0xffffffffffffffff, 0x0, 4, 0xfffffffffffffff0, 0x85),
        (0xffffffffffffffff, 0xffffffff, 5, 0xffffffffffffffe0, 0x81),
        (0xffffffff, 0xffffffffffffffff, 6, 0x3fffffffff, 0x4),
        (0xffffffff, 0xffffffff, 7, 0x7fffffff80, 0x0),
        (0x7fffffffffffffff, 0x0, 8, 0xffffffffffffff00, 0x85),
        (0x7fffffff, 0x0, 9, 0xfffffffe00, 0x4),
        (0x0, 0x7fffffff, 10, 0x0, 0x44),
        (0x80000000, 0x7fffffff, 11, 0x40000000000, 0x4),
        (0x7fffffff, 0x80000000, 12, 0x7fffffff000, 0x4),
        (0x8000000000000000, 0x7fffffff, 13, 0x0, 0x44),
        (0x7fffffff, 0x8000000000000000, 14, 0x1fffffffe000, 0x4),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            15,
            0xffffffffffffbfff,
            0x85,
        ),
        (0x8000000000000000, 0x7fffffffffffffff, 16, 0x7fff, 0x4),
        (0x8000000000000000, 0x8000000000000000, 17, 0x10000, 0x4),
    ];
    shiftd_test(variations, CodeAssembler::shld);
}

#[test]
fn shrd() {
    let variations = &[
        (0x0, 0x0, 0, 0x0, 0x0),
        (0xffffffffffffffff, 0x0, 1, 0x7fffffffffffffff, 0x805),
        (0x0, 0x0, 1, 0x0, 0x44),
        (0x64, 0x64, 1, 0x32, 0x0),
        (0x0, 0x1, 2, 0x4000000000000000, 0x4),
        (0x1, 0x0, 3, 0x0, 0x44),
        (0xffffffffffffffff, 0x0, 4, 0xfffffffffffffff, 0x5),
        (0xffffffffffffffff, 0xffffffff, 5, 0xffffffffffffffff, 0x85),
        (0xffffffff, 0xffffffffffffffff, 6, 0xfc00000003ffffff, 0x85),
        (0xffffffff, 0xffffffff, 7, 0xfe00000001ffffff, 0x85),
        (0x7fffffffffffffff, 0x0, 8, 0x7fffffffffffff, 0x5),
        (0x7fffffff, 0x0, 9, 0x3fffff, 0x5),
        (0x0, 0x7fffffff, 10, 0xffc0000000000000, 0x84),
        (0x80000000, 0x7fffffff, 11, 0xffe0000000100000, 0x84),
        (0x7fffffff, 0x80000000, 12, 0x7ffff, 0x5),
        (0x8000000000000000, 0x7fffffff, 13, 0xfffc000000000000, 0x84),
        (0x7fffffff, 0x8000000000000000, 14, 0x1ffff, 0x5),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            15,
            0xfffeffffffffffff,
            0x85,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            16,
            0xffff800000000000,
            0x84,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            17,
            0x400000000000,
            0x4,
        ),
    ];
    shiftd_test(variations, CodeAssembler::shrd);
}

fn shiftd_underflow_test(
    variations: &[(u32, u16, u64)],
    shift_op: impl Fn(&mut CodeAssembler, AsmMemoryOperand, AsmRegister16, u32) -> Result<(), IcedError>,
) {
    for &(count, result, rflags) in variations {
        let flagcount = count % 32;
        let flags = if flagcount == 0 {
            x86defs::RFlags::new()
        } else if flagcount != 1 {
            RFLAGS_LOGIC_MASK.with_overflow(false)
        } else {
            RFLAGS_LOGIC_MASK
        };

        let (state, cpu) = run_test(
            flags,
            |asm| shift_op(asm, ptr(0x100), ax, count),
            |state, cpu| {
                state.gps[CpuState::RAX] = 0;
                cpu.valid_gva = 0x100;
                cpu.mem_val = 0xFFFF;
            },
        );
        assert_eq!(cpu.mem_val, result.into());
        assert_eq!(state.rflags & flags, rflags.into());
    }
}

#[test]
fn shld_underflow() {
    let variations = &[
        (0, 0xffff, 0x0),
        (1, 0xfffe, 0x81),
        (2, 0xfffc, 0x85),
        (3, 0xfff8, 0x81),
        (4, 0xfff0, 0x85),
        (5, 0xffe0, 0x81),
        (6, 0xffc0, 0x85),
        (7, 0xff80, 0x81),
        (8, 0xff00, 0x85),
        (9, 0xfe00, 0x85),
        (10, 0xfc00, 0x85),
        (11, 0xf800, 0x85),
        (12, 0xf000, 0x85),
        (13, 0xe000, 0x85),
        (14, 0xc000, 0x85),
        (15, 0x8000, 0x85),
        (16, 0x0, 0x45),
        (17, 0x1, 0x0),
        (18, 0x3, 0x4),
        (19, 0x7, 0x0),
        (20, 0xf, 0x4),
        (21, 0x1f, 0x0),
        (22, 0x3f, 0x4),
        (23, 0x7f, 0x0),
        (24, 0xff, 0x4),
        (25, 0x1ff, 0x4),
        (26, 0x3ff, 0x4),
        (27, 0x7ff, 0x4),
        (28, 0xfff, 0x4),
        (29, 0x1fff, 0x4),
        (30, 0x3fff, 0x4),
        (31, 0x7fff, 0x4),
        (32, 0xffff, 0x0),
        (33, 0xfffe, 0x81),
        (34, 0xfffc, 0x85),
        (35, 0xfff8, 0x81),
        (36, 0xfff0, 0x85),
        (37, 0xffe0, 0x81),
        (38, 0xffc0, 0x85),
        (39, 0xff80, 0x81),
        (40, 0xff00, 0x85),
        (41, 0xfe00, 0x85),
        (42, 0xfc00, 0x85),
        (43, 0xf800, 0x85),
        (44, 0xf000, 0x85),
        (45, 0xe000, 0x85),
        (46, 0xc000, 0x85),
        (47, 0x8000, 0x85),
        (48, 0x0, 0x45),
        (49, 0x1, 0x0),
        (50, 0x3, 0x4),
        (51, 0x7, 0x0),
        (52, 0xf, 0x4),
        (53, 0x1f, 0x0),
        (54, 0x3f, 0x4),
        (55, 0x7f, 0x0),
        (56, 0xff, 0x4),
        (57, 0x1ff, 0x4),
        (58, 0x3ff, 0x4),
        (59, 0x7ff, 0x4),
        (60, 0xfff, 0x4),
        (61, 0x1fff, 0x4),
        (62, 0x3fff, 0x4),
        (63, 0x7fff, 0x4),
        (64, 0xffff, 0x0),
    ];
    shiftd_underflow_test(variations, CodeAssembler::shld);
}

#[test]
fn shrd_underflow() {
    let variations = &[
        (0, 0xffff, 0x0),
        (1, 0x7fff, 0x805),
        (2, 0x3fff, 0x5),
        (3, 0x1fff, 0x5),
        (4, 0xfff, 0x5),
        (5, 0x7ff, 0x5),
        (6, 0x3ff, 0x5),
        (7, 0x1ff, 0x5),
        (8, 0xff, 0x5),
        (9, 0x7f, 0x1),
        (10, 0x3f, 0x5),
        (11, 0x1f, 0x1),
        (12, 0xf, 0x5),
        (13, 0x7, 0x1),
        (14, 0x3, 0x5),
        (15, 0x1, 0x1),
        (16, 0x0, 0x45),
        (17, 0x8000, 0x84),
        (18, 0xc000, 0x84),
        (19, 0xe000, 0x84),
        (20, 0xf000, 0x84),
        (21, 0xf800, 0x84),
        (22, 0xfc00, 0x84),
        (23, 0xfe00, 0x84),
        (24, 0xff00, 0x84),
        (25, 0xff80, 0x80),
        (26, 0xffc0, 0x84),
        (27, 0xffe0, 0x80),
        (28, 0xfff0, 0x84),
        (29, 0xfff8, 0x80),
        (30, 0xfffc, 0x84),
        (31, 0xfffe, 0x80),
        (32, 0xffff, 0x0),
        (33, 0x7fff, 0x805),
        (34, 0x3fff, 0x5),
        (35, 0x1fff, 0x5),
        (36, 0xfff, 0x5),
        (37, 0x7ff, 0x5),
        (38, 0x3ff, 0x5),
        (39, 0x1ff, 0x5),
        (40, 0xff, 0x5),
        (41, 0x7f, 0x1),
        (42, 0x3f, 0x5),
        (43, 0x1f, 0x1),
        (44, 0xf, 0x5),
        (45, 0x7, 0x1),
        (46, 0x3, 0x5),
        (47, 0x1, 0x1),
        (48, 0x0, 0x45),
        (49, 0x8000, 0x84),
        (50, 0xc000, 0x84),
        (51, 0xe000, 0x84),
        (52, 0xf000, 0x84),
        (53, 0xf800, 0x84),
        (54, 0xfc00, 0x84),
        (55, 0xfe00, 0x84),
        (56, 0xff00, 0x84),
        (57, 0xff80, 0x80),
        (58, 0xffc0, 0x84),
        (59, 0xffe0, 0x80),
        (60, 0xfff0, 0x84),
        (61, 0xfff8, 0x80),
        (62, 0xfffc, 0x84),
        (63, 0xfffe, 0x80),
        (64, 0xffff, 0x0),
    ];
    shiftd_underflow_test(variations, CodeAssembler::shrd);
}