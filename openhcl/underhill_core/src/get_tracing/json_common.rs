// Copyright (C) Microsoft Corporation. All rights reserved.

//! Types for the JSON encoding

use guid::Guid;
use serde::Serialize;
use serde::Serializer;
use std::fmt::Display;
use std::num::NonZeroU64;
use std::time::Duration;

/// A message in the format that the host expects.
///
/// This is generic so that different users can provide different field and
/// level data.
// TODO: Remove some redundant fields once the legacy notifications are deprecated.
#[derive(serde::Serialize)]
pub struct Message<'a, L: Display, F: Serialize> {
    #[serde(serialize_with = "serialize_time")]
    pub timestamp: Duration,
    #[serde(with = "serde_helpers::as_string")]
    pub level: L,
    pub target: &'a str,
    #[serde(
        with = "serde_helpers::as_string",
        skip_serializing_if = "Guid::is_zero"
    )]
    pub related_activity_id: Guid,
    pub fields: F,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missed_events: Option<NonZeroU64>,
}

#[derive(serde::Serialize)]
pub struct KmsgMessage<'a, L: Display, F: Serialize> {
    #[serde(serialize_with = "serialize_time")]
    pub timestamp: Duration,
    #[serde(with = "serde_helpers::as_string")]
    pub level: L,
    pub target: &'a str,
    pub fields: F,
}

#[derive(serde::Serialize)]
pub struct SpanMessage<'a, F: Serialize> {
    #[serde(serialize_with = "serialize_time")]
    pub timestamp: Duration,
    pub name: &'a str,
    pub op_code: u8,
    pub target: &'a str,
    pub level: &'a str,
    #[serde(with = "serde_helpers::as_string")]
    pub activity_id: Guid,
    #[serde(
        with = "serde_helpers::as_string",
        skip_serializing_if = "Guid::is_zero"
    )]
    pub related_activity_id: Guid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<F>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_taken_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_active_ns: Option<u64>,
}

fn serialize_time<S>(time: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.collect_str(&format_args!(
        "{}.{:09}s",
        time.as_secs(),
        time.subsec_nanos()
    ))
}
