// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]
// UNSAFETY: fuzzing unsafe memory operations
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

use arbitrary::Arbitrary;
use std::mem::{align_of, size_of};
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

const MAX_BUFFER_SIZE: usize = 4096;

#[derive(Debug, Arbitrary)]
enum TryCopyOp {
    Copy {
        src_offset: usize,
        dest_offset: usize,
        count: usize,
    },
    WriteBytes {
        offset: usize,
        value: u8,
        count: usize,
    },
    ReadVolatileU8 {
        offset: usize,
    },
    ReadVolatileU16 {
        offset: usize,
    },
    ReadVolatileU32 {
        offset: usize,
    },
    ReadVolatileU64 {
        offset: usize,
    },
    WriteVolatileU8 {
        offset: usize,
        value: u8,
    },
    WriteVolatileU16 {
        offset: usize,
        value: u16,
    },
    WriteVolatileU32 {
        offset: usize,
        value: u32,
    },
    WriteVolatileU64 {
        offset: usize,
        value: u64,
    },
    CompareExchangeU8 {
        offset: usize,
        current: u8,
        new: u8,
    },
    CompareExchangeU16 {
        offset: usize,
        current: u16,
        new: u16,
    },
    CompareExchangeU32 {
        offset: usize,
        current: u32,
        new: u32,
    },
    CompareExchangeU64 {
        offset: usize,
        current: u64,
        new: u64,
    },
}

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    buffer_size: u16, // Keep it small for performance
    operations: Vec<TryCopyOp>,
}

/// Helper function to check if a typed read/write operation is safe
fn is_aligned_op_safe<T>(offset: usize, buffer_size: usize) -> bool {
    let size = size_of::<T>();
    let align = align_of::<T>();
    offset % align == 0 && offset + size <= buffer_size
}

/// Helper function to calculate safe copy length for two-pointer operations
fn safe_copy_length(
    src_offset: usize,
    dest_offset: usize,
    requested: usize,
    buffer_size: usize,
) -> Option<usize> {
    if src_offset >= buffer_size || dest_offset >= buffer_size {
        return None;
    }
    let max_from_src = buffer_size.saturating_sub(src_offset);
    let max_from_dest = buffer_size.saturating_sub(dest_offset);
    let safe_len = requested.min(max_from_src).min(max_from_dest);
    if safe_len > 0 { Some(safe_len) } else { None }
}

fn do_fuzz(input: FuzzInput) {
    // Initialize trycopy
    trycopy::initialize_try_copy();

    // Create a buffer to work with - limit size to avoid OOM
    let buffer_size = (input.buffer_size as usize).min(MAX_BUFFER_SIZE).max(1);
    let mut buffer = vec![0u8; buffer_size];
    let base_ptr = buffer.as_mut_ptr();

    fuzz_eprintln!("Testing with buffer size: {}", buffer_size);

    // Execute all operations
    for op in input.operations {
        match op {
            TryCopyOp::Copy {
                src_offset,
                dest_offset,
                count,
            } => {
                if let Some(safe_count) =
                    safe_copy_length(src_offset, dest_offset, count, buffer_size)
                {
                    let src = unsafe { base_ptr.add(src_offset) };
                    let dest = unsafe { base_ptr.add(dest_offset) };
                    let _ = unsafe { trycopy::try_copy::<u8>(src, dest, safe_count) };
                }
            }
            TryCopyOp::WriteBytes {
                offset,
                value,
                count,
            } => {
                if offset < buffer_size {
                    let dest = unsafe { base_ptr.add(offset) };
                    let max_count = buffer_size - offset;
                    let safe_count = count.min(max_count);
                    if safe_count > 0 {
                        let _ = unsafe { trycopy::try_write_bytes::<u8>(dest, value, safe_count) };
                    }
                }
            }
            TryCopyOp::ReadVolatileU8 { offset } => {
                if is_aligned_op_safe::<u8>(offset, buffer_size) {
                    let src = unsafe { base_ptr.add(offset).cast::<u8>() };
                    let _ = unsafe { trycopy::try_read_volatile(src) };
                }
            }
            TryCopyOp::ReadVolatileU16 { offset } => {
                if is_aligned_op_safe::<u16>(offset, buffer_size) {
                    let src = unsafe { base_ptr.add(offset).cast::<u16>() };
                    let _ = unsafe { trycopy::try_read_volatile(src) };
                }
            }
            TryCopyOp::ReadVolatileU32 { offset } => {
                if is_aligned_op_safe::<u32>(offset, buffer_size) {
                    let src = unsafe { base_ptr.add(offset).cast::<u32>() };
                    let _ = unsafe { trycopy::try_read_volatile(src) };
                }
            }
            TryCopyOp::ReadVolatileU64 { offset } => {
                if is_aligned_op_safe::<u64>(offset, buffer_size) {
                    let src = unsafe { base_ptr.add(offset).cast::<u64>() };
                    let _ = unsafe { trycopy::try_read_volatile(src) };
                }
            }
            TryCopyOp::WriteVolatileU8 { offset, value } => {
                if is_aligned_op_safe::<u8>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u8>() };
                    let _ = unsafe { trycopy::try_write_volatile(dest, &value) };
                }
            }
            TryCopyOp::WriteVolatileU16 { offset, value } => {
                if is_aligned_op_safe::<u16>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u16>() };
                    let _ = unsafe { trycopy::try_write_volatile(dest, &value) };
                }
            }
            TryCopyOp::WriteVolatileU32 { offset, value } => {
                if is_aligned_op_safe::<u32>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u32>() };
                    let _ = unsafe { trycopy::try_write_volatile(dest, &value) };
                }
            }
            TryCopyOp::WriteVolatileU64 { offset, value } => {
                if is_aligned_op_safe::<u64>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u64>() };
                    let _ = unsafe { trycopy::try_write_volatile(dest, &value) };
                }
            }
            TryCopyOp::CompareExchangeU8 {
                offset,
                current,
                new,
            } => {
                if is_aligned_op_safe::<u8>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u8>() };
                    let _ = unsafe { trycopy::try_compare_exchange(dest, current, new) };
                }
            }
            TryCopyOp::CompareExchangeU16 {
                offset,
                current,
                new,
            } => {
                if is_aligned_op_safe::<u16>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u16>() };
                    let _ = unsafe { trycopy::try_compare_exchange(dest, current, new) };
                }
            }
            TryCopyOp::CompareExchangeU32 {
                offset,
                current,
                new,
            } => {
                if is_aligned_op_safe::<u32>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u32>() };
                    let _ = unsafe { trycopy::try_compare_exchange(dest, current, new) };
                }
            }
            TryCopyOp::CompareExchangeU64 {
                offset,
                current,
                new,
            } => {
                if is_aligned_op_safe::<u64>(offset, buffer_size) {
                    let dest = unsafe { base_ptr.add(offset).cast::<u64>() };
                    let _ = unsafe { trycopy::try_compare_exchange(dest, current, new) };
                }
            }
        }
    }

    fuzz_eprintln!("Completed all operations successfully");
}

fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
