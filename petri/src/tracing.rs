// Copyright (C) Microsoft Corporation. All rights reserved.

use std::fs::File;
use std::path::Path;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::writer::EitherWriter;
use tracing_subscriber::fmt::writer::Tee;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::TestWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub(crate) const LINUX_TARGET: &str = "linux_log";
pub(crate) const PCAT_TARGET: &str = "pcat_log";
pub(crate) const UEFI_TARGET: &str = "uefi_log";
pub(crate) const OPENHCL_TARGET: &str = "openhcl_log";
pub(crate) const HVLITE_TARGET: &str = "hvlite_log";

pub(crate) fn try_init_tracing(
    log_file: File,
) -> Result<(), tracing_subscriber::util::TryInitError> {
    let targets =
        if let Ok(var) = std::env::var("OPENVMM_LOG").or_else(|_| std::env::var("HVLITE_LOG")) {
            var.parse().unwrap()
        } else {
            Targets::new().with_default(LevelFilter::DEBUG)
        };
    tracing_subscriber::fmt()
        .compact()
        .with_ansi(false) // avoid polluting logs with escape sequences
        .log_internal_errors(true)
        .with_writer(PetriWriter::new(log_file))
        .with_max_level(LevelFilter::TRACE)
        .finish()
        .with(targets)
        .try_init()
}

struct PetriWriter {
    log_file: File,
}

impl PetriWriter {
    fn new(log_file: File) -> Self {
        Self { log_file }
    }
}

impl<'a> MakeWriter<'a> for PetriWriter {
    type Writer = EitherWriter<TestWriter, Tee<TestWriter, &'a File>>;

    fn make_writer(&'a self) -> Self::Writer {
        // When unknown err on the side of logging too much.
        EitherWriter::B(Tee::new(TestWriter::new(), &self.log_file))
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        if [
            LINUX_TARGET,
            PCAT_TARGET,
            UEFI_TARGET,
            OPENHCL_TARGET,
            HVLITE_TARGET,
        ]
        .contains(&meta.target())
        {
            EitherWriter::A(TestWriter::new())
        } else {
            EitherWriter::B(Tee::new(TestWriter::new(), &self.log_file))
        }
    }
}

/// Report a file as an attachment to the currently running test. This ensures
/// that the file makes it into the test results.
pub fn trace_attachment(path: impl AsRef<Path>) {
    fn trace(path: &Path) {
        // ATTACHMENT is most reliable when using true canonicalized paths
        #[allow(clippy::disallowed_methods)]
        match path.canonicalize() {
            Ok(path) => {
                // Use the inline junit syntax to attach the file to the test
                // result.
                println!("[[ATTACHMENT|{}]]", path.display());
            }
            Err(err) => {
                tracing::error!(
                    path = %path.display(),
                    error = &err as &dyn std::error::Error,
                    "failed to canonicalize attachment path"
                );
            }
        }
    }
    trace(path.as_ref());
}
