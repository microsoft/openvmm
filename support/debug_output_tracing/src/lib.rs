// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows debug output tracing infrastructure.
//!
//! Provides reusable components for routing `tracing` events to the Windows
//! debug output stream (`OutputDebugStringA`), with a per-layer filter that
//! avoids formatting and allocating when no debugger is attached.
//!
//! This crate is **Windows-only** (`#![cfg(windows)]`).
//!
//! # Components
//!
//! * `DebugOutputWriter` — an [`std::io::Write`] implementation that forwards
//!   bytes to `OutputDebugStringA`.  Use with
//!   `tracing_subscriber::fmt::layer().with_writer()`.
//! * `DebuggerPresentFilter` — a per-layer `tracing_subscriber::layer::Filter`
//!   that gates on both the configured level **and** `IsDebuggerPresent()`.
//!   Attach this to a layer via `.with_filter(...)` to completely avoid
//!   formatting overhead when no debugger is listening.
//!
//! # Why this matters
//!
//! `OutputDebugStringA` is essentially a no-op when no debugger is attached,
//! but the `tracing` subscriber still formats every event into a heap-allocated
//! `String` before passing it to the writer.  At verbose levels (DEBUG / TRACE)
//! this causes significant CPU and allocation overhead even in production.
//! `DebuggerPresentFilter` prevents the layer's `on_event` from being called
//! at all, eliminating the wasted work.
//!
//! # Example
//!
//! ```ignore
//! use debug_output_tracing::{DebugOutputWriter, DebuggerPresentFilter};
//! use tracing_subscriber::Layer as _;
//! use tracing_subscriber::layer::SubscriberExt;
//! use tracing_subscriber::util::SubscriberInitExt;
//!
//! let fmt_layer = tracing_subscriber::fmt::layer()
//!     .with_ansi(false)
//!     .with_writer(DebugOutputWriter::new)
//!     .with_filter(DebuggerPresentFilter::new(
//!         tracing::metadata::LevelFilter::DEBUG,
//!     ));
//!
//! tracing_subscriber::Registry::default()
//!     .with(fmt_layer)
//!     .try_init()
//!     .ok();
//! ```

#![cfg(windows)]
// UNSAFETY: Calling Win32 `OutputDebugStringA` and `IsDebuggerPresent`.
#![expect(unsafe_code)]

use smallvec::SmallVec;
use tracing::Subscriber;
use tracing::metadata::LevelFilter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::Filter;
use windows_sys::Win32::System::Diagnostics::Debug::{IsDebuggerPresent, OutputDebugStringA};

// ---------------------------------------------------------------------------
// DebugOutputWriter
// ---------------------------------------------------------------------------

/// An [`std::io::Write`] implementation that sends bytes to
/// `OutputDebugStringA`.
///
/// Suitable for use with `tracing_subscriber::fmt::layer().with_writer()`.
pub struct DebugOutputWriter {
    _private: (),
}

impl DebugOutputWriter {
    /// Create a new writer.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl std::io::Write for DebugOutputWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Use a SmallVec so typical messages stay on the stack and only
        // spill to the heap for oversized output.
        let mut null_terminated: SmallVec<[u8; 1024]> = SmallVec::with_capacity(buf.len() + 1);
        null_terminated.extend_from_slice(buf);
        null_terminated.push(b'\0');
        // SAFETY: The buffer is null-terminated and valid.
        unsafe { OutputDebugStringA(null_terminated.as_ptr()) };
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DebuggerPresentFilter
// ---------------------------------------------------------------------------

/// Per-layer filter that gates on both a level threshold **and**
/// `IsDebuggerPresent()`.
///
/// When no debugger is attached the filter returns `false` for every event,
/// which means the associated layer's `on_event` is never called and no
/// formatting / allocation takes place.
pub struct DebuggerPresentFilter(LevelFilter);

impl DebuggerPresentFilter {
    /// Create a new filter with the given maximum level.
    pub fn new(max_level: LevelFilter) -> Self {
        Self(max_level)
    }
}

impl<S: Subscriber> Filter<S> for DebuggerPresentFilter {
    fn enabled(&self, meta: &tracing::Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        // SAFETY: `IsDebuggerPresent` is a trivial Win32 function with no
        // preconditions.
        *meta.level() <= self.0 && unsafe { IsDebuggerPresent() != 0 }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(self.0)
    }
}
