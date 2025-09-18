// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for workers that can run on mesh nodes.

use anyhow::Context;
use futures::Stream;
use futures::StreamExt;
use futures::executor::block_on;
use futures::stream::FusedStream;
use futures_concurrency::stream::Merge;
use inspect::Inspect;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use std::fmt;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::Poll;
use std::thread;
use unicycle::FuturesUnordered;

/// A unique identifier for a worker, used to specify which worker to launch.
#[derive(Copy, Clone, Debug)]
pub struct WorkerId<T>(&'static str, PhantomData<T>);

impl<T> WorkerId<T> {
    /// Makes a new worker ID with the name `id`.
    pub const fn new(id: &'static str) -> Self {
        Self(id, PhantomData)
    }

    /// Gets the ID string.
    pub const fn id(&self) -> &'static str {
        self.0
    }
}

impl<T> fmt::Display for WorkerId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.0)
    }
}

/// Trait implemented by workers.
pub trait Worker: 'static + Sized {
    /// Parameters passed to launch the worker. Used with [`Worker::new`].
    ///
    /// For this worker to be spawned on a remote node, `Parameters` must
    /// implement [`MeshPayload`].
    type Parameters: 'static + Send;

    /// State used to implement hot restart. Used with [`Worker::restart`].
    type State: 'static + MeshPayload + Send;

    /// String identifying the Worker. Used when launching workers in separate processes
    /// to specify which workers are supported and which worker to launch.
    /// IDs must be unique within a given worker host.
    const ID: WorkerId<Self::Parameters>;

    /// Instantiates the worker.
    ///
    /// The worker should not start running yet, but it can allocate any resources
    /// necessary to run.
    fn new(parameters: Self::Parameters) -> anyhow::Result<Self>;

    /// Restarts a worker from a previous worker's execution state.
    fn restart(state: Self::State) -> anyhow::Result<Self>;

    /// Synchronously runs the worker on the current thread.
    ///
    /// The worker should respond to commands sent in `recv`. If `recv` is closed,
    /// the worker should exit.
    ///
    /// The worker ends when it returns from this function.
    fn run(self, recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()>;
}

/// Common requests for workers.
#[derive(Debug, MeshPayload)]
#[mesh(bound = "T: 'static + MeshPayload + Send")]
pub enum WorkerRpc<T> {
    /// Tear down.
    Stop,
    /// Tear down and send the state necessary to restart on the provided
    /// channel.
    Restart(FailableRpc<(), T>),
    /// Inspect the worker.
    Inspect(inspect::Deferred),
}

#[derive(Debug, MeshPayload)]
enum LaunchType {
    New {
        parameters: mesh::OwnedMessage,
    },
    Restart {
        send: mesh::Sender<WorkerRpc<mesh::OwnedMessage>>,
        events: mesh::Receiver<WorkerEvent>,
    },
}

/// A runner returned by [`worker_host()`]. Used to handle worker launch
/// requests.
///
/// This may be sent across processes via mesh.
#[derive(Debug, MeshPayload)]
pub struct WorkerHostRunner(mesh::Receiver<WorkerHostLaunchRequest>);

impl WorkerHostRunner {
    /// Runs the worker host until all corresponding [`WorkerHost`] instances
    /// have been dropped and all workers have exited.
    ///
    /// `factory` provides the set of possible workers to launch. Typically,
    /// this will be [`RegisteredWorkers`].
    pub async fn run(mut self, factory: impl WorkerFactory) {
        let mut rundown = FuturesUnordered::new();
        loop {
            let mut stream = ((&mut self.0).map(Some), (&mut rundown).map(|_| None)).merge();
            let launch_params = match stream.next().await {
                Some(Some(launch_params)) => launch_params,
                Some(None) => continue,
                None => break,
            };

            let _requestspan = tracing::info_span!("worker_host_launch_request").entered();

            let result = factory.builder(&launch_params.name);
            match result {
                Ok(runner) => {
                    // start a new thread and run the runner.
                    let (rundown_send, rundown_recv) = mesh::oneshot::<()>();
                    thread::Builder::new()
                        .name(format!("worker-{}", &launch_params.name))
                        .spawn(move || {
                            launch_params.request.launch(runner);
                            drop(rundown_send);
                        })
                        .expect("thread launch failed");

                    rundown.push(rundown_recv);
                }
                Err(err) => {
                    // TODO: kharp 2021-05-26 Better tracing of errors, maybe tracing_error?
                    launch_params.request.fail(err);
                }
            }
        }
    }
}

