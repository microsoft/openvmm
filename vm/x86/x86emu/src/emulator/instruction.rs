// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions to help parse instructions.

use crate::Cpu;
use iced_x86::CodeSize;
use iced_x86::Instruction;
use iced_x86::Register;

// TODO: replace with just .address_size() if https://github.com/icedland/iced/issues/389 is accepted
pub fn address_size(instr: &Instruction) -> usize {
    if instr.memory_base() != Register::None {
        instr.memory_base().size()
    } else if instr.memory_index() != Register::None {
        instr.memory_index().size()
    } else if instr.memory_displ_size() >= 2 {
        instr.memory_displ_size() as usize
    } else {
        match instr.code_size() {
            CodeSize::Code64 => 8,
            CodeSize::Code32 => 4,
            CodeSize::Code16 => 2,
            CodeSize::Unknown => 8,
        }
    }
}

pub fn memory_op_offset<T: Cpu>(cpu: &mut T, instr: &Instruction, operand: u32) -> u64 {
    instr
        .virtual_address(operand, 0, |reg, _element_index, _element_size| {
            if reg.is_gpr() {
                Some(cpu.gp(reg.into()))
            } else if reg.is_segment_register() {
                // The segment base is ignored since it's applied in compute_and_validate_gva.
                Some(0)
            } else {
                todo!("missing register support {:?}", reg)
            }
        })
        .unwrap()
}
