// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use x86defs::RFlags;

/// Updates the sign, zero, and parity flags.
pub(super) fn update_flags_szp(flags: &mut RFlags, operand_size: usize, result: u64) {
    let op_shift = 64 - operand_size as u32 * 8;
    // Check if the low bits are zero.
    let zero = result.wrapping_shl(op_shift) == 0;
    // Extract the sign bit.
    let signed = (result.wrapping_shl(op_shift) as i64) < 0;
    // Cleverly count the number of ones (mod 2) in the low byte.
    let parity = (0x9669 >> ((result ^ (result >> 4)) & 0xf)) & 1 != 0;

    flags.set_parity(parity);
    flags.set_zero(zero);
    flags.set_sign(signed);
}