/// Represents a running [`Worker`] instance providing the ability to restart,
/// stop or wait for exit. To launch a worker and get a handle, use
/// [`WorkerHost::launch_worker`]
#[derive(Debug, MeshPayload, Inspect)]
pub struct WorkerHandle {
    #[inspect(skip)]
    name: String,
    #[inspect(flatten, send = "WorkerRpc::Inspect")]
    send: mesh::Sender<WorkerRpc<mesh::OwnedMessage>>,
    #[inspect(skip)]
    events: mesh::Receiver<WorkerEvent>,
}

impl Stream for WorkerHandle {
    type Item = WorkerEvent;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Poll::Ready(match std::task::ready!(self.events.poll_recv(cx)) {
            Ok(event) => Some(event),
            Err(mesh::RecvError::Error(err)) => Some(WorkerEvent::Failed(RemoteError::new(err))),
            Err(mesh::RecvError::Closed) => None,
        })
    }
}

impl FusedStream for WorkerHandle {
    fn is_terminated(&self) -> bool {
        self.events.is_terminated()
    }
}

/// A lifetime event for a worker.
#[derive(Debug, MeshPayload)]
pub enum WorkerEvent {
    /// The worker has stopped without error.
    Stopped,
    /// The worker has failed.
    Failed(RemoteError),
    /// The worker has started or restarted successfully.
    Started,
    /// The requested restart operation failed, but the worker is still running.
    RestartFailed(RemoteError),
}

impl WorkerHandle {
    /// Requests that the worker stop.
    pub fn stop(&mut self) {
        self.send.send(WorkerRpc::Stop);
    }

    /// Waits until the worker has stopped.
    pub async fn join(&mut self) -> anyhow::Result<()> {
        while let Some(event) = self.next().await {
            if let WorkerEvent::Failed(err) = event {
                return Err(err.into());
            }
        }
        Ok(())
    }

    /// Stops the worker, then restarts it, using the same state, on `host`.
    ///
    /// This can be used to upgrade a worker at runtime if `host` is a
    /// worker host in a new process.
    pub fn restart(&mut self, host: &WorkerHost) {
        let (send, recv) = mesh::channel();
        let (events_send, events) = mesh::channel();

        let send = std::mem::replace(&mut self.send, send);
        let events = std::mem::replace(&mut self.events, events);

        // Launch the new worker.
        host.launch_worker_internal(
            &self.name,
            recv,
            events_send,
            LaunchType::Restart { send, events },
        );
    }
}

/// A handle used to launch workers on a host.
///
/// You can get an instance of this by spawning a new host with
/// [`worker_host()`].
///
/// This may be sent across processes via mesh.
#[derive(Debug, MeshPayload, Clone)]
pub struct WorkerHost(mesh::Sender<WorkerHostLaunchRequest>);

/// Returns a new [`WorkerHost`], [`WorkerHostRunner`] pair.
///
/// The [`WorkerHost`] is used to launch workers, while the [`WorkerHostRunner`]
/// is used to handle worker launch requests. The caller must start
/// [`WorkerHostRunner::run()`] on an appropriate task before `WorkerHost` will
/// be able to launch workers.
///
/// This is useful over just using [`launch_local_worker`] because it provides
/// an indirection between the identity of the workers being launched
/// (identified via [`WorkerId`]) and the concrete worker implementation. This
/// can be used to swap worker implementations, improve build times, and to
/// support launching workers across process boundaries.
///
/// To achieve this latter feat, note that either half of the returned tuple may
/// be sent to over a mesh channel to another process, allowing a worker to be
/// spawned in a separate process from the caller. This can be useful for fault
/// or resource isolation and for security sandboxing.
///
/// # Example
/// ```
/// # use mesh_worker::{worker_host, WorkerHost, WorkerHostRunner, RegisteredWorkers, register_workers};
/// # use mesh_worker::test_support::DUMMY_WORKER as MY_WORKER;
/// # use futures::executor::block_on;
/// # register_workers!(mesh_worker::test_support::DummyWorker<u32>);
/// # block_on(async {
/// let (host, runner) = worker_host();
/// // Run the worker host on a separate thread. (Typically this would just be
/// // a separate task in your async framework.)
/// std::thread::spawn(|| block_on(runner.run(RegisteredWorkers)));
/// // Launch a worker by ID. This will call to the worker host runner.
/// host.launch_worker(MY_WORKER, ()).await.unwrap();
/// # })
/// ```
pub fn worker_host() -> (WorkerHost, WorkerHostRunner) {
    let (send, recv) = mesh::mpsc_channel();
    (WorkerHost(send), WorkerHostRunner(recv))
}

