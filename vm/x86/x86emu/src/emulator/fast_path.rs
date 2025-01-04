// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Emulation fast paths for specific use cases.

use super::instruction;
use crate::emulator::arith::ArithOp;
use crate::emulator::arith::OrOp;
use crate::registers::bitness;
use crate::registers::Bitness;
use crate::registers::Segment;
use crate::Cpu;
use iced_x86::OpKind;

const PAGE_SIZE: u32 = 4096;

/// Emulate atomic single-bit writes to a page.
///
/// This decodes `bts` and `or` instructions (which can and probably will have
/// lock prefixes). It assumes the caller knows which physical page is being
/// modified.
///
/// This is more efficient than going through the full emulation path for cases
/// where the guest is likely to be performing single-bit writes. This is
/// particularly useful for emulating Hyper-V monitor pages.
///
/// If the fast path is possible, updates the register state, advances the
/// instruction pointer, and returns the bit number being set within the 4K
/// page.
///
/// If the fast path is impossible, returns `None`. The caller should use the
/// full emulator.
pub fn emulate_fast_path_set_bit<T: Cpu>(instruction_bytes: &[u8], cpu: &mut T) -> Option<u32> {
    if cpu.rflags().trap() {
        return None;
    }

    let bitness = bitness(cpu.cr0(), cpu.efer(), cpu.segment(Segment::CS));
    let mut decoder = iced_x86::Decoder::new(bitness.into(), instruction_bytes, 0);
    decoder.set_ip(cpu.rip());

    let instr = decoder.decode();
    let mut rflags = cpu.rflags();
    let (address, bit) = match instr.code() {
        // [lock] bts m, r
        //
        // Used by Windows and Linux kernel drivers.
        iced_x86::Code::Bts_rm64_r64 | iced_x86::Code::Bts_rm32_r32
            if instr.op0_kind() == OpKind::Memory =>
        {
            let op_size = instr.memory_size().size() as u8 as i64;

            // When in the register form, the offset is treated as a signed value
            let bit_offset = cpu.gp_sign_extend(instr.op1_register().into());

            let address_size = instruction::address_size(&instr);

            let bit_base = instruction::memory_op_offset(cpu, &instr, 0);
            let address_mask = u64::MAX >> (64 - address_size * 8);
            let address = bit_base
                .wrapping_add_signed(op_size * bit_offset.div_euclid(op_size * 8))
                & address_mask;

            let bit = bit_offset.rem_euclid(op_size * 8) as u32;

            rflags.set_carry(false);
            (address, bit)
        }
        // [lock] or m, r
        //
        // Used by DPDK.
        iced_x86::Code::Or_rm32_r32
        | iced_x86::Code::Or_rm64_r64
        | iced_x86::Code::Or_rm8_r8
        | iced_x86::Code::Or_rm16_r16
            if instr.op0_kind() == OpKind::Memory =>
        {
            let address = instruction::memory_op_offset(cpu, &instr, 0);
            let mask = cpu.gp(instr.op1_register().into());
            if !mask.is_power_of_two() {
                tracing::debug!(mask, "fast path set bit: or without exactly one bit");
                return None;
            }
            OrOp::update_flags(&mut rflags, instr.memory_size().size(), mask, 0, mask);
            (address, mask.trailing_zeros())
        }
        iced_x86::Code::INVALID => {
            tracing::debug!(error = ?decoder.last_error(), "fast path set bit decode failure");
            return None;
        }
        _ => {
            tracing::debug!(bytes = ?instruction_bytes[..instr.len()], "unsupported instruction for fast path set bit");
            return None;
        }
    };

    let seg = cpu.segment(instr.memory_segment().into());
    let offset = page_offset(address.wrapping_add(seg.base));

    // Ensure the access doesn't straddle a page boundary.
    if offset > PAGE_SIZE - instr.memory_size().size() as u32 {
        return None;
    }

    // If there is a possibility of a segmentation violation, take the slow
    // path.
    if matches!(bitness, Bitness::Bit32 | Bitness::Bit16)
        && page_offset(seg.base | seg.limit.wrapping_add(1) as u64) != 0
    {
        return None;
    }

    let bit_in_page = offset * 8 + bit;
    tracing::trace!(bit_in_page, "fast path set bit");

    cpu.set_rip(instr.next_ip());
    cpu.set_rflags(rflags);
    Some(bit_in_page)
}

fn page_offset(address: u64) -> u32 {
    address as u32 & (PAGE_SIZE - 1)
}
