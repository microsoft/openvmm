// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for parsing /dev/kmsg
//!
//! See <https://www.kernel.org/doc/Documentation/ABI/testing/dev-kmsg> for
//! documentation on the kmsg entry format.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

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
#[derive(Copy, Clone, Debug)]
pub struct EncodedMessage<'a>(&'a str);

impl<'a> EncodedMessage<'a> {
    /// Creates a new encoded message from a raw string.
    pub fn new(raw: &'a str) -> Self {
        EncodedMessage(raw)
    }

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
#[error("invalid kmsg entry: {0:?}")]
pub struct InvalidKmsgEntry<'a>(&'a [u8]);

impl<'a> KmsgParsedEntry<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self, InvalidKmsgEntry<'a>> {
        Self::new_inner(data).ok_or(InvalidKmsgEntry(data))
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
        let secs = secs.parse().ok()?;
        let usecs = usecs.parse::<u32>().ok()?;
        if usecs >= 1_000_000 {
            return None;
        }
        let time = Duration::new(secs, usecs * 1000);

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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    // Test: Parse extended kmsg records with continuation flags and metadata.
    // Expected: Priority, 64-bit sequence and microsecond timestamp survive;
    //       display excludes metadata but raw message retains it.
    #[test]
    fn extended_records() {
        let entry = KmsgParsedEntry::new(
            b"188,18446744073709551615,1234567,c,caller=T42;disk: retry; pending\n DEVICE=nvme0\n",
        )
        .unwrap();
        assert_eq!(entry.facility, 23);
        assert_eq!(entry.level, 4);
        assert_eq!(entry.seq, u64::MAX);
        assert_eq!(entry.time, Duration::from_micros(1_234_567));
        assert_eq!(
            entry.message.as_raw(),
            "disk: retry; pending\n DEVICE=nvme0\n"
        );
        assert_eq!(
            entry.display(false).to_string(),
            "[1.234567] disk: retry; pending"
        );
        let empty = KmsgParsedEntry::new(b"0,0,0,-;").unwrap();
        assert_eq!(empty.display(false).to_string(), "[0.000000] ");
    }

    // Test: Decode the printable escapes emitted by the Linux printk producer.
    // Expected: ESC preserves terminal coloring, other encoded bytes become
    //       visible hexadecimal markers, and dictionary lines are omitted.
    #[test]
    fn encoded_kernel_text() {
        let entry =
            KmsgParsedEntry::new(b"6,9,2,-;a\\x1b[31mb\\x0a\\x5c\\xff\n SUBSYSTEM=pci\n").unwrap();
        assert_eq!(entry.message.to_string(), "a\x1b[31mb<0a><5c><ff>");
        assert_eq!(
            entry.display(false).to_string(),
            "[0.000002] a\x1b[31mb<0a><5c><ff>"
        );
    }

    // Test: Reject kmsg headers with missing, nonnumeric or overflowing fields.
    // Expected: Invalid records return an error instead of becoming log events.
    #[test]
    fn invalid_extended_headers() {
        for data in [
            b"".as_slice(),
            b"6,1,2,-",
            b"6;message",
            b"6,1;message",
            b"x,1,2,-;message",
            b"4294967296,1,2,-;message",
            b"6,-1,2,-;message",
            b"6,18446744073709551616,2,-;message",
            b"6,1,18446744073709551616,-;message",
            b"6,1,2,-;\xff",
        ] {
            assert!(KmsgParsedEntry::new(data).is_err(), "{data:?}");
        }
    }

    // Test: Parse serial-console syslog lines at microsecond boundaries.
    // Expected: Facility and severity separate correctly, whitespace is
    //       normalized, and timestamps retain all six fractional digits.
    #[test]
    fn syslog_records() {
        for (input, facility, level, time, expected) in [
            (
                "<13>[   3.593853] init: ready  ",
                1,
                5,
                Duration::from_micros(3_593_853),
                "[3.593853] init: ready",
            ),
            (
                "<191>[0.999999] trace",
                23,
                7,
                Duration::from_micros(999_999),
                "[0.999999] trace",
            ),
            ("<0>[0.000000] ", 0, 0, Duration::ZERO, "[0.000000] "),
        ] {
            let entry = SyslogParsedEntry::new(input).unwrap();
            assert_eq!(entry._facility, facility);
            assert_eq!(entry.level, level);
            assert_eq!(entry.time, time);
            assert_eq!(entry.display(false).to_string(), expected);
        }
    }

    // Test: Parse console lines that are not valid timestamped syslog records.
    // Expected: Malformed records return None for the consumer's plain-text
    //       fallback, including fractional timestamps of a million microseconds.
    #[test]
    fn invalid_syslog_headers() {
        for input in [
            "plain console text",
            "<6[0.000001] text",
            "<x>[0.000001] text",
            "<6>0.000001] text",
            "<6>[0] text",
            "<6>[0.1 text",
            "<6>[x.000001] text",
            "<6>[0.x] text",
            "<6>[0.1000000] text",
            "<6>[18446744073709551616.000000] text",
        ] {
            assert!(SyslogParsedEntry::new(input).is_none(), "{input}");
        }
    }

    // Test: Color parsed events at every severity, with and without a target.
    // Expected: Critical/error red, warning yellow and informational reset
    //       match the event level; each colored line ends with terminal reset.
    #[test]
    fn severity_colors() {
        for level in 0..8 {
            let color = match level {
                0..=3 => "\x1b[0;31m",
                4 => "\x1b[0;33m",
                _ => "\x1b[0m",
            };
            let record = format!("{level},2,1000001,-;driver: message \n");
            let entry = KmsgParsedEntry::new(record.as_bytes()).unwrap();
            assert_eq!(
                entry.display(true).to_string(),
                format!("\x1b[0;32m[1.000001] \x1b[0;33mdriver: {color}message\x1b[0m")
            );
            let record = format!("<{level}>[1.000001] message");
            let entry = SyslogParsedEntry::new(&record).unwrap();
            assert_eq!(
                entry.display(true).to_string(),
                format!("\x1b[0;32m[1.000001] {color}message\x1b[0m")
            );
        }
    }
}
