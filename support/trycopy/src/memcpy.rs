// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arch-independent functions for copying and setting memory in chunks, using
//! arch-specific low-level functions for the actual copying and setting.

use super::Fault;

/// Copy memory from `src` to `dest` in chunks in the forward direction, using
/// the provided functions for copying 1, 8, and 32 bytes at a time.
///
/// `copy1`, `copy8`, and `copy32` can assume that `length` is non-zero and a
/// multiple of 1, 8, and 32 respectively.
pub(crate) fn try_copy_forward_with(
    mut dest: *mut u8,
    mut src: *const u8,
    mut length: usize,
    copy1: impl Fn(*mut u8, *const u8, usize) -> Result<(), Fault>,
    copy8: impl Fn(*mut u8, *const u8, usize) -> Result<(), Fault>,
    copy32: impl Fn(*mut u8, *const u8, usize) -> Result<(), Fault>,
) -> Result<(), Fault> {
    if length >= 32 {
        let this = length & !31;
        copy32(dest, src, this)?;
        dest = dest.wrapping_add(this);
        src = src.wrapping_add(this);
        length &= 31;
    }
    if length >= 8 {
        let this = length & !7;
        copy8(dest, src, this)?;
        dest = dest.wrapping_add(this);
        src = src.wrapping_add(this);
        length &= 7;
    }
    if length >= 1 {
        copy1(dest, src, length)?;
    }
    Ok(())
}

/// Set memory at `dest` to byte `c` in chunks, using the provided functions
/// for setting 1, 8, and 32 bytes at a time.
///
/// `set1`, `set8`, and `set32_zero` can assume that `length` is non-zero and a
/// multiple of 1, 8, and 32 respectively. Note that `set32_zero` is used for
/// setting 32-byte chunks to zero only.
pub(crate) fn try_memset_with(
    mut dest: *mut u8,
    c: u8,
    mut length: usize,
    set1: impl Fn(*mut u8, u8, usize) -> Result<(), Fault>,
    set8: impl Fn(*mut u8, u8, usize) -> Result<(), Fault>,
    set32_zero: impl Fn(*mut u8, usize) -> Result<(), Fault>,
) -> Result<(), Fault> {
    if c == 0 && length >= 32 {
        let this = length & !31;
        set32_zero(dest, this)?;
        dest = dest.wrapping_add(this);
        length &= 31;
    }
    if length >= 8 {
        let this = length & !7;
        set8(dest, c, this)?;
        dest = dest.wrapping_add(this);
        length &= 7;
    }
    if length >= 1 {
        set1(dest, c, length)?;
    }
    Ok(())
}
