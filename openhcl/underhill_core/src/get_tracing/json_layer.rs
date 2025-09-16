// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a [`tracing_subscriber::Layer`] that sends a JSON string to the
//! mesh tracing backend.

use super::json_common::Message;
use super::json_common::SpanMessage;
use guid::Guid;
use mesh_tracing::TraceWriter;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap;
use serde::ser::SerializeSeq;
use std::fmt::Debug;
use std::fmt::Display;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tracing::Id;
use tracing::Subscriber;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::span::Attributes;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// A JSON layer wrapping a [`TraceWriter`].
pub struct JsonMeshLayer {
    writer: TraceWriter,
    missed_events: AtomicU64,
}

impl JsonMeshLayer {
    /// Returns a new JSON layer.
    pub fn new(writer: TraceWriter) -> Self {
        Self {
            writer,
            missed_events: 0.into(),
        }
    }
}

struct EventWrap<'a>(&'a tracing::Event<'a>);

impl Serialize for EventWrap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = serializer.serialize_map(None)?;
        let mut serde_visitor = SerdeVisitor {
            serializer: s,
            result: Ok(()),
        };
        self.0.record(&mut serde_visitor);
        serde_visitor.result?;
        serde_visitor.serializer.end()
    }
}

struct SerdeVisitor<S, E> {
    serializer: S,
    result: Result<(), E>,
}

#[derive(Serialize)]
#[serde(transparent)]
struct AsString<T: Display>(#[serde(with = "serde_helpers::as_string")] T);

struct AsError<'a>(&'a dyn std::error::Error);

impl Serialize for AsError<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        let mut err = Some(self.0);
        while let Some(e) = err {
            seq.serialize_element(&AsString(e))?;
            err = e.source();
        }
        seq.end()
    }
}

impl<S: SerializeMap> SerdeVisitor<S, S::Error> {
    fn entry<T: Serialize>(&mut self, field: &Field, value: &T) {
        if self.result.is_ok() {
            self.result = self.serializer.serialize_entry(field.name(), value);
        }
    }
}

impl<S: SerializeMap> Visit for SerdeVisitor<S, S::Error> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.entry(field, &AsString(format_args!("{:?}", value)))
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.entry(field, &value)
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.entry(field, &value)
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.entry(field, &value)
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.entry(field, &value)
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.entry(field, &value)
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.entry(field, &value)
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.entry(field, &value)
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.entry(field, &AsError(value))
    }
}

struct GuidVisitor<'a> {
    name: &'a str,
    guid: Option<Guid>,
}

impl Visit for GuidVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == self.name {
            let value = &format!("{:?}", value);
            if let Ok(guid) = Guid::from_str(value) {
                self.guid = Some(guid);
            }
        }
    }
}

trait Recordable {
    fn record_guid(&self, visitor: &mut GuidVisitor<'_>);
}

impl Recordable for Attributes<'_> {
    fn record_guid(&self, visitor: &mut GuidVisitor<'_>) {
        self.record(visitor);
    }
}

impl Recordable for tracing::Event<'_> {
    fn record_guid(&self, visitor: &mut GuidVisitor<'_>) {
        self.record(visitor);
    }
}

fn get_guid_field(fields: &dyn Recordable, name: &'static str) -> Option<Guid> {
    let mut visitor = GuidVisitor { name, guid: None };
    fields.record_guid(&mut visitor);
    visitor.guid
}

struct SpanFields<'a>(&'a Attributes<'a>);

#[derive(Clone)]
struct SpanData {
    start_time: pal_async::timer::Instant,
    enter_time: Option<pal_async::timer::Instant>,
    active_time: Duration,
    activity_id: Guid,
    related_activity_id: Option<Guid>,
    correlation_id: Option<Guid>,
    sent_enter: bool,
}

impl Serialize for SpanFields<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = serializer.serialize_map(None)?;
        let mut serde_visitor = SerdeVisitor {
            serializer: s,
            result: Ok(()),
        };
        self.0.record(&mut serde_visitor);
        serde_visitor.result?;
        serde_visitor.serializer.end()
    }
}

fn generate_tracing_luid(time: pal_async::timer::Instant, span_id: u64) -> Guid {
    let mut guid = Guid::new_zeroed();

    let process_id = std::process::id() as u16;

    guid.as_mut_bytes()[..6].copy_from_slice(&time.as_nanos().to_le_bytes()[..6]);
    guid.as_mut_bytes()[6..8].copy_from_slice(&process_id.to_ne_bytes());
    guid.as_mut_bytes()[8..].copy_from_slice(&span_id.to_ne_bytes());

    guid
}

fn get_mesh_tracing_level(level: tracing::Level) -> mesh_tracing::Level {
    match level {
        tracing::Level::INFO => mesh_tracing::Level::Info,
        tracing::Level::DEBUG => mesh_tracing::Level::Debug,
        tracing::Level::TRACE => mesh_tracing::Level::Trace,
        tracing::Level::WARN => mesh_tracing::Level::Warn,
        _ => mesh_tracing::Level::Error,
    }
}

