// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parses /dev/kmsg and produces a stream of trace logging notifications to
//! send to the host.

use super::json_common::KmsgMessage;
use cvm_tracing::CVM_ALLOWED;
use futures::AsyncRead;
use futures::Stream;
use get_helpers::build_tracelogging_notification_buffer;
use get_protocol::LogFlags;
use get_protocol::LogLevel;
use get_protocol::LogType;
use get_protocol::TRACE_LOGGING_MESSAGE_MAX_SIZE;
use kmsg::KmsgParsedEntry;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use std::io::ErrorKind;
use std::io::Write;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use tracing::Level;

/// A stream of trace logging notifications from `/dev/kmsg`.
pub struct KmsgStream {
    pipe: PolledPipe,
    next_seq: u64,
}

impl KmsgStream {
    /// Opens `/dev/kmsg`.
    pub fn new(driver: &(impl Driver + ?Sized)) -> std::io::Result<Self> {
        let kmsg = fs_err::File::open("/dev/kmsg")?.into();

        let kmsg_stream = KmsgStream {
            pipe: PolledPipe::new(driver, kmsg)?,
            next_seq: 0,
        };
        Ok(kmsg_stream)
    }
}

#[derive(serde::Serialize)]
struct Fields<'a> {
    #[serde(with = "serde_helpers::as_string")]
    message: kmsg::EncodedMessage<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    missed_entries: Option<NonZeroU64>,
}

struct SaturatingWriter<'a>(&'a mut [u8]);

impl Write for SaturatingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = buf.len().min(self.0.len());
        let (this, rest) = std::mem::take(&mut self.0).split_at_mut(len);
        this.copy_from_slice(&buf[..len]);
        self.0 = rest;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

macro_rules! kmsg_enabled {
    ($target:expr, $level:expr) => {
        match $level {
            kmsg_defs::LOGLEVEL_EMERG..=kmsg_defs::LOGLEVEL_ERR => {
                tracing::enabled!(target: $target, Level::ERROR)
            }
            kmsg_defs::LOGLEVEL_WARNING => tracing::enabled!(target: $target, Level::WARN),
            kmsg_defs::LOGLEVEL_NOTICE => tracing::enabled!(target: $target, Level::INFO),
            kmsg_defs::LOGLEVEL_INFO => tracing::enabled!(target: $target, Level::DEBUG),
            kmsg_defs::LOGLEVEL_DEBUG.. => tracing::enabled!(target: $target, Level::TRACE),
        }
    };
}

impl Stream for KmsgStream {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        const KMSG_ENTRY_MAX_SIZE: usize = 2048;
        let mut buf = [0; KMSG_ENTRY_MAX_SIZE];
        let item = loop {
            match ready!(Pin::new(&mut self.pipe).poll_read(cx, &mut buf)) {
                Ok(n) => {
                    let entry = KmsgParsedEntry::new(&buf[..n]).unwrap();
                    let missed_entries = NonZeroU64::new(entry.seq - self.next_seq);
                    self.next_seq = entry.seq + 1;
                    let target = match entry.facility {
                        kmsg_defs::UNDERHILL_KMSG_FACILITY => {
                            // Don't re-log messages from Underhill itself.
                            continue;
                        }
                        kmsg_defs::UNDERHILL_INIT_KMSG_FACILITY => {
                            if !kmsg_enabled!("underhill_init", entry.level) {
                                continue;
                            }
                            "underhill_init"
                        }
                        _ => {
                            if !kmsg_enabled!("kmsg", entry.level) {
                                continue;
                            }
                            "kmsg"
                        }
                    };

                    let level = match entry.level {
                        kmsg_defs::LOGLEVEL_EMERG..=kmsg_defs::LOGLEVEL_CRIT => LogLevel::CRITICAL,
                        kmsg_defs::LOGLEVEL_ERR => LogLevel::ERROR,
                        kmsg_defs::LOGLEVEL_WARNING => LogLevel::WARNING,
                        kmsg_defs::LOGLEVEL_NOTICE => LogLevel::INFORMATION,
                        kmsg_defs::LOGLEVEL_INFO.. => LogLevel::VERBOSE,
                    };

                    let mut message = [0; TRACE_LOGGING_MESSAGE_MAX_SIZE];
                    let mut writer = SaturatingWriter(&mut message);
                    serde_json::to_writer(
                        &mut writer,
                        &KmsgMessage {
                            timestamp: entry.time,
                            level: entry.level,
                            target,
                            fields: Fields {
                                message: entry.message,
                                missed_entries,
                            },
                        },
                    )
                    .unwrap();

                    let remaining = writer.0.len();
                    let message_len = message.len() - remaining;

                    let notification = build_tracelogging_notification_buffer(
                        LogType::EVENT,
                        level,
                        LogFlags::new().with_kmsg(true),
                        None,
                        None,
                        None,
                        None,
                        Some(target.as_bytes()),
                        None,
                        &message[..message_len],
                        (entry.time.as_nanos() / 100) as u64,
                    );

                    break notification;
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    // An event was dropped. This will be reported on the next
                    // event.
                }
                Err(err) => {
                    tracing::error!(
                        CVM_ALLOWED,
                        error = &err as &dyn std::error::Error,
                        "failed to read from /dev/kmsg"
                    );
                    return Poll::Ready(None);
                }
            }
        };
        Poll::Ready(Some(item))
    }
}