impl WorkerHost {
    /// Launches a [`Worker`] instance on this host.
    ///
    /// Returns before the worker has finished launching. Look for the
    /// [`WorkerEvent::Started`] event to ensure the worker did not fail to
    /// start.
    pub fn start_worker<T>(&self, id: WorkerId<T>, params: T) -> anyhow::Result<WorkerHandle>
    where
        T: 'static + MeshPayload + Send,
    {
        self.start_worker_inner(id.id(), mesh::OwnedMessage::new(params))
    }

    fn start_worker_inner(
        &self,
        id: &str,
        parameters: mesh::OwnedMessage,
    ) -> anyhow::Result<WorkerHandle> {
        let (events_send, events_recv) = mesh::channel();
        let (rpc_send, rpc_recv) = mesh::channel();
        self.launch_worker_internal(id, rpc_recv, events_send, LaunchType::New { parameters });
        Ok(WorkerHandle {
            name: id.to_string(),
            send: rpc_send,
            events: events_recv,
        })
    }

    /// Launches a [`Worker`] instance on this host, waiting for the worker to
    /// start running.
    pub async fn launch_worker<T>(&self, id: WorkerId<T>, params: T) -> anyhow::Result<WorkerHandle>
    where
        T: 'static + MeshPayload + Send,
    {
        let mut handle = self.start_worker_inner(id.id(), mesh::OwnedMessage::new(params))?;
        match handle.next().await.context("failed to launch worker")? {
            WorkerEvent::Started => Ok(handle),
            WorkerEvent::Failed(err) => Err(err).context("failed to launch worker")?,
            WorkerEvent::Stopped | WorkerEvent::RestartFailed(_) => {
                anyhow::bail!("received invalid worker event")
            }
        }
    }

    fn launch_worker_internal(
        &self,
        id: &str,
        rpc_recv: mesh::Receiver<WorkerRpc<mesh::OwnedMessage>>,
        events_send: mesh::Sender<WorkerEvent>,
        launch_type: LaunchType,
    ) {
        let request = WorkerHostLaunchRequest {
            name: id.to_string(),
            request: WorkerLaunchRequest {
                rpc: rpc_recv,
                events: events_send,
                launch_type,
            },
        };

        self.0.send(request);
    }
}

/// Launches a worker locally.
///
/// When launched via this API, a worker's parameters do not have to derive
/// `MeshPayload`.
///
/// # Example
/// ```
/// # use mesh_worker::test_support::DUMMY_WORKER;
/// # use mesh_worker::WorkerHost;
/// # use mesh_worker::launch_local_worker;
/// # type MyWorker = mesh_worker::test_support::DummyWorker<u32>;
/// # futures::executor::block_on(async {
/// let worker = launch_local_worker::<MyWorker>(()).await.unwrap();
/// # })
/// ```
pub async fn launch_local_worker<T: Worker>(
    parameters: T::Parameters,
) -> anyhow::Result<WorkerHandle> {
    let (rpc_send, rpc_recv) = mesh::channel();
    let (events_send, events_recv) = mesh::channel();
    let (result_send, result_recv) = mesh::oneshot();

    thread::Builder::new()
        .name(format!("worker-{}", &T::ID.id()))
        .spawn(move || match T::new(parameters) {
            Ok(worker) => {
                result_send.send(Ok(()));
                match worker.run(rpc_recv) {
                    Ok(()) => {
                        events_send.send(WorkerEvent::Stopped);
                    }
                    Err(err) => {
                        events_send.send(WorkerEvent::Failed(RemoteError::new(err)));
                    }
                }
            }
            Err(err) => {
                result_send.send(Err(err));
            }
        })
        .expect("thread launch failed");

    result_recv.await.unwrap()?;
    Ok(WorkerHandle {
        name: T::ID.id().to_owned(),
        // Erase the type of the worker state.
        send: mesh::local_node::Port::from(rpc_send).into(),
        events: events_recv,
    })
}

