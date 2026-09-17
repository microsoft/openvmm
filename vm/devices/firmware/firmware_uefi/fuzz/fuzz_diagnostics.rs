// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![no_main]
#![expect(missing_docs)]
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use arbitrary::Arbitrary;
use firmware_uefi::service::diagnostics::DiagnosticsServices;
use firmware_uefi_resources::LogLevel;
use guestmem::GuestMemory;
use xtask_fuzz::fuzz_target;

#[derive(Debug, Arbitrary)]
enum FuzzLogLevel {
    Default,
    Info,
    Full,
}

#[derive(Debug, Arbitrary)]
struct DiagnosticsInput {
    /// GPA offset for the diagnostics buffer within the memory
    gpa_offset: u32,
    /// The guest memory contents (filled with arbitrary data)
    memory_contents: Vec<u8>,
    /// Whether to allow reprocessing
    allow_reprocess: bool,
    /// Log level to use
    log_level: FuzzLogLevel,
}

fn do_fuzz(input: DiagnosticsInput) -> arbitrary::Result<()> {
    if input.memory_contents.is_empty() {
        // An empty memory region exercises no diagnostics code, so reject
        // these inputs to keep them out of the corpus.
        return Err(arbitrary::Error::IncorrectFormat);
    }

    // Create guest memory and fill it with fuzzed data
    let gm = GuestMemory::allocate(input.memory_contents.len());
    let _ = gm.write_at(0, &input.memory_contents);

    // Pick a GPA somewhere within the buffer to test various offsets
    let buffer_gpa = (input.gpa_offset as usize % input.memory_contents.len()) as u32;

    // Select log level based on fuzzed input to exercise filtering logic
    let log_level = match input.log_level {
        FuzzLogLevel::Default => LogLevel::make_default(),
        FuzzLogLevel::Info => LogLevel::make_info(),
        FuzzLogLevel::Full => LogLevel::make_full(),
    };

    // Create diagnostics service with the selected log level
    let mut diagnostics = DiagnosticsServices::new(log_level);

    // Set GPA to point somewhere in our fuzzed buffer
    diagnostics.set_gpa(buffer_gpa);
    let _ = diagnostics.process_diagnostics(input.allow_reprocess, &gm, None, |_log| {
        // Log handler - just discard logs during fuzzing
    });

    Ok(())
}

fuzz_target!(|input: DiagnosticsInput| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(input).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
