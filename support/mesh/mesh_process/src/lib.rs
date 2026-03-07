// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to create a multi-process mesh and spawn child processes
//! within it.

// UNSAFETY: Needed to accept a raw Fd/Handle from our spawning process.
#![expect(unsafe_code)]

use anyhow::Context;
use base64::Engine;
use debug_ptr::DebugPtr;
use futures::FutureExt;
use futures::StreamExt;
use futures::executor::block_on;
use futures_concurrency::future::Race;
use inspect::Inspect;
use inspect::SensitivityLevel;
use mesh::MeshPayload;
use mesh::OneshotReceiver;
use mesh::message::MeshField;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh_remote::InvitationAddress;
#[cfg(unix)]
use pal::unix::process::Builder as ProcessBuilder;
#[cfg(windows)]
use pal::windows::process;
#[cfg(windows)]
use pal::windows::process::Builder as ProcessBuilder;
use pal_async::DefaultPool;
use pal_async::task::Spawn;
use pal_async::task::Task;
use slab::Slab;
use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::File;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::path::PathBuf;
use std::thread;
use tracing::Instrument;
use tracing::instrument;
use unicycle::FuturesUnordered;

#[cfg(windows)]
type IpcNode = mesh_remote::windows::AlpcNode;

#[cfg(unix)]
type IpcNode = mesh_remote::unix::UnixNode;

#[cfg(unix)]
const IPC_FD: i32 = 3;

/// The environment variable for passing the mesh IPC invitation information to
/// a child process. This is passed through the environment instead of a command
/// line argument so that other processes cannot steal the invitation details
/// and use it to break into the mesh.
const INVITATION_ENV_NAME: &str = "MESH_WORKER_INVITATION";

#[derive(Protobuf)]
struct Invitation {
    node_name: String,
    address: InvitationAddress,
    #[cfg(windows)]
    directory_handle: usize,
    #[cfg(unix)]
    socket_fd: i32,
}

static PROCESS_NAME: DebugPtr<String> = DebugPtr::new();

/// Runs a mesh host in the current thread, then exits the process, if this
/// process was launched by [`Mesh::launch_host`].
///
/// The mesh invitation is provided via environment variables. If a mesh
/// invitation is not available this function will return immediately with `Ok`.
/// If a mesh invitation is available, this function joins the mesh and runs the
/// future returned by `f` until `f` returns or the parent process shuts down
/// the mesh.
pub fn try_run_mesh_host<U, F, T>(base_name: &str, f: F) -> anyhow::Result<()>
where
    U: 'static + MeshPayload + Send,
    F: AsyncFnOnce(U) -> anyhow::Result<T>,
{
    block_on(async {
        if let Some(r) = node_from_environment().await? {
            let NodeResult {
                node_name,
                node,
                initial_port,
            } = r;
            PROCESS_NAME.store(&node_name);
            set_program_name(&format!("{base_name}-{node_name}"));
            let init = OneshotReceiver::<InitialMessage<U>>::from(initial_port)
                .await
                .context("failed to receive initial message")?;
            let _drop = (
                f(init.init_message).map(Some),
                handle_host_requests(init.requests).map(|()| None),
            )
                .race()
                .await
                .transpose()?;

            tracing::debug!("waiting to shut down node");
            node.shutdown().await;
            drop(_drop);
            std::process::exit(0);
        }
        Ok(())
    })
}

async fn handle_host_requests(mut recv: mesh::Receiver<HostRequest>) {
    while let Some(req) = recv.next().await {
        match req {
            HostRequest::Inspect(deferred) => {
                deferred.respond(inspect_host);
            }
            HostRequest::Crash => panic!("explicit panic request"),
        }
    }
}

fn set_program_name(name: &str) {
    let _ = name;
    #[cfg(target_os = "linux")]
    {
        let _ = std::fs::write("/proc/self/comm", name);
    }
}

struct NodeResult {
    node_name: String,
    node: IpcNode,
    initial_port: mesh::local_node::Port,
}

