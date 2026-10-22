// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions and types for running a mesh for OpenVMM and launching workers
//! within it.

use anyhow::Context;
use inspect::Inspect;
use mesh_process::Mesh;
use mesh_process::ProcessConfig;
use mesh_process::try_run_mesh_host;
use mesh_worker::RegisteredWorkers;
use mesh_worker::WorkerHost;
use openvmm_defs::entrypoint::MeshHostParams;
use pal_async::task::Spawn;
use pal_async::task::Task;
#[cfg(target_os = "linux")]
use pal_tracer::TraceConfig;
use sandbox::profiles::SandboxRole;
use std::path::PathBuf;

const SANDBOX_ROLE_ARG: &str = "--openvmm-sandbox-role=";

pub(crate) fn run_vmm_mesh_host() -> anyhow::Result<()> {
    try_run_mesh_host("openvmm", async |params: MeshHostParams| {
        params.runner.run(RegisteredWorkers).await;
        Ok(())
    })
}

pub(crate) fn apply_vmm_mesh_host_sandbox() -> anyhow::Result<()> {
    if let Some(role) = sandbox_role_from_args()? {
        sandbox::apply(&role.profile()).context("failed to apply worker host sandbox")?;
    }
    Ok(())
}

fn sandbox_role_from_args() -> anyhow::Result<Option<SandboxRole>> {
    let mut role = None;
    for arg in std::env::args() {
        let Some(value) = arg.strip_prefix(SANDBOX_ROLE_ARG) else {
            continue;
        };
        let parsed = match value {
            "vm" => SandboxRole::Vm,
            "tpm" => SandboxRole::Tpm,
            _ => anyhow::bail!("unknown OpenVMM sandbox role `{value}`"),
        };
        if role.replace(parsed).is_some() {
            anyhow::bail!("OpenVMM sandbox role specified more than once");
        }
    }
    Ok(role)
}

#[derive(Inspect)]
pub(crate) struct VmmMesh {
    #[inspect(flatten)]
    mesh: Option<Mesh>,
    #[inspect(skip)]
    local_host: WorkerHost,
    #[inspect(skip)]
    _task: Task<()>,
    #[cfg(target_os = "linux")]
    #[inspect(skip)]
    worker_trace: Option<TraceConfig>,
}

impl VmmMesh {
    pub fn new(
        spawn: &impl Spawn,
        single_process: bool,
        #[cfg(target_os = "linux")] worker_trace_dir: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            !single_process || worker_trace_dir.is_none(),
            "worker tracing requires separate worker processes"
        );
        let mesh = if single_process {
            None
        } else {
            Some(Mesh::new("openvmm".to_string())?)
        };
        let (local_host, runner) = mesh_worker::worker_host();
        let task = spawn.spawn("worker-host", runner.run(RegisteredWorkers));
        #[cfg(target_os = "linux")]
        let worker_trace = worker_trace_dir
            .map(|output_dir| {
                let deny_syscalls = sandbox::platform_syscall_denylist()
                    .map(|entry| entry.name.to_string())
                    .collect::<Vec<_>>();
                let syscalls =
                    pal_tracer::resolve_trace_syscalls(&deny_syscalls, sandbox::nr_for_name)?;
                Ok::<_, anyhow::Error>(TraceConfig::new(output_dir, "worker", syscalls))
            })
            .transpose()?;
        Ok(Self {
            mesh,
            local_host,
            _task: task,
            #[cfg(target_os = "linux")]
            worker_trace,
        })
    }

    pub async fn make_host(
        &self,
        name: impl Into<String>,
        log_file: Option<PathBuf>,
    ) -> anyhow::Result<WorkerHost> {
        self.make_host_inner(name.into(), log_file, None).await
    }

    pub async fn make_sandboxed_host(
        &self,
        role: SandboxRole,
        log_file: Option<PathBuf>,
    ) -> anyhow::Result<WorkerHost> {
        self.make_host_inner(role.name().to_string(), log_file, Some(role))
            .await
    }

    async fn make_host_inner(
        &self,
        name: String,
        log_file: Option<PathBuf>,
        sandbox_role: Option<SandboxRole>,
    ) -> anyhow::Result<WorkerHost> {
        #[cfg(not(target_os = "linux"))]
        if sandbox_role.is_some() {
            return Err(sandbox::Error::UnsupportedPlatform.into());
        }

        let log_file: Option<std::fs::File> = if let Some(file) = &log_file {
            Some(
                std::fs::File::create(file)
                    .with_context(|| format!("failed to create log file {}", file.display()))?,
            )
        } else {
            None
        };

        let host = if let Some(mesh) = &self.mesh {
            let (host, runner) = mesh_worker::worker_host();
            #[cfg(target_os = "linux")]
            let process_config = match sandbox_role {
                Some(role) => ProcessConfig::new_with_sandbox(role.name(), role.profile())
                    .args([format!("{SANDBOX_ROLE_ARG}{}", role.name())])
                    .stderr(log_file),
                None => ProcessConfig::new(name).stderr(log_file),
            };
            #[cfg(target_os = "linux")]
            let process_config = if let Some(trace) = &self.worker_trace {
                process_config.trace(trace.clone())
            } else {
                process_config
            };
            #[cfg(not(target_os = "linux"))]
            let process_config = ProcessConfig::new(name).stderr(log_file);
            mesh.launch_host(process_config, MeshHostParams { runner })
                .await?;
            host
        } else {
            self.local_host.clone()
        };
        Ok(host)
    }

    pub async fn shutdown(self) {
        if let Some(mesh) = self.mesh {
            mesh.shutdown().await;
        }
    }
}
