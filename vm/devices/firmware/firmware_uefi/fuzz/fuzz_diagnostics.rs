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

    // Create guest memory and fill it with fuzzed data
    let gm = GuestMemory::allocate(input.memory_contents.len());
    let _ = gm.write_at(0, &input.memory_contents);

    // Pick a GPA somewhere within the buffer to test various offsets
    let buffer_gpa = (input.gpa_offset as usize % input.memory_contents.len()) as u32;

    // Select log level based on fuzzed input to exercise filtering logic
    let log_level = match input.log_level_variant % 3 {
        0 => LogLevel::make_default(),
        1 => LogLevel::make_info(),
        _ => LogLevel::make_full(),
    };

    // Create diagnostics service with the selected log level
    let mut diagnostics = DiagnosticsServices::new(log_level);

    // Set GPA to point somewhere in our fuzzed buffer
    diagnostics.set_gpa(buffer_gpa);
    let _ = diagnostics.process_diagnostics(input.allow_reprocess, &gm, None, |_log| {
        // Log handler - just discard logs during fuzzing
    });
}

fuzz_target!(|input: DiagnosticsInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
