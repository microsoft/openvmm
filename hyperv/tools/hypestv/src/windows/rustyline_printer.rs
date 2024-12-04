// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a `Write` adapter for `rustyline::ExternalPrinter`. This allows
//! writing to the console while there is an active rustyline prompt, without it
//! overwriting the prompt.
//!
//! Ideally `rustyline` would just provide this facility natively.

use parking_lot::Mutex;
use std::io::LineWriter;
use std::io::Write;
use std::sync::Arc;

#[derive(Clone)]
pub struct Printer(Arc<Mutex<LineWriter<PrinterInner>>>);

pub struct PrinterWriter<'a>(
    parking_lot::lock_api::MutexGuard<'a, parking_lot::RawMutex, LineWriter<PrinterInner>>,
);

impl PrinterWriter<'_> {
    pub fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        self.0.write_fmt(args)
    }
}

pub struct PrinterInner<T: ?Sized = dyn rustyline::ExternalPrinter + Send>(T);

impl Printer {
    /// Makes a new printer.
    pub fn new(printer: impl rustyline::ExternalPrinter + Send + 'static) -> Self {
        // Use a `LineWriter` internally because each printer `print` call will
        // write a full line. This way, as long as no line gets too long and no
        // one calls `flush` (which is not exposed), we will always write at
        // least one full line to the printer at a time.
        Self(Arc::new(Mutex::new(LineWriter::new(PrinterInner(printer)))))
    }

    /// Gets the writer. Internally, this takes a lock to ensure that the line
    /// is not split across multiple threads, so don't hold it for a long time
    /// or across `await`s.
    pub fn out(&self) -> PrinterWriter<'_> {
        PrinterWriter(self.0.lock())
    }
}

impl<T: rustyline::ExternalPrinter + ?Sized> Write for PrinterInner<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let s = String::from_utf8_lossy(buf).into_owned();
        self.0
            .print(s)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