impl<S: Subscriber> Layer<S> for JsonMeshLayer
where
    S: for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let time = pal_async::timer::Instant::now();
        let level = get_mesh_tracing_level(*event.metadata().level());

        let parent_span_data = {
            if event.is_root() {
                None
            } else {
                if event.is_contextual() {
                    ctx.current_span().id().cloned()
                } else {
                    event.parent().cloned()
                }
                .and_then(|id| {
                    ctx.span(&id)
                        .and_then(|span| span.extensions().get::<SpanData>().cloned())
                })
            }
        };

        let (related_activity_id, correlation_id) = if let Some(parent_span_data) = parent_span_data
        {
            (
                Some(parent_span_data.activity_id),
                parent_span_data.correlation_id,
            )
        } else {
            (None, get_guid_field(event, "correlation_id"))
        };

        let missed_events = if self
            .missed_events
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
        {
            NonZeroU64::new(
                self.missed_events
                    .swap(0, std::sync::atomic::Ordering::Relaxed),
            )
        } else {
            None
        };

        let message = Message {
            timestamp: Duration::from_nanos(time.as_nanos()),
            level: *event.metadata().level(),
            target: event.metadata().target(),
            related_activity_id: related_activity_id.unwrap_or(Guid::ZERO),
            fields: EventWrap(event),
            missed_events,
        };

        if !self.writer.send(
            mesh_tracing::Type::Event,
            time.as_nanos() / 100,
            level,
            None,
            Some(event.metadata().target().as_bytes().to_vec()),
            Some(serde_json::to_vec(&EventWrap(event)).unwrap()),
            None,
            related_activity_id,
            correlation_id,
            serde_json::to_vec(&message).unwrap(),
        ) {
            self.missed_events.fetch_add(
                1 + missed_events.map_or(0, |x| x.get()),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let time = pal_async::timer::Instant::now();
        let span = ctx.span(id).unwrap();
        let span_fields = SpanFields(attrs);
        let level = get_mesh_tracing_level(*ctx.metadata(id).unwrap().level());

        let parent_span_data = {
            if attrs.is_root() {
                None
            } else {
                if attrs.is_contextual() {
                    ctx.current_span().id().cloned()
                } else {
                    attrs.parent().cloned()
                }
                .and_then(|id| {
                    ctx.span(&id)
                        .and_then(|span| span.extensions().get::<SpanData>().cloned())
                })
            }
        };

        let (related_activity_id, correlation_id) = if let Some(parent_span_data) = parent_span_data
        {
            (
                Some(parent_span_data.activity_id),
                parent_span_data
                    .correlation_id
                    .or_else(|| get_guid_field(attrs, "correlation_id")),
            )
        } else {
            (None, get_guid_field(attrs, "correlation_id"))
        };

        let mut span_data = SpanData {
            start_time: time,
            activity_id: generate_tracing_luid(time, id.into_u64()),
            related_activity_id,
            correlation_id,
            enter_time: None,
            active_time: Duration::ZERO,
            sent_enter: false,
        };

        let span_message = SpanMessage {
            timestamp: Duration::from_nanos(time.as_nanos()),
            name: span.metadata().name(),
            op_code: 1,
            target: span.metadata().target(),
            level: ctx.metadata(id).unwrap().level().as_str(),
            activity_id: span_data.activity_id,
            related_activity_id: related_activity_id.unwrap_or(Guid::ZERO),
            fields: Some(&span_fields),
            time_taken_ns: None,
            time_active_ns: None,
        };

        if self.writer.send(
            mesh_tracing::Type::SpanEnter,
            time.as_nanos() / 100,
            level,
            Some(span.metadata().name().as_bytes().to_vec()),
            Some(span.metadata().target().as_bytes().to_vec()),
            Some(serde_json::to_vec(&span_fields).unwrap()),
            Some(span_data.activity_id),
            span_data.related_activity_id,
            correlation_id,
            serde_json::to_vec(&span_message).unwrap(),
        ) {
            span_data.sent_enter = true;
        } else {
            self.missed_events
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        ctx.span(id).unwrap().extensions_mut().insert(span_data);
    }

    fn on_close(&self, id: Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let time = pal_async::timer::Instant::now();
        let span = ctx.span(&id).unwrap();
        let extensions = span.extensions();
        let span_data = extensions.get::<SpanData>().unwrap();
        if !span_data.sent_enter {
            return;
        }
        let time_taken = time - span_data.start_time;
        let level = get_mesh_tracing_level(*ctx.metadata(&id).unwrap().level());

        let span_message = SpanMessage {
            timestamp: Duration::from_nanos(time.as_nanos()),
            name: span.metadata().name(),
            op_code: 2,
            target: span.metadata().target(),
            level: ctx.metadata(&id).unwrap().level().as_str(),
            activity_id: span_data.activity_id,
            related_activity_id: span_data.related_activity_id.unwrap_or(Guid::ZERO),
            fields: None::<()>,
            time_taken_ns: Some(time_taken.as_nanos() as u64),
            time_active_ns: Some(span_data.active_time.as_nanos() as u64),
        };

        self.writer.send(
            mesh_tracing::Type::SpanExit,
            time.as_nanos() / 100,
            level,
            Some(span.metadata().name().as_bytes().to_vec()),
            Some(span.metadata().target().as_bytes().to_vec()),
            None,
            Some(span_data.activity_id),
            span_data.related_activity_id,
            span_data.correlation_id,
            serde_json::to_vec(&span_message).unwrap(),
        );
    }

    fn on_enter(&self, id: &Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let time = pal_async::timer::Instant::now();
        let span = ctx.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        if let Some(span_data) = extensions.get_mut::<SpanData>() {
            span_data.enter_time = Some(time);
        }
    }

    fn on_exit(&self, id: &Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span = ctx.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        if let Some(span_data) = extensions.get_mut::<SpanData>() {
            // Compute the current time once we have a mutable reference to the
            // span. This cannot occur earlier as another thread may have
            // modified the enter_time which would result in enter_time being
            // bigger than the current time.
            let time = pal_async::timer::Instant::now();
            if let Some(enter) = span_data.enter_time.take() {
                span_data.active_time += time.saturating_sub(enter);
            }
        }
    }
}
