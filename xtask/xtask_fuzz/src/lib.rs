// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers when writing fuzzing crates that get invoked via `cargo xtask fuzz`.
//!
//! Might end up getting deprecated, if/when
//! <https://github.com/rust-fuzz/cargo-fuzz/issues/346> is resolved

use std::sync::OnceLock;

static IS_REPRO: OnceLock<bool> = OnceLock::new();

/// Check if the `XTASK_FUZZ_REPRO` env var was set.
///
/// Caches result in a global static, to avoid redundant lookups.
pub fn is_repro() -> bool {
    *IS_REPRO.get_or_init(|| std::env::var("XTASK_FUZZ_REPRO").is_ok())
}

/// Initialize tracing if this is a repro run.
pub fn init_tracing_if_repro() {
    use std::sync::Once;
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // cargo-fuzz can call our fuzz target multiple times, but we can only initialize tracing once.
    static INIT: Once = Once::new();

    if is_repro() {
        INIT.call_once(|| {
            let targets = if let Ok(var) = std::env::var("OPENVMM_LOG") {
                var.parse().unwrap()
            } else {
                Targets::new().with_default(LevelFilter::TRACE)
            };

            tracing_subscriber::fmt()
                .compact()
                .log_internal_errors(true)
                .with_max_level(LevelFilter::TRACE)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .finish()
                .with(targets)
                .init();
        });
    }
}

/// `eprintln!` that only gets calls when reproducing fuzz test cases locally
#[macro_export]
macro_rules! fuzz_eprintln {
    ($($arg:tt)*) => {
        if $crate::is_repro() {
            eprintln!($($arg)*)
        }
    };
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub use libfuzzer_sys::fuzz_target;

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
/// Fake version of [`libfuzzer_sys::fuzz_target`] for non-linux-gnu targets.
#[macro_export]
macro_rules! fuzz_target {
    ($($tt:tt)*) => {
        // libfuzzer-sys is only supported on Linux gnu, so add a main function
        // that references do_fuzz to satisfy rust-analyzer.
        fn main() {
            let _ = do_fuzz;
        }
    };
}