/// Create an IPC node from an invitation provided via the process environment.
///
/// Returns `None` if the invitation is not present in the environment.
async fn node_from_environment() -> anyhow::Result<Option<NodeResult>> {
    // return early with no node if the invitation is not present in the environment.
    let invitation_str = match std::env::var(INVITATION_ENV_NAME) {
        Ok(str) => str,
        Err(_) => return Ok(None),
    };

    // Clear the string to avoid leaking the invitation information into child
    // processes.
    //
    // TODO: this function is unsafe because
    // it can cause UB if non-Rust code is concurrently accessing the
    // environment in another thread. To be completely sound,
    // either this function and its callers need to become
    // `unsafe`, or we need to avoid using the environment to propagate the
    // invitation so that we can avoid this call.
    //
    // SAFETY: Seems to work so far.
    unsafe {
        std::env::remove_var(INVITATION_ENV_NAME);
    }

    let invitation: Invitation = mesh::payload::decode(
        &base64::engine::general_purpose::STANDARD
            .decode(invitation_str)
            .context("failed to base64 decode invitation")?,
    )
    .context("failed to protobuf decode invitation")?;

    let (left, right) = mesh::local_node::Port::new_pair();

    let node;
    #[cfg(windows)]
    {
        // SAFETY: trusting the initiating process to pass a valid handle. A
        // malicious process could pass a bad handle here, but a malicious
        // process could also just corrupt our memory arbitrarily, so...
        let directory =
            unsafe { OwnedHandle::from_raw_handle(invitation.directory_handle as RawHandle) };

        let invitation = mesh_remote::windows::AlpcInvitation {
            address: invitation.address,
            directory,
        };

        // join the node w/ the provided invitation and the send port of the channel.
        node = mesh_remote::windows::AlpcNode::join(
            pal_async::windows::TpPool::system(),
            invitation,
            left,
        )
        .context("failed to join mesh")?;
    }

    #[cfg(unix)]
    {
        // SAFETY: trusting the initiating process to pass a valid fd. A
        // malicious process could pass a bad fd here, but a malicious
        // process could also just corrupt our memory arbitrarily, so...
        let fd = unsafe { OwnedFd::from_raw_fd(invitation.socket_fd) };
        let invitation = mesh_remote::unix::UnixInvitation {
            address: invitation.address,
            fd,
        };

        // FUTURE: use pool provided by the caller.
        let (_, driver) = DefaultPool::spawn_on_thread("mesh-worker-pool");
        node = mesh_remote::unix::UnixNode::join(driver, invitation, left)
            .await
            .context("failed to join mesh")?;
    }

    Ok(Some(NodeResult {
        node_name: invitation.node_name,
        node,
        initial_port: right,
    }))
}

/// Represents a mesh::Node with the ability to spawn new processes that can
/// communicate with any other process belonging to the same mesh.
///
/// # Process creation
/// A `Mesh` instance can spawn new processes with an initial communication
/// channel associated with the mesh. All processes originating from the same
/// mesh can potentially communicate and exchange channels with each other.
///
/// Each spawned process can be configured differently via [`ProcessConfig`].
/// Processes are created with [`Mesh::launch_host`].
///
/// ```no_run
/// # use mesh_process::{Mesh, ProcessConfig};
/// # futures::executor::block_on(async {
/// let mesh = Mesh::new("remote_mesh".to_string()).unwrap();
/// let (send, recv) = mesh::channel();
/// mesh.launch_host(ProcessConfig::new("test"), recv).await.unwrap();
/// send.send(String::from("message for new process"));
/// # })
/// ```
#[derive(Inspect)]
pub struct Mesh {
    #[inspect(rename = "name")]
    mesh_name: String,
    #[inspect(flatten, send = "MeshRequest::Inspect")]
    request: mesh::Sender<MeshRequest>,
    #[inspect(skip)]
    task: Task<()>,
}

/// Sandbox profile trait used for mesh hosts.
pub trait SandboxProfile: Send {
    /// Apply executes in the parent context and configures any sandbox
    /// features that will be applied to the newly created process via
    /// the pal builder object.
    fn apply(&mut self, builder: &mut ProcessBuilder<'_>);

    /// Finalize is intended to execute in the child process context after
    /// application specific initialization is complete. It's optional as not
    /// every sandbox profile will need to perform additional sandboxing.
    /// In addition, the child will need to be aware enough to instantiate its
    /// sandbox profile and invoke this method.
    fn finalize(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Configuration for launching a new process in the mesh.
pub struct ProcessConfig {
    name: String,
    process_name: Option<PathBuf>,
    process_args: Vec<OsString>,
    stderr: Option<File>,
    skip_worker_arg: bool,
    sandbox_profile: Option<Box<dyn SandboxProfile + Sync>>,
    env_vars: Vec<(OsString, OsString)>,
}

impl ProcessConfig {
    /// Returns new process configuration using the current process as the
    /// process name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            process_name: None,
            process_args: Vec::new(),
            stderr: None,
            skip_worker_arg: false,
            sandbox_profile: None,
            env_vars: Vec::new(),
        }
    }

    /// Returns a new process configuration using the current process as the
    /// process name.
    pub fn new_with_sandbox(
        name: impl Into<String>,
        sandbox_profile: Box<dyn SandboxProfile + Sync>,
    ) -> Self {
        Self {
            name: name.into(),
            process_name: None,
            process_args: Vec::new(),
            stderr: None,
            skip_worker_arg: false,
            sandbox_profile: Some(sandbox_profile),
            env_vars: Vec::new(),
        }
    }

    /// Sets the process name.
    pub fn process_name(mut self, name: impl Into<PathBuf>) -> Self {
        self.process_name = Some(name.into());
        self
    }

    /// Specifies whether to  appending `<node name>` to the process's command
    /// line.
    ///
    /// This is done by default to make it easier to identify the process in
    /// task lists, but if your process parses the command line then this may
    /// get in the way.
    pub fn skip_worker_arg(mut self, skip: bool) -> Self {
        self.skip_worker_arg = skip;
        self
    }

    /// Adds arguments to the process command line.
    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        self.process_args.extend(args.into_iter().map(|x| x.into()));
        self
    }

