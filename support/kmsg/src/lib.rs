// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for parsing /dev/kmsg
//!
//! See <https://www.kernel.org/doc/Documentation/ABI/testing/dev-kmsg> for
//! documentation on the kmsg entry format.

#![expect(missing_docs)]
use std::fmt::Display;
use std::time::Duration;
use thiserror::Error;

/// A parsed kmsg entry.
pub struct KmsgParsedEntry<'a> {
    /// The facility.
    pub facility: u8,
    /// The message level.
    pub level: u8,
    /// The sequence number.
    pub seq: u64,
    /// The time of the message since boot.
    pub time: Duration,
    /// The encoded message.
    pub message: EncodedMessage<'a>,
}

/// An encoded message.
pub struct EncodedMessage<'a>(&'a str);

impl EncodedMessage<'_> {
    /// The raw encoded string.
    pub fn as_raw(&self) -> &str {
        self.0
    }
}

impl Display for EncodedMessage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = self.0.split('\n').next().unwrap();
        let mut last = 0;
        for (i, _) in message.match_indices('\\') {
            write!(f, "{}", &message[last..i])?;
            if message.as_bytes().get(i + 1) == Some(&b'\\') {
                write!(f, "\\")?;
                last = i + 2;
            } else if let Some([b'x', escape @ ..]) = message.get(i + 1..i + 4).map(str::as_bytes) {
                // Allow escaped ESC for ANSI color.
                match escape {
                    b"1b" => write!(f, "\x1b")?,
                    _ => write!(f, "<{}>", &message[i + 2..i + 4])?,
                }
                last = i + 4;
            } else {
                last = i + 1;
            }
        }
        write!(f, "{}", &message[last..])
    }
}

/// An error indicating the kmsg entry could not be parsed because it is invalid.
#[derive(Debug, Error)]
#[error("invalid kmsg entry")]
pub struct InvalidKmsgEntry;

impl<'a> KmsgParsedEntry<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self, InvalidKmsgEntry> {
        Self::new_inner(data).ok_or(InvalidKmsgEntry)
    }

    fn new_inner(data: &'a [u8]) -> Option<Self> {
        let s = std::str::from_utf8(data).ok()?;
        let (kvs, message) = s.split_once(';')?;
        let mut kvs = kvs.split(',');
        let n: u32 = kvs.next()?.parse().ok()?;
        let level = (n & 7) as u8;
        let facility = (n >> 3) as u8;
        let seq = kvs.next()?.parse().ok()?;
        let time = Duration::from_micros(kvs.next()?.parse().ok()?);

        Some(Self {
            facility,
            level,
            seq,
            time,
            message: EncodedMessage(message),
        })
    }

    /// Returns a [`Display`] implementation that includes colors if `ansi`.
    pub fn display(&self, ansi: bool) -> KmsgEntryDisplay<'_> {
        KmsgEntryDisplay { ansi, entry: self }
    }
}

/// [`Display`] implementation for [`SyslogParsedEntry`].
pub struct KmsgEntryDisplay<'a> {
    ansi: bool,
    entry: &'a KmsgParsedEntry<'a>,
}

impl Display for KmsgEntryDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_entry(
            f,
            self.ansi,
            self.entry.level,
            self.entry.time,
            &self.entry.message,
        )
    }
}

fn fmt_entry(
    f: &mut std::fmt::Formatter<'_>,
    ansi: bool,
    level: u8,
    time: Duration,
    message: &impl Display,
) -> std::fmt::Result {
    let time_sec = time.as_secs();
    let time_usec = time.subsec_micros();

    if !ansi {
        return write!(f, "[{time_sec}.{time_usec:06}] {message}");
    }

    let red = "\x1b[0;31m";
    let green = "\x1b[0;32m";
    let yellow = "\x1b[0;33m";
    let reset = "\x1b[0m";

    write!(f, "{green}[{time_sec}.{time_usec:06}] ")?;

    let message = message.to_string();
    let mut message = message.as_str();
    let mut target = None;
    if let Some((s, rest)) = message.split_once(' ') {
        if let Some(s) = s.strip_suffix(':') {
            target = Some(s);
            message = rest;
        }
    }

    if let Some(target) = target {
        write!(f, "{yellow}{target}: ")?;
    }

    let level_color = match level {
        0..=3 => red,
        4 => yellow,
        5..=7 => reset,
        _ => unreachable!(),
    };

    let message = message.trim();
    write!(f, "{level_color}{message}{reset}")
}

/// A parsed syslog-format entry.
pub struct SyslogParsedEntry<'a> {
    pub _facility: u8,
    pub level: u8,
    pub time: Duration,
    pub message: &'a str,
}

impl<'a> SyslogParsedEntry<'a> {
    /// Parses an entry that looks like: `<n>[   3.593853] target: message`.
    pub fn new(s: &'a str) -> Option<Self> {
        let s = s.strip_prefix('<')?;
        let (n, s) = s.split_once('>')?;
        let n: u32 = n.parse().ok()?;
        let level = (n & 7) as u8;
        let facility = (n >> 3) as u8;
        let s = s.strip_prefix('[')?;
        let (secs, s) = s.trim_start().split_once('.')?;
        let (usecs, s) = s.split_once(']')?;
        let time = Duration::new(secs.parse().ok()?, usecs.parse::<u32>().ok()? * 1000);

        Some(SyslogParsedEntry {
            _facility: facility,
            level,
            time,
            message: s.trim(),
        })
    }

    /// Returns a [`Display`] implementation that includes colors if `ansi`.
    pub fn display(&self, ansi: bool) -> SyslogEntryDisplay<'_> {
        SyslogEntryDisplay { ansi, entry: self }
    }
}

/// [`Display`] implementation for [`SyslogParsedEntry`].
pub struct SyslogEntryDisplay<'a> {
    ansi: bool,
    entry: &'a SyslogParsedEntry<'a>,
}

impl Display for SyslogEntryDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_entry(
            f,
            self.ansi,
            self.entry.level,
            self.entry.time,
            &self.entry.message,
        )
    }
}
