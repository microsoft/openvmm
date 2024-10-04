// Copyright (C) Microsoft Corporation. All rights reserved.

//! Underhill tracing configuration using the GET tracing backend.
//!
//! Underhill tracing currently works as follows: there are two tracing backends
//! ("layers") that trace events go to. One is stderr, which is usually hooked
//! up to COM3. The other is a mesh tracing layer which forwards trace events to
//! a tracing task. The tracing task sends received trace events to the host via
//! a VMBus channel.
//!
//! Each Underhill process registers these layers, but the tracing task can only
//! run in a single process since the host only offers one instance of the
//! tracing channel. So the tracing task gets instantiated on the initial
//! Underhill process, and the subprocesses receive cross-process mesh channels
//! to use to send their tracing events.
//!
//! This is less efficient than sending the traces directly to the host from
//! each process, but this is expected to be a low-frequency channel in
//! production.

mod json_common;
mod json_layer;
mod kmsg_stream;
mod kmsg_writer;

use anyhow::Context;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use get_helpers::build_tracelogging_notification_buffer;
use get_protocol::LogFlags;
use get_protocol::LogLevel;
use get_protocol::LogType;
use get_protocol::TraceLoggingNotificationLegacy;
use get_protocol::GET_LOG_INTERFACE_GUID;
use get_protocol::GET_LOG_INTERFACE_GUID_LEGACY;
use mesh::rpc::Rpc;
use mesh_tracing::Level;
use mesh_tracing::RemoteTracer;
use mesh_tracing::TracingBackend;
use mesh_tracing::TracingRequestStream;
use mesh_tracing::Type;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use tracing_helpers::formatter::FieldFormatter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::format::Format;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use vmbus_async::async_dgram::AsyncSendExt;
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

fn tracing_log_level(level: Level) -> LogLevel {
    match level {
        Level::Trace | Level::Debug => LogLevel::VERBOSE,
        Level::Info => LogLevel::INFORMATION,
        Level::Warn => LogLevel::WARNING,
        Level::Error => LogLevel::ERROR,
    }
}

/// Initializes the tracing backend, opening the VMBus pipe to send tracing
/// events to the host.
pub fn init_tracing_backend(driver: impl 'static + SpawnDriver) -> anyhow::Result<TracingBackend> {
    let trace_filter = std::env::var("HVLITE_LOG").unwrap_or_else(|_| "info".to_owned());
    let perf_trace_filter = std::env::var("HVLITE_PERF_TRACE").unwrap_or_else(|_| "off".to_owned());

    let mut legacy_traces = false;

    let mut pipe = vmbus_user_channel::open_uio_device(&GET_LOG_INTERFACE_GUID)
        .and_then(|dev| vmbus_user_channel::message_pipe(&driver, dev))
        .map_err(|err| {
            tracing::error!(
                error = &err as &dyn std::error::Error,
                "failed to open the new vmbus tracing channel"
            );
            legacy_traces = true;
        })
        .ok();

    if legacy_traces {
        pipe = vmbus_user_channel::open_uio_device(&GET_LOG_INTERFACE_GUID_LEGACY)
            .and_then(|dev| vmbus_user_channel::message_pipe(&driver, dev))
            .map_err(|err| {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to open the legacy vmbus tracing channel"
                );
            })
            .ok();
    }

    let kmsg = kmsg_stream::KmsgStream::new(&driver, legacy_traces)?;

    let mut get_backend = pipe.map(|pipe| GetTracingBackend {
        pipe,
        legacy_traces,
    });

    TracingBackend::new(
        driver,
        trace_filter,
        perf_trace_filter,
        move |requests, flush| async move {
            if let Some(get_backend) = &mut get_backend {
                get_backend.run(requests, kmsg, flush).await;
            }
        },
    )
}

struct GetTracingBackend {
    pipe: vmbus_async::pipe::MessagePipe<vmbus_user_channel::MappedRingMem>,
    legacy_traces: bool,
}