    /// Adds environment variables when launching the process.
    pub fn env<I>(mut self, env_vars: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<(OsString, OsString)>,
    {
        self.env_vars.extend(env_vars.into_iter().map(|x| x.into()));
        self
    }

    /// Sets the process's stderr to `file`.
    pub fn stderr(mut self, file: Option<File>) -> Self {
        self.stderr = file;
        self
    }
}

struct MeshInner {
    requests: mesh::Receiver<MeshRequest>,
    hosts: Slab<MeshHostInner>,
    /// Handles for spawned host processes.
    waiters: FuturesUnordered<OneshotReceiver<usize>>,
    /// Mesh node for host process communication.
    node: IpcNode,
    /// Name for this mesh instance, used for tracing/debugging.
    mesh_name: String,
    /// Job object. When closed, it will terminate all the child processes. This
    /// is used to ensure the child processes don't outlive the parent.
    #[cfg(windows)]
    job: pal::windows::job::Job,
}

struct MeshHostInner {
    name: String,
    pid: i32,
    node_id: mesh::NodeId,
    send: mesh::Sender<HostRequest>,
}

enum MeshRequest {
    NewHost(Rpc<NewHostParams, anyhow::Result<()>>),
    Inspect(inspect::Deferred),
    Crash(i32),
}

struct NewHostParams {
    config: ProcessConfig,
    recv: mesh::local_node::Port,
    request_send: mesh::Sender<HostRequest>,
}

impl Mesh {
    /// Creates a new mesh with the given name.
    pub fn new(mesh_name: String) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let job = {
            let job = pal::windows::job::Job::new().context("failed to create job object")?;
            job.set_terminate_on_close()
                .context("failed to set job object terminate on close")?;
            job
        };

        #[cfg(windows)]
        let node = mesh_remote::windows::AlpcNode::new(pal_async::windows::TpPool::system())
            .context("AlpcNode creation failure")?;
        #[cfg(unix)]
        let node = {
            // FUTURE: use pool provided by the caller.
            let (_, driver) = DefaultPool::spawn_on_thread("mesh-worker-pool");
            mesh_remote::unix::UnixNode::new(driver)
        };

        let (request, requests) = mesh::channel();
        let mut inner = MeshInner {
            requests,
            hosts: Default::default(),
            waiters: Default::default(),
            node,
            mesh_name: mesh_name.clone(),
            #[cfg(windows)]
            job,
        };

        // Spawn a separate thread for launching mesh processes to avoid bad
        // interactions with any other pools.
        let (_, driver) = DefaultPool::spawn_on_thread("mesh");
        let task = driver.spawn(
            format!("mesh-{}", &mesh_name),
            async move { inner.run().await },
        );

        Ok(Self {
            request,
            mesh_name,
            task,
        })
    }