/// Trait implemented by a type that can dispatch requests to a worker.
///
/// This trait is generally not used directly. Instead, use either
/// [`RegisteredWorkers`], or generate a factory type with the
/// [`crate::runnable_workers!`] macro.
pub trait WorkerFactory: 'static + Send + Sync {
    /// Returns a builder for the worker with the given name.
    fn builder(&self, name: &str) -> anyhow::Result<WorkerBuilder>;
}

#[derive(Debug, MeshPayload)]
struct WorkerLaunchRequest {
    rpc: mesh::Receiver<WorkerRpc<mesh::OwnedMessage>>,
    events: mesh::Sender<WorkerEvent>,
    launch_type: LaunchType,
}

impl WorkerLaunchRequest {
    fn fail(self, err: anyhow::Error) {
        match self.launch_type {
            LaunchType::New { .. } => {
                self.events.send(WorkerEvent::Failed(RemoteError::new(err)));
            }
            LaunchType::Restart { send, events } => {
                // Report the error and revert communications to the old worker.
                self.events
                    .send(WorkerEvent::RestartFailed(RemoteError::new(err)));
                self.rpc.bridge(send);
                self.events.bridge(events);
            }
        }
    }

    fn launch(self, builder: WorkerBuilder) {
        let worker = match self.launch_type {
            LaunchType::New { parameters } => {
                let _span =
                    tracing::info_span!("worker_new", name = builder.id, action = "new").entered();
                match builder.build_and_run(BuildRequest::New(parameters)) {
                    Ok(worker) => worker,
                    Err(err) => {
                        self.events.send(WorkerEvent::Failed(RemoteError::new(err)));
                        return;
                    }
                }
            }
            LaunchType::Restart { send, events } => {
                let state_recv = send.call_failable(WorkerRpc::Restart, ());
                let state = match block_on(state_recv) {
                    Ok(state) => state,
                    Err(err) => {
                        self.events
                            .send(WorkerEvent::RestartFailed(RemoteError::new(err)));
                        // Revert communications to the old worker.
                        self.events.bridge(events);
                        self.rpc.bridge(send);
                        return;
                    }
                };
                let _span =
                    tracing::info_span!("worker_new", name = builder.id, action = "restart")
                        .entered();
                match builder.build_and_run(BuildRequest::Restart(state)) {
                    Ok(worker) => worker,
                    Err(err) => {
                        self.events.send(WorkerEvent::Failed(RemoteError::new(err)));
                        return;
                    }
                }
            }
        };

        self.events.send(WorkerEvent::Started);
        match worker.run(self.rpc) {
            Ok(()) => {
                self.events.send(WorkerEvent::Stopped);
            }
            Err(err) => {
                self.events.send(WorkerEvent::Failed(RemoteError::new(err)));
            }
        }
    }
}

/// A builder for a worker.
pub struct WorkerBuilder {
    inner: Box<dyn WorkerBuildAndRun>,
    id: &'static str,
}

impl WorkerBuilder {
    /// Returns a builder for `T`.
    pub fn new<T: Worker>() -> Self
    where
        T::Parameters: MeshPayload,
    {
        Self {
            inner: Box::new(BuilderInner::<T>(PhantomData)),
            id: T::ID.id(),
        }
    }

    fn build_and_run(self, request: BuildRequest) -> anyhow::Result<Box<dyn Run>> {
        self.inner.build_and_run(request)
    }
}

