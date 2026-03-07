// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Formatter for use with tracing crate.

use std::fmt;
use std::io;
use std::io::Write;
use tracing::field::Visit;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Writer;

struct FieldFormatterVisitor<'a> {
    writer: Writer<'a>,
    is_empty: bool,
    result: fmt::Result,
}

impl FieldFormatterVisitor<'_> {
    fn maybe_pad(&mut self) {
        if self.is_empty {
            self.is_empty = false;
        } else {
            self.result = write!(self.writer, " ");
        }
    }

    fn record_display(&mut self, field: &tracing::field::Field, value: &dyn fmt::Display) {
        if self.result.is_err() {
            return;
        }

        self.maybe_pad();
        self.result = match field.name() {
            "message" => write!(self.writer, "{}", value),
            // Skip fields that are actually log metadata that have already been handled
            name if name.starts_with("log.") => Ok(()),
            name if name.starts_with("r#") => write!(self.writer, "{}={}", &name[2..], value),
            name => write!(self.writer, "{}={}", name, value),
        };
    }
}

impl Visit for FieldFormatterVisitor<'_> {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.record_display(field, &value)
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_display(field, &value)
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        // Use hex encoding for better readability for most values.
        self.record_display(field, &format_args!("{:#x}", value))
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_display(field, &value)
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_display(field, &format_args!("{:?}", value))
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        mut value: &(dyn std::error::Error + 'static),
    ) {
        self.record_debug(field, &format_args!("{}", value));
        while let Some(s) = value.source() {
            value = s;
            if self.result.is_err() {
                return;
            }
            self.result = write!(self.writer, ": {}", value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        // Use hex encoding for better readability for most values.
        self.record_display(field, &format_args!("{:x?}", value))
    }
}

/// Field formatter that fixes a few issues with the default formatter:
///
/// 1. Displays the full error source chain, not just the first error.
/// 2. Displays unsigned values as hex instead of decimal, improving readability
///    for values that we tend to log in OpenVMM.
pub struct FieldFormatter;

impl FormatFields<'_> for FieldFormatter {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'_>, fields: R) -> fmt::Result {
        let mut visitor = FieldFormatterVisitor {
            writer,
            is_empty: false,
            result: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

/// A Write implementation that wraps `T` and converts LFs into CRLFs.
pub struct CrlfWriter<T> {
    inner: T,
    write_lf: bool,
}

impl<T: Write> CrlfWriter<T> {
    /// Creates a new writer around `t`.
    pub fn new(t: T) -> Self {
        CrlfWriter {
            inner: t,
            write_lf: false,
        }
    }

    fn flush_lf(&mut self) -> io::Result<()> {
        if self.write_lf {
            if self.inner.write(b"\n")? == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.write_lf = false;
        }
        Ok(())
    }
}

impl<T: Write> Write for CrlfWriter<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.flush_lf()?;
        if buf.first() == Some(&b'\n') {
            return Ok(match self.inner.write(b"\r\n")? {
                0 => 0,
                1 => {
                    self.write_lf = true;
                    1
                }
                _ => 1,
            });
        }

        let len = buf.iter().position(|x| *x == b'\n').unwrap_or(buf.len());
        self.inner.write(&buf[..len])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_lf()?;
        self.inner.flush()
    }
}