    /// Spawns a new host in the mesh with the provided configuration and
    /// initial message.
    ///
    /// The initial message will be provided to the closure passed to
    /// [`try_run_mesh_host()`].
    pub async fn launch_host<T: 'static + MeshField + Send>(
        &self,
        config: ProcessConfig,
        initial_message: T,
    ) -> anyhow::Result<()> {
        let (request_send, request_recv) = mesh::channel();

        let (init_send, init_recv) = mesh::oneshot::<InitialMessage<T>>();
        init_send.send(InitialMessage {
            requests: request_recv,
            init_message: initial_message,
        });

        self.request
            .call(
                MeshRequest::NewHost,
                NewHostParams {
                    config,
                    recv: init_recv.into(),
                    request_send,
                },
            )
            .await
            .context("mesh failed")?
    }

    /// Shutdown the mesh and wait for any spawned processes to exit.
    ///
    /// The `Mesh` instance is no longer usable after `shutdown`.
    pub async fn shutdown(self) {
        let span = tracing::span!(
            tracing::Level::INFO,
            "mesh_shutdown",
            name = self.mesh_name.as_str(),
        );

        async {
            drop(self.request);
            self.task.await;
        }
        .instrument(span)
        .await;
    }

    /// Crashes the child process with the given process ID.
    pub fn crash(&self, pid: i32) {
        self.request.send(MeshRequest::Crash(pid));
    }
}

#[derive(MeshPayload)]
struct InitialMessage<T> {
    requests: mesh::Receiver<HostRequest>,
    init_message: T,
}

#[derive(Debug, MeshPayload)]
enum HostRequest {
    #[mesh(transparent)]
    Inspect(inspect::Deferred),
    Crash,
}

fn inspect_host(resp: &mut inspect::Response<'_>) {
    resp.field("tasks", inspect_task::inspect_task_list());
}

#[derive(Inspect)]
struct HostInspect<'a> {
    #[inspect(safe)]
    name: &'a str,
    #[inspect(debug, safe)]
    node_id: mesh::NodeId,
    #[cfg(target_os = "linux")]
    #[inspect(safe)]
    rlimit: inspect_rlimit::InspectRlimit,
}

impl MeshInner {
    async fn run(&mut self) {
        enum Event {
            Request(MeshRequest),
            Done(usize),
        }

        loop {
            let event = futures::select! { // merge semantics
                request = self.requests.select_next_some() => Event::Request(request),
                n = self.waiters.select_next_some() => Event::Done(n.unwrap()),
                complete => break,
            };

            match event {
                Event::Request(request) => match request {
                    MeshRequest::NewHost(rpc) => {
                        rpc.handle(async |params| self.spawn_process(params).await)
                            .await
                    }
                    MeshRequest::Inspect(deferred) => {
                        deferred.respond(|resp| {
                            resp.sensitivity_child("hosts", SensitivityLevel::Safe, |req| {
                                let mut resp = req.respond();
                                for host in self.hosts.iter().map(|(_, host)| host) {
                                    resp.sensitivity_field_mut(
                                        &host.pid.to_string(),
                                        SensitivityLevel::Safe,
                                        &mut inspect::adhoc(|req| {
                                            req.respond()
                                                .merge(&HostInspect {
                                                    name: &host.name,
                                                    node_id: host.node_id,
                                                    #[cfg(target_os = "linux")]
                                                    rlimit: inspect_rlimit::InspectRlimit::for_pid(
                                                        host.pid,
                                                    ),
                                                })
                                                .merge(inspect::send(
                                                    &host.send,
                                                    HostRequest::Inspect,
                                                ));
                                        }),
                                    );
                                }
                            })
                            .sensitivity_field_mut(
                                &format!("hosts/{}", std::process::id()),
                                SensitivityLevel::Safe,
                                &mut inspect::adhoc(|req| {
                                    let mut resp = req.respond();
                                    resp.merge(&HostInspect {
                                        name: &self.mesh_name,
                                        node_id: self.node.id(),
                                        #[cfg(target_os = "linux")]
                                        rlimit: inspect_rlimit::InspectRlimit::new(),
                                    });
                                    inspect_host(&mut resp);
                                }),
                            );
                        });
                    }
                    MeshRequest::Crash(pid) => {
                        if pid == std::process::id() as i32 {
                            panic!("explicit panic request");
                        }

                        let mut found = false;
                        for (_, host) in &self.hosts {
                            if host.pid == pid {
                                host.send.send(HostRequest::Crash);
                                found = true;
                                break;
                            }
                        }

                        if !found {
                            tracing::error!("failed to crash process, pid {pid} not found");
                        }
                    }
                },
                Event::Done(id) => {
                    self.hosts.remove(id);
                }
            }
        }
    }

