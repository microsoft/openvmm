// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A [`log::Log`] implementation that translates log-levels to ADO logging
//! commands when running in CI, and uses ANSI colors otherwise.
//!
//! Inlined from `ci_logger` to avoid a cross-workspace path dependency.

use log::Level;
use log::LevelFilter;
use log::Metadata;
use log::Record;

struct AdoLogger {
    max_level: LevelFilter,
    in_ci: bool,
}

impl AdoLogger {
    fn new(log_level: Option<&str>) -> AdoLogger {
        let max_level = match log_level.map(|s| s.to_lowercase()).as_deref() {
            Some("trace") => LevelFilter::Trace,
            Some("debug") => LevelFilter::Debug,
            Some("info") => LevelFilter::Info,
            Some("warn" | "warning") => LevelFilter::Warn,
            Some("error") => LevelFilter::Error,
            Some("off") => LevelFilter::Off,
            _ => LevelFilter::Info,
        };

        AdoLogger {
            in_ci: std::env::var("TF_BUILD").is_ok(),
            max_level,
        }
    }
}

impl log::Log for AdoLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.max_level
    }

    fn log(&self, record: &Record<'_>) {
        if record.level() <= self.max_level {
            let (prefix, postfix) = if self.in_ci {
                let prefix = match record.level() {
                    Level::Error => "##vso[task.logissue type=error]",
                    Level::Warn => "##vso[task.logissue type=warning]",
                    Level::Info => "",
                    Level::Debug => "##[debug]",
                    Level::Trace => "##[debug](trace)",
                };
                (prefix, "")
            } else {
                let prefix = match record.level() {
                    Level::Error => "\x1B[0;31m", // red
                    Level::Warn => "\x1B[0;33m",  // yellow
                    Level::Info => "",
                    Level::Debug => "\x1B[0;36m", // cyan
                    Level::Trace => "\x1B[0;35m", // purple
                };
                (prefix, "\x1B[0m")
            };

            if record.level() <= Level::Info {
                eprintln!("{}{}{}", prefix, record.args(), postfix)
            } else {
                eprintln!(
                    "{}[{}:{}] {}{}",
                    prefix,
                    record.module_path().unwrap_or("?"),
                    record
                        .line()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".into()),
                    record.args(),
                    postfix
                )
            }
        }
    }

    fn flush(&self) {}
}

/// Initialize the ADO logger
pub fn init(log_env_var: &str) -> Result<(), log::SetLoggerError> {
    log::set_boxed_logger(Box::new(AdoLogger::new(
        std::env::var(log_env_var).ok().as_deref(),
    )))
    .map(|()| log::set_max_level(LevelFilter::Trace))
}
