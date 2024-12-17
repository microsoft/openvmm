// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A [`log::Log`] implementation translates log-levels to ADO logging commands.

use env_logger::filter::Filter;
use log::Level;
use log::Metadata;
use log::Record;

struct AdoLogger {
    filter: Filter,
    in_ci: bool,
}

impl AdoLogger {
    fn new(log_env_var: &str) -> AdoLogger {
        let mut builder = env_logger::filter::Builder::new();
        if let Ok(var) = std::env::var(log_env_var) {
            builder.parse(&var);
        } else {
            builder.filter_level(log::LevelFilter::Info);
        }
        let filter = builder.build();

        AdoLogger {
            in_ci: std::env::var("TF_BUILD").is_ok(),
            filter,
        }
    }

    fn new_value(log_level: &str) -> AdoLogger {
        let filter = env_logger::filter::Builder::new().parse(log_level).build();

        AdoLogger {
            in_ci: std::env::var("TF_BUILD").is_ok(),
            filter,
        }
    }
}

impl log::Log for AdoLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if self.filter.matches(record) {
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
    log::set_boxed_logger(Box::new(AdoLogger::new(log_env_var)))
        .map(|()| log::set_max_level(log::LevelFilter::Trace))
}

/// Initialize the ADO logger with a specific value, instead of an env var.
pub fn init_value(log_level: &str) -> Result<(), log::SetLoggerError> {
    log::set_boxed_logger(Box::new(AdoLogger::new_value(log_level)))
        .map(|()| log::set_max_level(log::LevelFilter::Trace))
}