    /// Spawns a new process with a mesh channel associated with this `Mesh` instance.
    #[instrument(name = "mesh_spawn_process", skip(self, params), fields(mesh_name = self.mesh_name.as_str(), pid = tracing::field::Empty))]
    async fn spawn_process(&mut self, params: NewHostParams) -> anyhow::Result<()> {
        let NewHostParams {
            config,
            recv,
            request_send,
        } = params;

        let pid;
        let node_id;

        // If no process name was passed, use the current executable path to
        // ensure we get the right file, but set arg0 to match how this process
        // was launched.
        let (arg0, process_name) = if let Some(n) = &config.process_name {
            (None, Cow::Borrowed(n))
        } else {
            (
                std::env::args_os().next(),
                Cow::Owned(std::env::current_exe().context("failed to get current exe path")?),
            )
        };

        let name = config.name.clone();

        #[cfg(windows)]
        let wait = {
            let (invitation, handle) = self.node.invite(recv).context("mesh node invite error")?;
            node_id = invitation.address.local_addr.node;

            let invitation_env = base64::engine::general_purpose::STANDARD.encode(
                mesh::payload::encode(Invitation {
                    node_name: name.clone(),
                    address: invitation.address,
                    directory_handle: invitation.directory.as_raw_handle() as usize,
                }),
            );

            let mut args = config.process_args;
            if !config.skip_worker_arg {
                args.push(name.clone().into());
            }

            let mut builder = process::Builder::from_args(
                arg0.as_ref()
                    .map_or_else(|| process_name.as_os_str(), |x| x.as_os_str()),
                &args,
            );
            if arg0.is_some() {
                builder.application_name(process_name.as_path());
            }
            builder
                .stdin(process::Stdio::Null)
                .stdout(process::Stdio::Null)
                .handle(&invitation.directory)
                .env(INVITATION_ENV_NAME, invitation_env)
                .extend_env(config.env_vars)
                .job(self.job.as_handle());

            if let Some(log_file) = config.stderr.as_ref() {
                builder.stderr(process::Stdio::Handle(log_file.as_handle()));
            }

            if let Some(mut sandbox_profile) = config.sandbox_profile {
                sandbox_profile.apply(&mut builder);
            }

            let child = builder.spawn().context("failed to launch mesh process")?;
            // Wait for the child to connect to the mesh. TODO: timeout
            handle.await;
            pid = child.id() as i32;
            tracing::Span::current().record("pid", pid);
            move || {
                child.wait();
                let code = child.exit_code();
                if code == 0 {
                    tracing::info!(pid, name = name.as_str(), "mesh child exited successfully");
                } else {
                    tracing::error!(pid, name = name.as_str(), code, "mesh child abnormal exit");
                }
            }
        };
        #[cfg(unix)]
        let mut wait = {
            use pal::unix::process;

            let invitation = self
                .node
                .invite(recv)
                .await
                .context("mesh node invite error")?;

            node_id = invitation.address.local_addr.node;

            let invitation_env = base64::engine::general_purpose::STANDARD.encode(
                mesh::payload::encode(Invitation {
                    node_name: name.clone(),
                    address: invitation.address,
                    socket_fd: IPC_FD,
                }),
            );

            let mut command = process::Builder::new(process_name.into_owned());
            if let Some(arg0) = arg0 {
                command.arg0(arg0);
            }
            command
                .args(&config.process_args)
                .stdin(process::Stdio::Null)
                .stdout(process::Stdio::Null)
                .dup_fd(invitation.fd.as_fd(), IPC_FD)
                .env(INVITATION_ENV_NAME, invitation_env);

            if !config.skip_worker_arg {
                command.arg(&name);
            }

            if let Some(log_file) = config.stderr.as_ref() {
                command.stderr(process::Stdio::Fd(log_file.as_fd()));
            }

            if let Some(mut sandbox_profile) = config.sandbox_profile {
                sandbox_profile.apply(&mut command);
            }

            let mut child = command.spawn().context("failed to launch mesh process")?;
            pid = child.id();
            tracing::Span::current().record("pid", pid);
            move || {
                let exit_status = child.wait().expect("mesh child wait failure");
                if let Some(0) = exit_status.code() {
                    tracing::info!(pid, name = name.as_str(), "mesh child exited successfully");
                } else {
                    tracing::error!(
                        pid,
                        name = name.as_str(),
                        %exit_status,
                        "mesh child abnormal exit"
                    );
                }
            }
        };

        let (wait_send, wait_recv) = mesh::oneshot();

        let id = self.hosts.insert(MeshHostInner {
            name: config.name,
            pid,
            node_id,
            send: request_send,
        });

        thread::Builder::new()
            .name(format!("wait-mesh-child-{}", pid))
            .spawn(move || {
                wait();
                wait_send.send(id);
            })
            .unwrap();

        self.waiters.push(wait_recv);
        Ok(())
    }
}
