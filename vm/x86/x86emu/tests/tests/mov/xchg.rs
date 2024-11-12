// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use iced_x86::code_asm::dword_ptr;
use iced_x86::code_asm::ebx;
use x86emu::CpuState;

#[test]
fn test_xchg() {
    let (state, cpu) = run_lockable_test(
        0.into(),
        LockTestBehavior::FailImplicitLock,
        |asm| asm.xchg(dword_ptr(0x100), ebx),
        |state, cpu| {
            cpu.valid_gva = 0x100;
            cpu.mem_val = 0xffffffff_12345678u64;
            state.gps[CpuState::RBX] = 0x55555555;
        },
    );
    assert_eq!(cpu.mem_val, 0xffffffff_55555555);
    assert_eq!(state.gps[CpuState::RBX], 0x12345678);
}
