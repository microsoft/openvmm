// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for defining tests that have tracing output.

#![forbid(unsafe_code)]

#[cfg(test)]
extern crate self as test_with_tracing;

pub use test_with_tracing_macro::test;
use tracing::metadata::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

#[doc(hidden)]
/// Initializes `tracing` for tests.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| {
        let targets = if let Ok(var) = std::env::var("RUST_LOG") {
            var.parse().unwrap()
        } else {
            Targets::new().with_default(LevelFilter::DEBUG)
        };
        tracing_subscriber::fmt()
            .pretty()
            .with_ansi(false) // avoid polluting logs with escape sequences
            .log_internal_errors(true)
            .with_test_writer()
            .with_max_level(LevelFilter::TRACE)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .finish()
            .with(targets)
            .init();
    });
}

#[cfg(test)]
mod tests {
    use super::test;

    #[test]
    fn test_it() {
        tracing::info!("should show tracing warning");
        log::info!("should show log warning");
    }

    #[test]
    fn test_with_return() -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("ok");
        Ok(())
    }
}
