// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers to build GET (Guest Emulation Transport) protocol payloads.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use get_protocol::LogFlags;
use get_protocol::LogLevel;
use get_protocol::LogType;
use get_protocol::TraceLoggingBufferOffset;
use get_protocol::TraceLoggingNotificationHeader;
use get_protocol::TRACE_LOGGING_FIELDS_MAX_SIZE;
use get_protocol::TRACE_LOGGING_MESSAGE_MAX_SIZE;
use get_protocol::TRACE_LOGGING_NAME_MAX_SIZE;
use get_protocol::TRACE_LOGGING_TARGET_MAX_SIZE;
use guid::Guid;
use zerocopy::IntoBytes;

/// Truncates the specified slice by the specified length.
fn truncate_slice(input: &[u8], len: usize) -> &[u8] {
    if input.len() <= len {
        input
    } else {
        &input[..len]
    }
}

/// Helper function to build the tracelogging buffer with the specified fields
pub fn build_tracelogging_notification_buffer(
    log_type: LogType,
    level: LogLevel,
    flags: LogFlags,
    activity_id: Option<Guid>,
    related_activity_id: Option<Guid>,
    correlation_id: Option<Guid>,
    name: Option<&[u8]>,
    target: Option<&[u8]>,
    fields: Option<&[u8]>,
    message: &[u8],
    timestamp: u64,
) -> Vec<u8> {
    let name = name.map_or(&[] as &[u8], |slice| {
        truncate_slice(slice, TRACE_LOGGING_NAME_MAX_SIZE)
    });
    let name_size = name.len();
    let name_offset = 0;

    let target = target.map_or(&[] as &[u8], |slice| {
        truncate_slice(slice, TRACE_LOGGING_TARGET_MAX_SIZE)
    });
    let target_size = target.len();
    let target_offset = name_offset + name_size;

    let fields = fields.map_or(&[] as &[u8], |slice| {
        truncate_slice(slice, TRACE_LOGGING_FIELDS_MAX_SIZE)
    });
    let fields_size = fields.len();
    let fields_offset = target_offset + target_size;

    let message = truncate_slice(message, TRACE_LOGGING_MESSAGE_MAX_SIZE);
    let message_size = message.len();
    let message_offset = fields_offset + fields_size;

    let mut buffer = vec![];

    let header = TraceLoggingNotificationHeader {
        log_type,
        level: level.into(),
        flags,
        name: TraceLoggingBufferOffset {
            size: name_size as u16,
            offset: name_offset as u16,
        },
        target: TraceLoggingBufferOffset {
            size: target_size as u16,
            offset: target_offset as u16,
        },
        fields: TraceLoggingBufferOffset {
            size: fields_size as u16,
            offset: fields_offset as u16,
        },
        message: TraceLoggingBufferOffset {
            size: message_size as u16,
            offset: message_offset as u16,
        },
        mbz0: 0,
        activity_id: activity_id.unwrap_or(Guid::ZERO),
        related_activity_id: related_activity_id.unwrap_or(Guid::ZERO),
        correlation_id: correlation_id.unwrap_or(Guid::ZERO),
        timestamp,
    };

    buffer.extend_from_slice(header.as_bytes());
    buffer.extend_from_slice(name);
    buffer.extend_from_slice(target);
    buffer.extend_from_slice(fields);
    buffer.extend_from_slice(message);

    buffer
}
