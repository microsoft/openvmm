// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh tracing backend.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod bounded;

use self::bounded::BoundedReceiver;
use self::bounded::BoundedSender;
use self::bounded::bounded;
use anyhow::Context as _;
use futures::FutureExt;
use futures::Stream;
use futures::future::join_all;
use guid::Guid;
use inspect::InspectMut;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::fs::File;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Filtered;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;

#[derive(Debug, MeshPayload)]
pub struct RemoteTracer {
    pub trace_writer: TraceWriter,
    pub trace_filter: MeshFilter,
    pub perf_trace_filter: MeshFilter,
    pub perf_trace_file: File,
    pub perf_trace_flush: mesh::Receiver<Rpc<(), ()>>,
}

#[derive(Debug, MeshPayload, Clone)]
pub struct MeshFilter {
    filter: mesh::Cell<String>,
}

impl MeshFilter {
    /// Wraps `layer` in a filter that will be dynamically updated by incoming
    /// mesh messages.
    ///
    /// Returns a future that polls for filter updates and applies them to the
    /// layer.
    pub fn apply<L, S, T>(
        self,
        layer: L,
        parse: impl Fn(&str) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<(
        reload::Layer<Filtered<L, T, S>, S>,
        impl Future<Output = ()> + 'static,
    )>
    where
        L: Layer<S> + Send + Sync,
        S: tracing::Subscriber + for<'span> LookupSpan<'span>,
        T: tracing_subscriber::layer::Filter<S> + Send + Sync + 'static,
    {
        let targets = self
            .filter
            .with(|filter| parse(filter))
            .context("failed to parse filter")?;

        let (layer, reload_handle) = reload::Layer::new(layer.with_filter(targets));
        let mut filter_cell = self.filter;
        let task = async move {
            loop {
                // Apply the current filter. This runs even the first time
                // through to ensure that any events emited by this path are
                // logged--before this task starts, the tracing subscriber has
                // not yet been registered.
                filter_cell.with(|filter| match parse(filter) {
                    Ok(new_targets) => {
                        let _ = reload_handle.modify(|layer| *layer.filter_mut() = new_targets);
                        tracing::info!(filter = filter.as_str(), "updated trace filter");
                    }
                    Err(err) => {
                        tracing::error!(
                            error = err.as_ref() as &dyn std::error::Error,
                            "failed to update filter"
                        );
                    }
                });
                // Wait for the next value.
                filter_cell.wait_next().await;
            }
        };

        Ok((layer, task))
    }
}

#[derive(Debug)]
struct MeshFilterUpdater {
    updater: mesh::CellUpdater<String>,
}

impl MeshFilterUpdater {
    fn get(&self) -> &str {
        self.updater.get()
    }

    fn update(&mut self, filter: &str) -> anyhow::Result<()> {
        // Validate the filter.
        let _: Targets = filter.parse().context("invalid filter")?;
        self.updater.set(filter.into()).now_or_never();
        Ok(())
    }
}

impl InspectMut for MeshFilterUpdater {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        match req.update() {
            Ok(req) => match self.update(req.new_value()) {
                Ok(()) => req.succeed(self.get()),
                Err(err) => req.fail(err),
            },
            Err(req) => req.value(self.get()),
        }
    }
}

fn filter(initial: String) -> anyhow::Result<(MeshFilterUpdater, MeshFilter)> {
    // Validate the filter.
    let _: Targets = initial.parse().context("invalid filter")?;
    let (updater, cell) = mesh::cell(initial);
    Ok((MeshFilterUpdater { updater }, MeshFilter { filter: cell }))
}

struct MeshFlusher {
    spawn: Box<dyn Spawn>,
    remotes: Vec<mesh::Sender<Rpc<(), ()>>>,
}

impl MeshFlusher {
    fn add(&mut self) -> mesh::Receiver<Rpc<(), ()>> {
        let (send, recv) = mesh::channel();
        self.remotes.retain(|s| !s.is_closed());
        self.remotes.push(send);
        recv
    }
}

impl InspectMut for MeshFlusher {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        match req.update() {
            Ok(req) => {
                let join = join_all(self.remotes.iter().map(|r| r.call(|rpc| rpc, ())));
                let req = req.defer();
                self.spawn
                    .spawn("trace-flush", async move {
                        let _ = join.await;
                        req.succeed(true);
                    })
                    .detach();
            }
            Err(req) => req.value(false),
        }
    }
}

#[derive(Debug, MeshPayload)]
pub struct TracingRequest {
    pub log_type: Type,
    pub timestamp: u64,
    pub level: Level,
    pub name: Option<Vec<u8>>,
    pub target: Option<Vec<u8>>,
    pub fields: Option<Vec<u8>>,
    pub activity_id: Option<Guid>,
    pub related_activity_id: Option<Guid>,
    pub correlation_id: Option<Guid>,
    pub message: Vec<u8>,
}

#[derive(Debug, MeshPayload)]
pub enum Type {
    Event = 0,
    SpanEnter = 1,
    SpanExit = 2,
}

