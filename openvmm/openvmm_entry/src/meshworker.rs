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
use std::ffi::OsString;
use std::path::PathBuf;

const SANDBOX_ROLE_ARG: &str = "--openvmm-sandbox-role=";

pub(crate) fn run_vmm_mesh_host() -> anyhow::Result<()> {
    try_run_mesh_host("openvmm", async |params: MeshHostParams| {
        params.runner.run(RegisteredWorkers).await;
        Ok(())
    })
}

pub(crate) fn apply_vmm_mesh_host_sandbox() -> anyhow::Result<()> {
    if let Some(role) = sandbox_role_from_args(std::env::args_os())? {
        sandbox::apply(&role.profile()).context("failed to apply worker host sandbox")?;
    }
    Ok(())
}

fn sandbox_role_from_args(
    args: impl IntoIterator<Item = OsString>,
) -> anyhow::Result<Option<SandboxRole>> {
    let mut role = None;
    for arg in args {
        let Some(value) = arg
            .as_encoded_bytes()
            .strip_prefix(SANDBOX_ROLE_ARG.as_bytes())
        else {
            continue;
        };
        let parsed = match value {
            b"vm" => SandboxRole::Vm,
            b"tpm" => SandboxRole::Tpm,
            _ => anyhow::bail!(
                "unknown OpenVMM sandbox role `{}`",
                String::from_utf8_lossy(value),
            ),
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
        let _ = sandbox_role;

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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    #[cfg(unix)]
    fn sandbox_role_ignores_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(b"/tmp/kernel-\xff".to_vec());
        for expected in [None, Some("vm"), Some("tpm")] {
            let mut args = vec![OsString::from("openvmm")];
            if let Some(role) = expected {
                args.push(format!("{SANDBOX_ROLE_ARG}{role}").into());
            }
            args.extend(["--kernel".into(), path.clone()]);
            assert_eq!(
                sandbox_role_from_args(args)
                    .unwrap()
                    .map(|role| role.name()),
                expected,
            );
        }

        let mut invalid_role = SANDBOX_ROLE_ARG.as_bytes().to_vec();
        invalid_role.push(0xff);
        assert!(sandbox_role_from_args([OsString::from_vec(invalid_role)]).is_err());
    }

    #[test]
    fn sandbox_role_rejects_unknown_and_duplicate_roles() {
        for roles in [vec!["unknown"], vec!["vm", "vm"], vec!["vm", "tpm"]] {
            let args = roles
                .into_iter()
                .map(|role| OsString::from(format!("{SANDBOX_ROLE_ARG}{role}")));
            assert!(sandbox_role_from_args(args).is_err());
        }
    }
}
