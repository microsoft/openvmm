// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]
// UNSAFETY: fuzzing unsafe memory operations
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

use arbitrary::Arbitrary;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

const MAX_BUFFER_SIZE: usize = 8192;

#[derive(Debug, Arbitrary)]
enum MemcpyOp {
    Memcpy {
        src_offset: usize,
        dest_offset: usize,
        len: usize,
    },
    Memmove {
        src_offset: usize,
        dest_offset: usize,
        len: usize,
    },
}

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    buffer_size: u16, // Keep it small for performance
    initial_pattern: u8,
    operations: Vec<MemcpyOp>,
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
    if safe_len > 0 {
        Some(safe_len)
    } else {
        None
    }
}

fn do_fuzz(input: FuzzInput) {
    // Create buffers to work with - limit size to avoid OOM
    let buffer_size = (input.buffer_size as usize).min(MAX_BUFFER_SIZE).max(16);

    // Initialize buffers with a pattern
    let src_buffer = vec![input.initial_pattern; buffer_size];
    let mut dest_buffer = vec![!input.initial_pattern; buffer_size];

    // Also create a reference buffer for memmove testing
    let mut reference_buffer = vec![0u8; buffer_size];

    fuzz_eprintln!("Testing with buffer size: {}", buffer_size);

    // Execute all operations
    for op in input.operations {
        match op {
            MemcpyOp::Memcpy {
                src_offset,
                dest_offset,
                len,
            } => {
                if let Some(max_len) = safe_copy_length(src_offset, dest_offset, len, buffer_size)
                {
                    // Test memcpy with non-overlapping buffers
                    unsafe {
                        fast_memcpy::memcpy(
                            dest_buffer.as_mut_ptr().add(dest_offset),
                            src_buffer.as_ptr().add(src_offset),
                            max_len,
                        );
                    }

                    // Verify the copy worked correctly
                    assert_eq!(
                        &dest_buffer[dest_offset..dest_offset + max_len],
                        &src_buffer[src_offset..src_offset + max_len],
                        "memcpy failed: dest and src differ"
                    );
                }
            }
            MemcpyOp::Memmove {
                src_offset,
                dest_offset,
                len,
            } => {
                if let Some(max_len) = safe_copy_length(src_offset, dest_offset, len, buffer_size)
                {
                    // Reset reference buffer to current state
                    reference_buffer.copy_from_slice(&dest_buffer);

                    // Test memmove with potentially overlapping regions
                    unsafe {
                        fast_memcpy::memmove(
                            dest_buffer.as_mut_ptr().add(dest_offset),
                            dest_buffer.as_ptr().add(src_offset),
                            max_len,
                        );
                    }

                    // Use reference buffer to verify the operation
                    // by simulating the expected behavior
                    reference_buffer.copy_within(src_offset..src_offset + max_len, dest_offset);

                    assert_eq!(
                        dest_buffer, reference_buffer,
                        "memmove failed: result doesn't match expected behavior"
                    );
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
