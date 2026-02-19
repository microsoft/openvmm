// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory profile tracing helpers for OpenHCL.

#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

/// Wrapper around `dhat::Profiler` lifecycle used by OpenHCL memory profile tracing.
pub struct HeapProfiler {
    profiler: dhat::Profiler,
}

impl HeapProfiler {
    /// Starts a heap profiler session.
    pub fn new() -> Self {
        Self {
            profiler: dhat::Profiler::new_heap(),
        }
    }

    /// Captures the current memory profile and immediately starts a new session.
    pub fn capture_and_restart(&mut self) -> Vec<u8> {
        let summary = self.profiler.drop_and_get_memory_output();

        // `drop_and_get_memory_output` transitions DHAT global state to Ready.
        // Replacing with a fresh profiler starts a new Running session, and we must
        // not run `Drop` on the old profiler (it would interact with the new state).
        let old = std::mem::replace(&mut self.profiler, dhat::Profiler::new_heap());
        std::mem::forget(old);

        summary.into()
    }
}