#[doc(hidden)]
pub enum BuildRequest {
    New(mesh::OwnedMessage),
    Restart(mesh::OwnedMessage),
}

struct BuilderInner<T: Worker>(PhantomData<fn() -> T>);

trait WorkerBuildAndRun: Send {
    fn build_and_run(self: Box<Self>, request: BuildRequest) -> anyhow::Result<Box<dyn Run>>;
}

trait Run {
    fn run(
        self: Box<Self>,
        recv: mesh::Receiver<WorkerRpc<mesh::OwnedMessage>>,
    ) -> anyhow::Result<()>;
}

impl<T: Worker> Run for T {
    fn run(
        self: Box<Self>,
        recv: mesh::Receiver<WorkerRpc<mesh::OwnedMessage>>,
    ) -> anyhow::Result<()> {
        // Unerase the type of the worker state.
        let recv = mesh::local_node::Port::from(recv).into();
        Worker::run(*self, recv)
    }
}

impl<T: Worker> WorkerBuildAndRun for BuilderInner<T>
where
    T::Parameters: MeshPayload,
{
    fn build_and_run(self: Box<Self>, request: BuildRequest) -> anyhow::Result<Box<dyn Run>> {
        let worker = match request {
            BuildRequest::New(parameters) => {
                T::new(parameters.parse().context("failed to receive parameters")?)
            }
            BuildRequest::Restart(state) => T::restart(
                // Unerase the type of the worker state.
                state
                    .serialize()
                    .into_message()
                    .context("failed to parse restart state")?,
            ),
        }?;

        Ok(Box::new(worker))
    }
}