impl GetTracingBackend {
    async fn run(
        &mut self,
        requests: TracingRequestStream,
        mut kmsg: kmsg_stream::KmsgStream,
        mut flush: mesh::Receiver<Rpc<(), ()>>,
    ) {
        let mut tracing_requests = requests.map(|request| {
            if self.legacy_traces {
                let mut notification = TraceLoggingNotificationLegacy::new_zeroed();
                let len = request.message.len().min(notification.message.len());
                notification.level = tracing_log_level(request.level);
                notification.size = len as u16;
                notification.message[..len].copy_from_slice(&request.message[..len]);
                notification.as_bytes().to_vec()
            } else {
                let log_type = match request.log_type {
                    Type::Event => LogType::EVENT,
                    Type::SpanEnter => LogType::SPAN_ENTER,
                    Type::SpanExit => LogType::SPAN_EXIT,
                };

                build_tracelogging_notification_buffer(
                    log_type,
                    tracing_log_level(request.level),
                    LogFlags::new(),
                    request.activity_id,
                    request.related_activity_id,
                    request.correlation_id,
                    request.name.as_ref().map(Vec::as_ref),
                    request.target.as_ref().map(Vec::as_ref),
                    request.fields.as_ref().map(Vec::as_ref),
                    request.message.as_ref(),
                    request.timestamp,
                )
            }
        });

        enum Event {
            Trace(Vec<u8>),
            Flush(Rpc<(), ()>),
            Done,
        }

        let (_, mut write) = self.pipe.split();
        loop {
            let mut streams = (
                (&mut tracing_requests).map(Event::Trace),
                (&mut kmsg).map(Event::Trace),
                (&mut flush)
                    .map(Event::Flush)
                    .chain(futures::stream::repeat_with(|| Event::Done)),
            )
                .merge();

            let flush_response = loop {
                let trace_type = streams.next().await.unwrap();
                match trace_type {
                    Event::Trace(data) => {
                        write.send(&data).await.ok();
                    }
                    Event::Flush(Rpc((), response)) => break Some(response),
                    Event::Done => break None,
                }
            };

            // Drain everything we've got.
            while let Some(data) = tracing_requests.next().now_or_never().flatten() {
                write.send(&data).await.ok();
            }
            while let Some(data) = kmsg.next().now_or_never().flatten() {
                write.send(&data).await.ok();
            }

            // Wait for the host to read everything.
            write.wait_empty().await.ok();

            if let Some(resp) = flush_response {
                resp.send(());
            } else {
                break;
            }
        }
    }
}

/// Enables tracing output to the tracing task and to stderr.
pub fn init_tracing(spawn: impl Spawn, tracer: RemoteTracer) -> anyhow::Result<()> {
    if std::env::var_os("HVLITE_DISABLE_TRACING_RATELIMITS").map_or(false, |v| !v.is_empty()) {
        tracelimit::disable_rate_limiting(true);
    }

    let span_events = match std::env::var("OPENVMM_SHOW_SPANS")
        .as_ref()
        .map_or("", |v| v.as_str())
    {
        "close" => fmt::format::FmtSpan::CLOSE,
        "1" | "true" => fmt::format::FmtSpan::NEW | fmt::format::FmtSpan::CLOSE,
        "" => fmt::format::FmtSpan::NONE,
        x => anyhow::bail!("invalid OPENVMM_SHOW_SPANS value: {x}"),
    };

    // Format events into JSON and send them to mesh backend.
    let json_fmt_layer = json_layer::JsonMeshLayer::new(tracer.trace_writer);

    // Output nicely readable events to kmsg (and therefore also serial).
    let kmsg_layer = fmt::layer()
        .event_format(
            Format::default()
                .without_time()
                .with_ansi(false)
                .with_target(false),
        )
        .fmt_fields(FieldFormatter)
        .log_internal_errors(true)
        .with_span_events(span_events)
        .with_writer(kmsg_writer::KmsgWriter::new(
            kmsg_defs::UNDERHILL_KMSG_FACILITY,
        )?);

    // Filter out events that aren't allowed for CVMs, when filtering is enabled.
    let cvm_filter = underhill_confidentiality::confidential_filtering_enabled()
        .then(cvm_tracing::confidential_event_filter);

    let output_layers = json_fmt_layer.and_then(kmsg_layer);
    let with_cvm_filter = output_layers.with_filter(cvm_filter);

    // Filter events based on the updatable-via-inspect target filter.
    // Make sure this is the outermost layer for performance reasons.
    let combined = tracer.trace_filter.apply(&spawn, with_cvm_filter)?;

    tracing_subscriber::registry()
        .with(combined)
        .try_init()
        .context("failed to enable tracing")?;

    Ok(())
}
