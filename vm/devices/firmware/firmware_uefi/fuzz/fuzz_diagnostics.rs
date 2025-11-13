// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![no_main]
#![expect(missing_docs)]
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use arbitrary::Arbitrary;
use firmware_uefi::service::diagnostics::DiagnosticsServices;
use firmware_uefi::service::diagnostics::LogLevel;
use guestmem::GuestMemory;
use xtask_fuzz::fuzz_target;

/// Maximum size of guest memory to allocate during fuzzing.
/// By default, libFuzzer's RSS limit is 2GB
const MAX_GUEST_MEMORY_SIZE: usize = 1024 * 1024 * 1024; // 1GB

#[derive(Debug, Arbitrary)]
struct DiagnosticsInput {
    /// GPA offset for the diagnostics buffer within the memory
    gpa_offset: u32,
    /// The guest memory contents (filled with arbitrary data)
    memory_contents: Vec<u8>,
    /// Whether to allow reprocessing
    allow_reprocess: bool,
    /// Log level variant to use (0=default, 1=info, 2=full)
    log_level_variant: u8,
}

fn do_fuzz(input: DiagnosticsInput) {
    if input.memory_contents.is_empty() {
        return;
    }

    // Limit memory size to avoid OOM
    let mem_size = input.memory_contents.len().min(MAX_GUEST_MEMORY_SIZE);

    // Create guest memory and fill it with fuzzed data
    let gm = GuestMemory::allocate(mem_size);
    let _ = gm.write_at(0, &input.memory_contents[..mem_size]);

    // Use the raw GPA value to test validation logic in set_gpa/Gpa::new
    // This allows testing rejection of invalid values like 0 and u32::MAX
    let buffer_gpa = input.gpa_offset;

    // Select log level based on fuzzed input to exercise filtering logic
    let log_level = match input.log_level_variant % 3 {
        0 => LogLevel::make_default(),
        1 => LogLevel::make_info(),
        _ => LogLevel::make_full(),
    };

    // Create diagnostics service with the selected log level
    let mut diagnostics = DiagnosticsServices::new(log_level);

    // Set GPA - this will internally validate via Gpa::new() and reject invalid values
    diagnostics.set_gpa(buffer_gpa);
    let _ = diagnostics.process_diagnostics(input.allow_reprocess, &gm, |_log| {
        // Log handler - just discard logs during fuzzing
    });
}

fuzz_target!(|input: DiagnosticsInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
