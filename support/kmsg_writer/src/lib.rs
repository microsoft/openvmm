// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a type for writing tracing events to `/dev/kmsg`.

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::IoSlice;
use std::io::Write;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

/// Implements [`MakeWriter`] on top of `/dev/kmsg`.
pub struct KmsgWriter {
    file: File,
    facility: u8,
}

impl KmsgWriter {
    /// Opens `/dev/kmsg`.
    pub fn new(facility: u8) -> std::io::Result<Self> {
        let file = fs_err::OpenOptions::new()
            .write(true)
            .open("/dev/kmsg")?
            .into();

        Ok(Self { facility, file })
    }

    fn make_prefix(&self, level: Level, target: &str) -> String {
        let level = match level {
            Level::ERROR => 3,
            Level::WARN => 4,
            Level::INFO => 6,
            Level::DEBUG | Level::TRACE => 7,
        };

        let n = (level as u8) | (self.facility << 3);
        format!("<{}>{}: ", n, target)
    }
}

/// A writer that writes to `/dev/kmsg` with a prefix.
pub struct KmsgWithPrefix<'a> {
    kmsg: &'a File,
    prefix: String,
}

impl<'a> MakeWriter<'a> for KmsgWriter {
    type Writer = KmsgWithPrefix<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        KmsgWithPrefix {
            kmsg: &self.file,
            prefix: self.make_prefix(Level::INFO, "underhill"),
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        KmsgWithPrefix {
            kmsg: &self.file,
            prefix: self.make_prefix(*meta.level(), meta.target()),
        }
    }
}

impl Write for KmsgWithPrefix<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // The maximum length for a kmsg write is either 976 or 992 bytes
        // depending on kernel configuration. Lets be safe.
        let max_buf_len = 976 - self.prefix.len();

        let buf_end = std::cmp::min(buf.len(), max_buf_len);
        let bytes_written = self.kmsg.write_vectored(&[
            IoSlice::new(self.prefix.as_bytes()),
            IoSlice::new(&buf[..buf_end]),
        ])?;

        Ok(bytes_written - self.prefix.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