#[derive(Debug, MeshPayload)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(MeshPayload, Debug)]
pub struct TraceWriter(BoundedSender<TracingRequest>);

impl From<BoundedSender<TracingRequest>> for TraceWriter {
    fn from(sender: BoundedSender<TracingRequest>) -> Self {
        Self(sender)
    }
}

impl TraceWriter {
    pub fn send(
        &self,
        log_type: Type,
        timestamp: u64,
        level: Level,
        name: Option<Vec<u8>>,
        target: Option<Vec<u8>>,
        fields: Option<Vec<u8>>,
        activity_id: Option<Guid>,
        related_activity_id: Option<Guid>,
        correlation_id: Option<Guid>,
        message: Vec<u8>,
    ) -> bool {
        self.0
            .try_send(TracingRequest {
                log_type,
                timestamp,
                level,
                name,
                target,
                fields,
                activity_id,
                related_activity_id,
                correlation_id,
                message,
            })
            .is_ok()
    }
}

/// Object to configure and reconfigure tracing for Underhill.
#[derive(InspectMut)]
pub struct TracingBackend {
    #[inspect(skip)]
    state: BackendState,

    #[inspect(mut, safe)]
    filter: MeshFilterUpdater,
    #[inspect(rename = "perf/filter", mut)]
    perf_filter: MeshFilterUpdater,
    #[inspect(rename = "perf/flush", mut)]
    perf_flush: MeshFlusher,
}

struct BackendState {
    trace_writer: mesh::Sender<BoundedReceiver<TracingRequest>>,
    trace_filter: MeshFilter,
    perf_trace_filter: MeshFilter,
    perf_trace_file: File,
    flush_send: mesh::Sender<Rpc<(), ()>>,
    task: Task<()>,
}

pub struct TracingRequestStream {
    new_receivers: mesh::Receiver<BoundedReceiver<TracingRequest>>,
    receivers: Vec<BoundedReceiver<TracingRequest>>,
}

impl Stream for TracingRequestStream {
    type Item = TracingRequest;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut i = 0;
        while let Poll::Ready(Some(recv)) = Pin::new(&mut this.new_receivers).poll_next(cx) {
            this.receivers.push(recv);
        }
        while i < this.receivers.len() {
            match Pin::new(&mut this.receivers[i]).poll_next(cx) {
                r @ Poll::Ready(Some(_)) => return r,
                Poll::Ready(None) => {
                    this.receivers.swap_remove(i);
                }
                Poll::Pending => {}
            }
            i += 1;
        }
        Poll::Pending
    }
}

impl TracingBackend {
    /// Spawns worker that sends traces to the host
    pub fn new<Fut, F>(
        driver: impl 'static + Spawn,
        trace_filter: String,
        perf_trace_filter: String,
        handle_requests: F,
    ) -> anyhow::Result<Self>
    where
        F: 'static + Send + FnOnce(TracingRequestStream, mesh::Receiver<Rpc<(), ()>>) -> Fut,
        Fut: 'static + Send + Future<Output = ()>,
    {
        let (send, recv) = mesh::channel();

        let (trace_filter_updater, trace_filter) = filter(trace_filter)?;
        let (perf_trace_filter_updater, perf_trace_filter) = filter(perf_trace_filter)?;

        // This perf trace file can be shared across all processes in the mesh,
        // without extra synchronization. This works because the file extending
        // writes are atomic.
        let perf_trace_file = File::options()
            .append(true)
            .create(true)
            .open("underhill.perfetto")
            .context("failed to open underhill.perfetto")?;

        let (flush_send, flush_recv) = mesh::channel();
        let task = driver.spawn(
            "log write",
            handle_requests(
                TracingRequestStream {
                    new_receivers: recv,
                    receivers: Vec::new(),
                },
                flush_recv,
            ),
        );
        Ok(Self {
            state: BackendState {
                trace_writer: send,
                trace_filter,
                perf_trace_filter,
                perf_trace_file,
                flush_send,
                task,
            },
            filter: trace_filter_updater,
            perf_filter: perf_trace_filter_updater,
            perf_flush: MeshFlusher {
                spawn: Box::new(driver),
                remotes: Vec::new(),
            },
        })
    }

    pub fn tracer(&mut self) -> RemoteTracer {
        let (send, recv) = bounded(256);
        self.state.trace_writer.send(recv);
        RemoteTracer {
            trace_writer: TraceWriter(send),
            trace_filter: self.state.trace_filter.clone(),
            perf_trace_filter: self.state.perf_trace_filter.clone(),
            perf_trace_file: self.state.perf_trace_file.try_clone().unwrap(),
            perf_trace_flush: self.perf_flush.add(),
        }
    }

    /// Requests that all sent log messages have been flushed.
    pub async fn flush(&mut self) {
        self.state.flush_send.call(|x| x, ()).await.ok();
    }

    /// Shuts down the tracing backend.
    ///
    /// This implicitly flushes any sent log messages.
    pub async fn shutdown(self) {
        drop(self.state.flush_send);
        self.state.task.await;
    }
}
