// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Time types for mesh protobuf encoding.

use crate::inplace::InplaceOption;
use crate::inplace_some;
use crate::protobuf::MessageReader;
use crate::protobuf::MessageSizer;
use crate::protobuf::MessageWriter;
use crate::protofile::DescribeField;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;
use crate::table::DescribeTable;
use crate::table::TableEncoder;
use crate::DecodeError;
use crate::MessageDecode;
use crate::MessageEncode;
use mesh_protobuf::Protobuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use thiserror::Error;

const NANOS_PER_SEC: u32 = 1_000_000_000;

impl DescribeTable for Timestamp {
    const DESCRIPTION: MessageDescription<'static> = MessageDescription::External {
        name: "google.protobuf.Timestamp",
        import_path: "google/protobuf/timestamp.proto",
    };
}

/// A timestamp representing a point in UTC time with nanosecond resolution.
#[derive(Debug, Protobuf, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    /// The number of seconds of UTC time since the Unix epoch.
    #[mesh(1, encoding = "mesh_protobuf::encoding::VarintField")]
    pub seconds: i64,
    /// Non-negative fractions of a second at nanosecond resolution.
    #[mesh(2, encoding = "mesh_protobuf::encoding::VarintField")]
    pub nanos: i32,
}

impl From<SystemTime> for Timestamp {
    fn from(value: SystemTime) -> Self {
        match value.duration_since(UNIX_EPOCH) {
            Ok(since_epoch) => Self {
                seconds: since_epoch.as_secs() as i64,
                nanos: since_epoch.subsec_nanos() as i32,
            },
            Err(err) => {
                let since_epoch = err.duration();
                if since_epoch.subsec_nanos() == 0 {
                    Self {
                        seconds: -(since_epoch.as_secs() as i64),
                        nanos: 0,
                    }
                } else {
                    Self {
                        seconds: -(since_epoch.as_secs() as i64) - 1,
                        nanos: (1_000_000_000 - since_epoch.subsec_nanos()) as i32,
                    }
                }
            }
        }
    }
}

#[derive(Debug, Error)]
#[error("timestamp out of range for system time")]
pub struct TimestampOutOfRange;

impl TryFrom<Timestamp> for SystemTime {
    type Error = TimestampOutOfRange;

    fn try_from(value: Timestamp) -> Result<Self, Self::Error> {
        if value.nanos < 0 || value.nanos >= NANOS_PER_SEC as i32 {
            return Err(TimestampOutOfRange);
        }
        if value.seconds >= 0 {
            SystemTime::UNIX_EPOCH
                .checked_add(Duration::new(value.seconds as u64, value.nanos as u32))
        } else {
            let secs = value.seconds.checked_neg().ok_or(TimestampOutOfRange)? as u64;
            if value.nanos == 0 {
                SystemTime::UNIX_EPOCH.checked_sub(Duration::new(secs, 0))
            } else {
                SystemTime::UNIX_EPOCH
                    .checked_sub(Duration::new(secs - 1, NANOS_PER_SEC - value.nanos as u32))
            }
        }
        .ok_or(TimestampOutOfRange)
    }
}

/// Protobuf-compatible encoding for [`Duration`].
pub struct DurationEncoding;

impl DescribeField<Duration> for DurationEncoding {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("google.protobuf.Duration");
}

impl<R> MessageEncode<Duration, R> for DurationEncoding {
    fn write_message(item: Duration, writer: MessageWriter<'_, '_, R>) {
        TableEncoder::write_message((item.as_secs(), item.subsec_nanos()), writer);
    }

    fn compute_message_size(item: &mut Duration, sizer: MessageSizer<'_>) {
        <TableEncoder as MessageEncode<_, R>>::compute_message_size(
            &mut (item.as_secs(), item.subsec_nanos()),
            sizer,
        );
    }
}

impl<R> MessageDecode<'_, Duration, R> for DurationEncoding {
    fn read_message(
        item: &mut InplaceOption<'_, Duration>,
        reader: MessageReader<'_, '_, R>,
    ) -> crate::Result<()> {
        let duration = item.take().unwrap_or_default();
        let message = (duration.as_secs(), duration.subsec_nanos());
        inplace_some!(message);
        TableEncoder::read_message(&mut message, reader)?;
        let (secs, nanos) = message.take().unwrap();
        if (secs as i64) < 0 || nanos >= NANOS_PER_SEC {
            return Err(DecodeError::DurationRange.into());
        }
        item.set(Duration::new(secs, nanos));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;
    use std::time::SystemTime;

    #[test]
    fn test_timestamp_system_time() {
        let check = |st: SystemTime| {
            let st2 = SystemTime::try_from(Timestamp::from(st)).unwrap();
            assert_eq!(st, st2);
        };

        check(SystemTime::now());
        check(SystemTime::now() + std::time::Duration::from_secs(1));
        check(SystemTime::now() - std::time::Duration::from_secs(1));
        check(SystemTime::UNIX_EPOCH - std::time::Duration::from_nanos(1_500_000_000));
        check(SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(1_500_000_000));

        assert_eq!(
            Timestamp::from(
                SystemTime::UNIX_EPOCH - std::time::Duration::from_nanos(1_500_000_000)
            ),
            Timestamp {
                seconds: -2,
                nanos: 500_000_000,
            }
        );
    }
}