/// Generates a type that defines the set of workers that can be run by a worker host.
/// Generate a type to that can be used to match a requested worker name and run it.
///
/// The resulting type is an empty struct implementing the [`WorkerFactory`] trait.
///
/// This is used to enumerate the list of worker types a host can instantiate.
///
/// Workers can be conditionally enabled by tagging them with a corresponding `#[cfg]` attr.
///
/// # Example
///
/// ```no_run
/// # use mesh_worker::test_support;
/// # use mesh_worker::runnable_workers;
/// # type MyWorker1 = test_support::DummyWorker<u32>;
/// # type MyWorker2 = test_support::DummyWorker<i32>;
/// runnable_workers! {
///     RunnableWorkers {
///         MyWorker1,
///         #[cfg(unix)]
///         MyWorker2,
///     }
/// }
/// ```
#[macro_export]
macro_rules! runnable_workers {
    (
        $name:ident {
            $($(#[$vattr:meta])* $worker:ty),*$(,)?
        }
    ) => {

        #[derive(Debug, Clone)]
        struct $name;

        impl $crate::WorkerFactory for $name {
            fn builder(&self, name: &str) -> anyhow::Result<$crate::WorkerBuilder> {
                $(
                    $(#[$vattr])*
                    {
                        if name == <$worker as $crate::Worker>::ID.id() {
                            return Ok($crate::WorkerBuilder::new::<$worker>());
                        }
                    }
                )*

                anyhow::bail!("unsupported worker {name}")
            }
        }
    };
}

#[doc(hidden)]
pub mod private {
    // UNSAFETY: Needed for linkme.
    #![expect(unsafe_code)]

    use super::RegisteredWorkers;
    use super::WorkerFactory;
    use crate::Worker;
    use crate::WorkerBuilder;
    pub use linkme;
    use mesh::MeshPayload;

    // Use Option<&X> in case the linker inserts some stray nulls, as we think
    // it might on Windows.
    //
    // See <https://devblogs.microsoft.com/oldnewthing/20181108-00/?p=100165>.
    #[linkme::distributed_slice]
    pub static WORKERS: [Option<&'static RegisteredWorker>] = [..];

    // Always have at least one entry to work around linker bugs.
    //
    // See <https://github.com/llvm/llvm-project/issues/65855>.
    #[linkme::distributed_slice(WORKERS)]
    static WORKAROUND: Option<&'static RegisteredWorker> = None;

    pub struct RegisteredWorker {
        id: &'static str,
        build: fn() -> WorkerBuilder,
    }

    impl RegisteredWorker {
        pub const fn new<T: Worker>() -> Self
        where
            T::Parameters: MeshPayload,
        {
            Self {
                id: T::ID.id(),
                build: WorkerBuilder::new::<T>,
            }
        }
    }

    impl WorkerFactory for RegisteredWorkers {
        fn builder(&self, name: &str) -> anyhow::Result<WorkerBuilder> {
            for worker in WORKERS.iter().flatten() {
                if worker.id == name {
                    return Ok((worker.build)());
                }
            }
            anyhow::bail!("unsupported worker {name}")
        }
    }

    /// Registers workers for use with
    /// [`RegisteredWorkers`](super::RegisteredWorkers).
    ///
    /// You can invoke this macro multiple times, even from different crates.
    /// All registered workers will be available from any user of
    /// `RegisteredWorkers`.
    #[macro_export]
    macro_rules! register_workers {
        {} => {};
        { $($(#[$attr:meta])* $worker:ty),+ $(,)? } => {
            $(
            $(#[$attr])*
            const _: () = {
                use $crate::private;
                use private::linkme;

                #[linkme::distributed_slice(private::WORKERS)]
                #[linkme(crate = linkme)]
                static WORKER: Option<&'static private::RegisteredWorker> = Some(&private::RegisteredWorker::new::<$worker>());
            };
            )*
        };
    }
}

/// A worker factory that can build any worker built with
/// [`register_workers`](crate::register_workers).
///
/// ```
/// # use mesh_worker::register_workers;
/// # use mesh_worker::RegisteredWorkers;
/// # use futures::executor::block_on;
/// # type MyWorker1 = mesh_worker::test_support::DummyWorker<u32>;
/// # type MyWorker2 = mesh_worker::test_support::DummyWorker<i32>;
/// register_workers! {
///     MyWorker1,
///     MyWorker2,
/// }
///
/// // Construct a worker host for these workers.
/// let (host, runner) = mesh_worker::worker_host();
/// std::thread::spawn(|| block_on(runner.run(RegisteredWorkers)));
/// ```
#[derive(Debug, Clone)]
pub struct RegisteredWorkers;

/// A request to launch a worker on a host.
#[derive(Debug, MeshPayload)]
struct WorkerHostLaunchRequest {
    /// Name of the worker to launch
    name: String,
    /// Request parameters.
    request: WorkerLaunchRequest,
}

#[cfg(test)]
mod tests {
    use super::Worker;
    use super::WorkerFactory;
    use super::WorkerId;
    use super::WorkerRpc;
    use crate::launch_local_worker;
    use crate::worker::WorkerEvent;
    use futures::StreamExt;
    use futures::executor::block_on;
    use mesh::MeshPayload;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use test_with_tracing::test;

    struct TestWorker {
        value: u64,
    }

    #[derive(MeshPayload, Default)]
    struct TestWorkerConfig {
        pub value: u64,
    }

    #[derive(MeshPayload)]
    struct TestWorkerState {
        pub value: u64,
    }

    impl Worker for TestWorker {
        type Parameters = TestWorkerConfig;
        type State = TestWorkerState;
        const ID: WorkerId<Self::Parameters> = WorkerId::new("TestWorker");

        fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
            Ok(Self {
                value: parameters.value,
            })
        }

        fn restart(state: Self::State) -> anyhow::Result<Self> {
            Ok(Self { value: state.value })
        }

        fn run(self, mut recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
            block_on(async {
                while let Ok(req) = recv.recv().await {
                    match req {
                        WorkerRpc::Stop => break,
                        WorkerRpc::Restart(rpc) => {
                            rpc.complete(Ok(TestWorkerState { value: self.value }));
                            break;
                        }
                        WorkerRpc::Inspect(_deferred) => (),
                    }
                }
                Ok(())
            })
        }
    }

    struct TestWorker2;

    impl Worker for TestWorker2 {
        type Parameters = ();
        type State = ();
        const ID: WorkerId<Self::Parameters> = WorkerId::new("TestWorker2");

        fn new(_parameters: Self::Parameters) -> anyhow::Result<Self> {
            Ok(Self)
        }

        fn restart(_state: ()) -> anyhow::Result<Self> {
            Ok(Self)
        }

        fn run(self, mut recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
            block_on(async {
                while let Ok(req) = recv.recv().await {
                    match req {
                        WorkerRpc::Stop => break,
                        WorkerRpc::Restart(rpc) => {
                            rpc.complete(Ok(()));
                            break;
                        }
                        WorkerRpc::Inspect(_deferred) => (),
                    }
                }
                Ok(())
            })
        }
    }

    runnable_workers! {
        RunnableWorkers1 {
            TestWorker,
        }
    }

    #[test]
    fn test_runnable_workers_unsupported() {
        let result = RunnableWorkers1.builder("foobar");

        assert!(result.is_err());
    }

    #[async_test]
    async fn test_launch_worker_remote_supported_worker(driver: DefaultDriver) {
        let (host, runner) = super::worker_host();

        // N.B. remote host needs to start first to recv and respond as
        // launch_worker blocks waiting for the response.
        let task = driver.spawn("runner", runner.run(RunnableWorkers1));

        let result = host.launch_worker(TestWorker::ID, Default::default()).await;

        assert!(result.is_ok());
        // drop the handle to get the worker to exit.
        drop(result.unwrap());
        // drop the host (owns the send port), to get the host to exit.
        drop(host);
        task.await;
    }

    #[async_test]
    async fn test_launch_worker_remote_unsupported_worker(driver: DefaultDriver) {
        let (host, runner) = super::worker_host();

        // N.B. remote host needs to start first to recv and respond as
        // launch_worker blocks waiting for the response.
        let task = driver.spawn("runner", runner.run(RunnableWorkers1));

        let result = host.launch_worker(TestWorker2::ID, ()).await;

        assert!(result.is_err());
        // drop the target (owns the send port), to get the host to exit.
        drop(host);
        task.await;
    }

    #[async_test]
    async fn test_launch_worker_remote_restart_worker(driver: DefaultDriver) {
        let (host, runner) = super::worker_host();

        // N.B. remote host needs to start first to recv and respond as
        // launch_worker blocks waiting for the response.
        let task = driver.spawn("runner", runner.run(RunnableWorkers1));

        let result = host.launch_worker(TestWorker::ID, Default::default()).await;

        let mut handle = result.expect("worker launch failed");
        handle.restart(&host);
        assert!(matches!(handle.next().await.unwrap(), WorkerEvent::Started));
        handle.stop();
        assert!(matches!(handle.next().await.unwrap(), WorkerEvent::Stopped));

        assert!(handle.next().await.is_none());

        // drop the target (owns the send port), to get the host to exit.
        drop(host);
        task.await;
    }

    struct LocalWorker;

    impl Worker for LocalWorker {
        type Parameters = fn() -> anyhow::Result<()>;
        type State = ();
        const ID: WorkerId<Self::Parameters> = WorkerId::new("local");

        fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
            parameters()?;
            Ok(Self)
        }

        fn restart(_state: Self::State) -> anyhow::Result<Self> {
            unreachable!()
        }

        fn run(self, _recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_test]
    async fn test_launch_local_no_mesh() {
        let mut worker = launch_local_worker::<LocalWorker>(|| Ok(())).await.unwrap();
        worker.join().await.unwrap();
    }
}

/// Internal test support
#[doc(hidden)]
pub mod test_support {
    use std::marker::PhantomData;

    use crate::Worker;
    use crate::WorkerId;
    use crate::WorkerRpc;

    // Worker that always fails. Used for doc tests.
    pub struct DummyWorker<T>(PhantomData<T>);

    pub const DUMMY_WORKER: WorkerId<()> = WorkerId::new("DummyWorker");

    impl<T: 'static + Send> Worker for DummyWorker<T> {
        type Parameters = ();
        type State = ();
        const ID: WorkerId<Self::Parameters> = DUMMY_WORKER;

        fn new(_parameters: Self::Parameters) -> anyhow::Result<Self> {
            Ok(Self(PhantomData))
        }

        fn restart(_state: Self::State) -> anyhow::Result<Self> {
            Ok(Self(PhantomData))
        }

        fn run(self, _recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
            todo!()
        }
    }
}
