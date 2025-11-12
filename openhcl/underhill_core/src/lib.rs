// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the interactive control process and the entry point
//! for the underhill environment.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod diag;
mod dispatch;
mod emuplat;
mod get_tracing;
mod inspect_internal;
mod inspect_proc;
mod livedump;
mod loader;
mod nvme_manager;
mod options;
mod reference_time;
mod servicing;
mod threadpool_vm_task_backend;
mod vmbus_relay_unit;
mod vmgs_logger;
mod vp;
mod vpci;
mod worker;
mod wrapped_partition;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use options::Options;

use crate::diag::DiagWorker;
use crate::dispatch::UhVmRpc;
use crate::worker::UnderhillEnvCfg;
use crate::worker::UnderhillRemoteConsoleCfg;
use crate::worker::UnderhillVmWorker;
use crate::worker::UnderhillWorkerParameters;
use anyhow::Context;
use bootloader_fdt_parser::BootTimes;
use cvm_tracing::CVM_ALLOWED;
use framebuffer::FRAMEBUFFER_SIZE;
use framebuffer::FramebufferAccess;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use get_tracing::init_tracing;
use get_tracing::init_tracing_backend;
use inspect::Inspect;
use inspect::SensitivityLevel;
use mesh::CancelContext;
use mesh::CancelReason;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh_process::Mesh;
use mesh_process::ProcessConfig;
use mesh_process::try_run_mesh_host;
use mesh_tracing::RemoteTracer;
use mesh_tracing::TracingBackend;
use mesh_worker::RegisteredWorkers;
use mesh_worker::WorkerEvent;
use mesh_worker::WorkerHandle;
use mesh_worker::WorkerHost;
use mesh_worker::WorkerHostRunner;
use mesh_worker::launch_local_worker;
use mesh_worker::register_workers;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::task::Spawn;
#[cfg(feature = "profiler")]
use profiler_worker::ProfilerWorker;
#[cfg(feature = "profiler")]
use profiler_worker::ProfilerWorkerParameters;
use std::time::Duration;
use vmsocket::VmAddress;
use vmsocket::VmListener;
use vnc_worker_defs::VncParameters;

fn new_underhill_remote_console_cfg(
    framebuffer_gpa_base: Option<u64>,
) -> anyhow::Result<(UnderhillRemoteConsoleCfg, Option<FramebufferAccess>)> {
    if let Some(framebuffer_gpa_base) = framebuffer_gpa_base {
        // Underhill accesses the framebuffer by using /dev/mshv_vtl_low to read
        // from a second mapping placed after the end of RAM at a static
        // location specified by the host.
        //
        // Open the file directly rather than use the `hcl` crate to avoid
        // leaking `hcl` stuff into this crate.
        //
        // FUTURE: use an approach that doesn't require double mapping the
        // framebuffer from the host.
        let gpa_fd = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mshv_vtl_low")
            .context("failed to open gpa device")?;

        let vram = sparse_mmap::new_mappable_from_file(gpa_fd.file(), true, false)?;
        let (fb, fba) = framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, framebuffer_gpa_base)
            .context("allocating framebuffer")?;
        tracing::debug!("framebuffer_gpa_base: {:#x}", framebuffer_gpa_base);

        Ok((
            UnderhillRemoteConsoleCfg {
                synth_keyboard: true,
                synth_mouse: true,
                synth_video: true,
                input: mesh::Receiver::new(),
                framebuffer: Some(fb),
            },
            Some(fba),
        ))
    } else {
        Ok((
            UnderhillRemoteConsoleCfg {
                synth_keyboard: false,
                synth_mouse: false,
                synth_video: false,
                input: mesh::Receiver::new(),
                framebuffer: None,
            },
            None,
        ))
    }
}

pub fn main() -> anyhow::Result<()> {
    // Install a panic hook to prefix the current async task name before the
    // standard panic output.
    install_task_name_panic_hook();

    if let Some(path) = std::env::var_os("OPENVMM_WRITE_SAVED_STATE_PROTO") {
        if cfg!(debug_assertions) {
            mesh::payload::protofile::DescriptorWriter::new(
                vmcore::save_restore::saved_state_roots(),
            )
            .write_to_path(path)
            .context("failed to write protobuf descriptors")?;
            return Ok(());
        } else {
            // The generated code for this is too large for release builds.
            anyhow::bail!(".proto output only supported in debug builds");
        }
    }

    // FUTURE: create and use the affinitized threadpool here.
    let (_, tracing_driver) = DefaultPool::spawn_on_thread("tracing");

    // Try to run as a worker host, sending a remote tracer that will forward
    // tracing events back to the initial process for logging to the host. See
    // [`get_tracing`] doc comments for more details.
    //
    // On success the worker runs to completion and then exits the process (does
    // not return). Any worker host setup errors are return and bubbled up.
    try_run_mesh_host("underhill", {
        let tracing_driver = tracing_driver.clone();
        async |params: MeshHostParams| {
            if let Some(remote_tracer) = params.tracer {
                init_tracing(tracing_driver, remote_tracer).context("failed to init tracing")?;
            }
            params.runner.run(RegisteredWorkers).await;
            Ok(())
        }
    })?;

    // Initialize the tracing backend used by this and all subprocesses.
    let mut tracing = init_tracing_backend(tracing_driver.clone())?;
    // Initialize tracing from the backend.
    init_tracing(tracing_driver, tracing.tracer()).context("failed to init tracing")?;
    DefaultPool::run_with(|driver| do_main(driver, tracing))
}

fn install_task_name_panic_hook() {
    use std::io::Write;

    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        pal_async::task::with_current_task_metadata(|metadata| {
            if let Some(metadata) = metadata {
                let _ = write!(std::io::stderr(), "task '{}', ", metadata.name());
            }
        });
        // This will proceed with writing "thread ... panicked at ..."
        panic_hook(info);
    }));
}

async fn do_main(driver: DefaultDriver, mut tracing: TracingBackend) -> anyhow::Result<()> {
    let opt = Options::parse(Vec::new(), Vec::new())?;

    let crate_name = build_info::get().crate_name();
    let crate_revision = build_info::get().scm_revision();
    tracing::info!(CVM_ALLOWED, ?crate_name, ?crate_revision, "VMM process");
    log_boot_times().context("failure logging boot times")?;

    // Write the current pid to a file, with rich diagnostics to help investigate ENOSPC / write failures.
    //
    // Make diagnostics unconditional by choosing a default path if none provided via env/CLI.
    {
        use std::path::PathBuf;

        let pid_path: PathBuf = opt
            .pid
            .clone()
            .unwrap_or_else(|| PathBuf::from("/run/underhill.pid"));

        let pid_path_source = if opt.pid.is_some() {
            "provided"
        } else {
            "default_fallback"
        };

        tracing::info!(
            CVM_ALLOWED,
            pid_path=?pid_path,
            source=%pid_path_source,
            "PID_DIAG: preparing pid file diagnostics"
        );

        if let Some(parent) = pid_path.parent() {
            match std::fs::metadata(parent) {
                Ok(meta) => {
                    tracing::debug!(
                        CVM_ALLOWED,
                        parent=?parent,
                        is_dir=meta.is_dir(),
                        "PID_DIAG: parent metadata ok"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        CVM_ALLOWED,
                        parent=?parent,
                        error=%e,
                        "PID_DIAG: parent metadata error"
                    );
                }
            }
        }

        // Detailed /proc/meminfo parse (selected fields).
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            let mut mem_kv = std::collections::HashMap::new();
            for line in meminfo.lines().take(40) {
                if let Some((k, v)) = line.split_once(':') {
                    mem_kv.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            let summary = [
                "MemTotal",
                "MemFree",
                "Buffers",
                "Cached",
                "SwapTotal",
                "SwapFree",
            ]
            .iter()
            .map(|k| format!("{}={}", k, mem_kv.get(*k).unwrap_or(&"?".to_string())))
            .collect::<Vec<_>>()
            .join(" ");
            tracing::debug!(CVM_ALLOWED, meminfo_summary=%summary, "PID_DIAG: meminfo summary");
        }

        // Inode diagnostics: capture global inode stats to help confirm inode exhaustion.
        if let (Ok(inode_nr), Ok(inode_state)) = (
            std::fs::read_to_string("/proc/sys/fs/inode-nr"),
            std::fs::read_to_string("/proc/sys/fs/inode-state"),
        ) {
            let inode_nr = inode_nr.trim().to_string();
            let inode_state = inode_state.trim().to_string();
            tracing::debug!(
                CVM_ALLOWED,
                inode_nr=%inode_nr,
                inode_state=%inode_state,
                "PID_DIAG: inode global stats"
            );
        }

        // Root directory sample (capped) to approximate breadth of rootfs unpack contents.
        if let Ok(root_iter) = std::fs::read_dir("/") {
            let mut total = 0usize;
            let mut dirs = 0usize;
            let mut files = 0usize;
            let mut others = 0usize;
            for entry in root_iter.flatten().take(256) {
                total += 1;
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        dirs += 1;
                    } else if ft.is_file() {
                        files += 1;
                    } else {
                        others += 1;
                    }
                }
            }
            tracing::debug!(
                CVM_ALLOWED,
                root_sample_total = total,
                root_sample_dirs = dirs,
                root_sample_files = files,
                root_sample_other = others,
                "PID_DIAG: root directory sample (max 256 entries)"
            );
        }

        // Mount lines (root and /run).
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            let root_line = mounts
                .lines()
                .find(|l| l.split_whitespace().nth(1) == Some("/"));
            let run_line = mounts
                .lines()
                .find(|l| l.split_whitespace().nth(1) == Some("/run"));

            let root_fs_type = root_line
                .and_then(|l| l.split_whitespace().nth(2))
                .unwrap_or("?");
            let run_fs_type = run_line
                .and_then(|l| l.split_whitespace().nth(2))
                .unwrap_or("?");

            tracing::debug!(
                CVM_ALLOWED,
                root_mount=?root_line,
                run_mount=?run_line,
                root_fs_type=%root_fs_type,
                run_fs_type=%run_fs_type,
                "PID_DIAG: /proc/mounts subset"
            );
        }

        // Enumerate parent directory entries (sample) & mountinfo subset.
        if let Some(parent) = pid_path.parent() {
            if let Ok(read_dir) = std::fs::read_dir(parent) {
                let mut file_count = 0usize;
                let mut dir_count = 0usize;
                let mut other_count = 0usize;
                let mut sample_names = Vec::new();
                for entry in read_dir.flatten().take(64) {
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_dir() {
                            dir_count += 1;
                        } else if ft.is_file() {
                            file_count += 1;
                        } else {
                            other_count += 1;
                        }
                    }
                    if let Some(name) = entry.file_name().to_str() {
                        sample_names.push(name.to_string());
                    }
                }
                tracing::debug!(
                    CVM_ALLOWED,
                    path=?parent,
                    files=file_count,
                    dirs=dir_count,
                    other=other_count,
                    sample=?sample_names,
                    "PID_DIAG: parent directory sample (max 64 entries)"
                );
            }

            if let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") {
                let root_mount = mountinfo
                    .lines()
                    .find(|l| l.split_whitespace().any(|f| f == "/"));
                let run_mount = mountinfo
                    .lines()
                    .find(|l| l.split_whitespace().any(|f| f == "/run"));
                tracing::debug!(
                    CVM_ALLOWED,
                    root_mount=?root_mount,
                    run_mount=?run_mount,
                    "PID_DIAG: mountinfo subset"
                );
            }
        }

        // /proc/self/status summary (VmSize / VmRSS).
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            let mut vm_size = "?";
            let mut vm_rss = "?";
            for line in status.lines() {
                if line.starts_with("VmSize:") {
                    vm_size = line.split_whitespace().nth(1).unwrap_or("?");
                }
                if line.starts_with("VmRSS:") {
                    vm_rss = line.split_whitespace().nth(1).unwrap_or("?");
                }
            }
            tracing::debug!(CVM_ALLOWED, vm_size=%vm_size, vm_rss=%vm_rss, "PID_DIAG: process memory");
        }

        let pid_str = std::process::id().to_string();

        match std::fs::write(&pid_path, &pid_str) {
            Ok(_) => {
                tracing::info!(
                    CVM_ALLOWED,
                    pid=?pid_str,
                    pid_path=?pid_path,
                    source=%pid_path_source,
                    "PID_DIAG: pid file written"
                );
            }
            Err(e) => {
                let raw = e.raw_os_error();
                let enospc = raw == Some(28); // ENOSPC
                let kind = e.kind();
                let probe_outcome = pid_path.parent().map(|p| {
                    let probe = p.join("underhill_probe.tmp");
                    match std::fs::write(&probe, b"probe") {
                        Ok(_) => {
                            let _ = std::fs::remove_file(&probe);
                            "probe_ok"
                        }
                        Err(pe) => {
                            if pe.raw_os_error() == Some(28) {
                                "probe_failed_enospc"
                            } else {
                                "probe_failed_other"
                            }
                        }
                    }
                });
                let post_memfree = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
                    m.lines()
                        .find(|l| l.starts_with("MemFree:"))
                        .map(|l| l.to_string())
                });
                // Multi-file probe to further differentiate inode exhaustion vs byte-size exhaustion.
                let multi_probe_outcome = pid_path.parent().map(|p| {
                    let mut created = 0usize;
                    let mut enospc_failures = 0usize;
                    for i in 0..8 {
                        let probe = p.join(format!("underhill_probe_{i}.tmp"));
                        match std::fs::write(&probe, b"probe") {
                            Ok(_) => {
                                created += 1;
                                let _ = std::fs::remove_file(&probe);
                            }
                            Err(pe) => {
                                if pe.raw_os_error() == Some(28) {
                                    enospc_failures += 1;
                                }
                            }
                        }
                    }
                    format!("multi_created={created} enospc_failures={enospc_failures}")
                });

                // Capture inode stats again post-failure.
                let inode_nr_now = std::fs::read_to_string("/proc/sys/fs/inode-nr").ok();
                let inode_state_now = std::fs::read_to_string("/proc/sys/fs/inode-state").ok();

                tracing::error!(
                    CVM_ALLOWED,
                    pid_path=?pid_path,
                    source=%pid_path_source,
                    error=%e,
                    kind=?kind,
                    raw_os_error=?raw,
                    enospc=?enospc,
                    probe=?probe_outcome,
                    multi_probe=?multi_probe_outcome,
                    post_memfree=?post_memfree,
                    inode_nr=?inode_nr_now,
                    inode_state=?inode_state_now,
                    "PID_DIAG: failed to write pid file"
                );
                return Err(anyhow::anyhow!(e)
                    .context(format!("failed to write pid to {}", pid_path.display())));
            }
        }
    }

    let mesh = Mesh::new("underhill".to_string()).context("failed to create mesh")?;

    let r = run_control(driver, &mesh, opt, &mut tracing).await;
    if let Err(err) = &r {
        tracing::error!(
            CVM_ALLOWED,
            error = err.as_ref() as &dyn std::error::Error,
            "VM failure"
        );
    }

    // Wait a few seconds for child processes to terminate and tracing to finish.
    CancelContext::new()
        .with_timeout(Duration::from_secs(10))
        .until_cancelled(async {
            mesh.shutdown().await;
            tracing.shutdown().await;
        })
        .await
        .ok();

    r
}

fn log_boot_times() -> anyhow::Result<()> {
    fn diff(start: Option<u64>, end: Option<u64>) -> Option<tracing::field::DebugValue<Duration>> {
        use reference_time::ReferenceTime;
        Some(tracing::field::debug(
            ReferenceTime::new(end?).since(ReferenceTime::new(start?))?,
        ))
    }

    // Read boot times provided by the bootloader.
    let BootTimes {
        start,
        end,
        sidecar_start,
        sidecar_end,
    } = BootTimes::new().context("failed to parse boot times")?;
    tracing::info!(
        CVM_ALLOWED,
        start,
        end,
        sidecar_start,
        sidecar_end,
        elapsed = diff(start, end),
        sidecar_elapsed = diff(sidecar_start, sidecar_end),
        "boot loader times"
    );
    Ok(())
}

struct DiagState {
    _worker: WorkerHandle,
    request_recv: mesh::Receiver<diag_server::DiagRequest>,
}

impl DiagState {
    async fn new() -> anyhow::Result<Self> {
        // Start the diagnostics worker immediately.
        let (request_send, request_recv) = mesh::channel();
        let worker = launch_local_worker::<DiagWorker>(diag::DiagWorkerParameters { request_send })
            .await
            .context("failed to launch diagnostics worker")?;
        Ok(Self {
            _worker: worker,
            request_recv,
        })
    }
}

#[derive(Inspect)]
struct Workers {
    vm: WorkerHandle,
    #[inspect(skip)]
    vm_rpc: mesh::Sender<UhVmRpc>,
    vnc: Option<WorkerHandle>,
    #[cfg(feature = "gdb")]
    gdb: Option<WorkerHandle>,
}

#[derive(MeshPayload)]
struct MeshHostParams {
    tracer: Option<RemoteTracer>,
    runner: WorkerHostRunner,
}

async fn launch_mesh_host(
    mesh: &Mesh,
    name: &str,
    tracer: Option<RemoteTracer>,
) -> anyhow::Result<WorkerHost> {
    let (host, runner) = mesh_worker::worker_host();
    mesh.launch_host(ProcessConfig::new(name), MeshHostParams { tracer, runner })
        .await?;
    Ok(host)
}

async fn launch_workers(
    mesh: &Mesh,
    tracing: &mut TracingBackend,
    control_send: mesh::Sender<ControlRequest>,
    opt: Options,
) -> anyhow::Result<Workers> {
    let env_cfg = UnderhillEnvCfg {
        vmbus_max_version: opt.vmbus_max_version,
        vmbus_enable_mnf: opt.vmbus_enable_mnf,
        vmbus_force_confidential_external_memory: opt.vmbus_force_confidential_external_memory,
        vmbus_channel_unstick_delay: (opt.vmbus_channel_unstick_delay_ms != 0)
            .then(|| Duration::from_millis(opt.vmbus_channel_unstick_delay_ms)),
        cmdline_append: opt.cmdline_append.clone(),
        reformat_vmgs: opt.reformat_vmgs,
        vtl0_starts_paused: opt.vtl0_starts_paused,
        emulated_serial_wait_for_rts: opt.serial_wait_for_rts,
        force_load_vtl0_image: opt.force_load_vtl0_image,
        nvme_vfio: opt.nvme_vfio,
        mcr: opt.mcr,
        halt_on_guest_halt: opt.halt_on_guest_halt,
        no_sidecar_hotplug: opt.no_sidecar_hotplug,
        gdbstub: opt.gdbstub,
        hide_isolation: opt.hide_isolation,
        nvme_keep_alive: opt.nvme_keep_alive,
        mana_keep_alive: opt.mana_keep_alive,
        nvme_always_flr: opt.nvme_always_flr,
        test_configuration: opt.test_configuration,
        disable_uefi_frontpage: opt.disable_uefi_frontpage,
        default_boot_always_attempt: opt.default_boot_always_attempt,
        guest_state_lifetime: opt.guest_state_lifetime,
        guest_state_encryption_policy: opt.guest_state_encryption_policy,
        strict_encryption_policy: opt.strict_encryption_policy,
        attempt_ak_cert_callback: opt.attempt_ak_cert_callback,
        enable_vpci_relay: opt.enable_vpci_relay,
    };

    let (mut remote_console_cfg, framebuffer_access) =
        new_underhill_remote_console_cfg(opt.framebuffer_gpa_base)?;

    let mut vnc_worker = None;
    if let Some(framebuffer) = framebuffer_access {
        let listener = VmListener::bind(VmAddress::vsock_any(opt.vnc_port))
            .context("failed to bind socket")?;

        let input_send = remote_console_cfg.input.sender();

        let vnc_host = launch_mesh_host(mesh, "vnc", Some(tracing.tracer()))
            .await
            .context("spawning vnc process failed")?;

        vnc_worker = Some(
            vnc_host
                .launch_worker(
                    vnc_worker_defs::VNC_WORKER_VMSOCKET,
                    VncParameters {
                        listener,
                        framebuffer,
                        input_send,
                    },
                )
                .await?,
        )
    }

    #[cfg(feature = "gdb")]
    let mut gdbstub_worker = None;
    #[cfg_attr(not(feature = "gdb"), expect(unused_mut))]
    let mut debugger_rpc = None;
    #[cfg(feature = "gdb")]
    if opt.gdbstub {
        let listener = VmListener::bind(VmAddress::vsock_any(opt.gdbstub_port))
            .context("failed to bind socket")?;

        let gdb_host = launch_mesh_host(mesh, "gdb", Some(tracing.tracer()))
            .await
            .context("failed to spawn gdb host process")?;

        // Get the VP count of this machine. It's too early to read it directly
        // from IGVM parameters, but the kernel already has the IGVM parsed VP
        // count via the boot loader anyways.
        let vp_count =
            pal::unix::affinity::max_present_cpu().context("failed to get max present cpu")? + 1;

        let (send, recv) = mesh::channel();
        debugger_rpc = Some(recv);
        gdbstub_worker = Some(
            gdb_host
                .launch_worker(
                    debug_worker_defs::DEBUGGER_VSOCK_WORKER,
                    debug_worker_defs::DebuggerParameters {
                        listener,
                        req_chan: send,
                        vp_count,
                        target_arch: if cfg!(guest_arch = "x86_64") {
                            debug_worker_defs::TargetArch::X86_64
                        } else {
                            debug_worker_defs::TargetArch::Aarch64
                        },
                    },
                )
                .await?,
        );
    }
    let (vm_rpc, vm_rpc_rx) = mesh::channel();

    // Spawn the worker in a separate process in case the diagnostics server (in
    // this process) is used to run gdbserver against it, or in case it needs to
    // be restarted.
    let host = launch_mesh_host(mesh, "vm", Some(tracing.tracer()))
        .await
        .context("failed to launch worker process")?;

    let vm_worker = host
        .start_worker(
            worker::UNDERHILL_WORKER,
            UnderhillWorkerParameters {
                env_cfg,
                remote_console_cfg,
                debugger_rpc,
                vm_rpc: vm_rpc_rx,
                control_send,
            },
        )
        .context("failed to launch worker")?;

    Ok(Workers {
        vm: vm_worker,
        vm_rpc,
        vnc: vnc_worker,
        #[cfg(feature = "gdb")]
        gdb: gdbstub_worker,
    })
}

/// State for inspect only.
#[derive(Inspect)]
enum ControlState {
    WaitingForStart,
    Starting,
    Started,
    Restarting,
}

#[derive(MeshPayload)]
pub enum ControlRequest {
    FlushLogs(Rpc<CancelContext, Result<(), CancelReason>>),
}

async fn run_control(
    driver: DefaultDriver,
    mesh: &Mesh,
    opt: Options,
    mut tracing: &mut TracingBackend,
) -> anyhow::Result<()> {
    let (control_send, mut control_recv) = mesh::channel();
    let mut control_send = Some(control_send);

    if opt.signal_vtl0_started {
        signal_vtl0_started(&driver)
            .await
            .context("failed to signal vtl0 started")?;
    }

    let mut diag = DiagState::new().await?;

    let (diag_reinspect_send, mut diag_reinspect_recv) = mesh::channel();
    #[cfg(feature = "profiler")]
    let mut profiler_host = None;
    let mut state;
    let mut workers = if opt.wait_for_start {
        state = ControlState::WaitingForStart;
        None
    } else {
        state = ControlState::Starting;
        let workers = launch_workers(mesh, tracing, control_send.take().unwrap(), opt)
            .await
            .context("failed to launch workers")?;
        Some(workers)
    };

    enum Event {
        Diag(diag_server::DiagRequest),
        Worker(WorkerEvent),
        Control(ControlRequest),
    }

    let mut restart_rpc = None;
    loop {
        let event = {
            let mut stream = (
                (&mut diag.request_recv).map(Event::Diag),
                (&mut diag_reinspect_recv)
                    .map(|req| Event::Diag(diag_server::DiagRequest::Inspect(req))),
                (&mut control_recv).map(Event::Control),
                futures::stream::select_all(workers.as_mut().map(|w| &mut w.vm)).map(Event::Worker),
            )
                .merge();

            let Some(event) = stream.next().await else {
                break;
            };
            event
        };

        match event {
            Event::Diag(request) => {
                match request {
                    diag_server::DiagRequest::Start(rpc) => {
                        rpc.handle_failable(async |params| {
                            if workers.is_some() {
                                Err(anyhow::anyhow!("workers have already been started"))?;
                            }
                            let new_opt = Options::parse(params.args, params.env)
                                .context("failed to parse new options")?;

                            workers = Some(
                                launch_workers(
                                    mesh,
                                    tracing,
                                    control_send.take().unwrap(),
                                    new_opt,
                                )
                                .await?,
                            );
                            state = ControlState::Starting;
                            anyhow::Ok(())
                        })
                        .await
                    }
                    diag_server::DiagRequest::Inspect(deferred) => deferred.respond(|resp| {
                        resp.sensitivity_field("mesh", SensitivityLevel::Safe, mesh)
                            .sensitivity_field_mut("trace", SensitivityLevel::Safe, &mut tracing)
                            .sensitivity_field(
                                "build_info",
                                SensitivityLevel::Safe,
                                build_info::get(),
                            )
                            .sensitivity_child(
                                "proc",
                                SensitivityLevel::Safe,
                                inspect_proc::inspect_proc,
                            )
                            .sensitivity_field("control_state", SensitivityLevel::Safe, &state)
                            // This node can not be renamed due to stability guarantees.
                            // See the comment at the top of inspect_internal for more details.
                            .sensitivity_child("uhdiag", SensitivityLevel::Safe, |req| {
                                inspect_internal::inspect_internal_diagnostics(
                                    req,
                                    &diag_reinspect_send,
                                    &driver,
                                )
                            });

                        resp.merge(&workers);
                    }),
                    diag_server::DiagRequest::Crash(pid) => {
                        mesh.crash(pid);
                    }
                    diag_server::DiagRequest::Restart(rpc) => {
                        let Some(workers) = &mut workers else {
                            rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                                "worker has not been started yet"
                            ))));
                            continue;
                        };

                        let r = async {
                            if restart_rpc.is_some() {
                                anyhow::bail!("previous restart still in progress");
                            }

                            let host = launch_mesh_host(mesh, "vm", Some(tracing.tracer()))
                                .await
                                .context("failed to launch worker process")?;

                            workers.vm.restart(&host);
                            Ok(())
                        }
                        .await;

                        if r.is_err() {
                            rpc.complete(r.map_err(RemoteError::new));
                        } else {
                            state = ControlState::Restarting;
                            restart_rpc = Some(rpc);
                        }
                    }
                    diag_server::DiagRequest::Pause(rpc) => {
                        let Some(workers) = &mut workers else {
                            rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                                "worker has not been started yet"
                            ))));
                            continue;
                        };

                        // create the req future output the spawn, so that
                        // we don't need to clone + move vm_rpc.
                        let req = workers.vm_rpc.call(UhVmRpc::Pause, ());

                        // FUTURE: consider supporting cancellation
                        driver
                            .spawn("diag-pause", async move {
                                let was_paused = req.await.expect("failed to pause VM");
                                rpc.handle_failable_sync(|_| {
                                    if !was_paused {
                                        Err(anyhow::anyhow!("VM is already paused"))
                                    } else {
                                        Ok(())
                                    }
                                });
                            })
                            .detach();
                    }
                    diag_server::DiagRequest::PacketCapture(rpc) => {
                        let Some(workers) = &mut workers else {
                            rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                                "worker has not been started yet"
                            ))));
                            continue;
                        };

                        workers.vm_rpc.send(UhVmRpc::PacketCapture(rpc));
                    }
                    diag_server::DiagRequest::Resume(rpc) => {
                        let Some(workers) = &mut workers else {
                            rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                                "worker has not been started yet"
                            ))));
                            continue;
                        };

                        let was_resumed = workers
                            .vm_rpc
                            .call(UhVmRpc::Resume, ())
                            .await
                            .context("failed to resumed VM")?;

                        let was_halted = workers
                            .vm_rpc
                            .call(UhVmRpc::ClearHalt, ())
                            .await
                            .context("failed to clear halt from VPs")?;

                        rpc.handle_sync(|_| {
                            if was_resumed || was_halted {
                                Ok(())
                            } else {
                                Err(RemoteError::new(anyhow::anyhow!("VM is currently running")))
                            }
                        });
                    }
                    diag_server::DiagRequest::Save(rpc) => {
                        let Some(workers) = &mut workers else {
                            rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                                "worker has not been started yet"
                            ))));
                            continue;
                        };

                        workers.vm_rpc.send(UhVmRpc::Save(rpc));
                    }
                    #[cfg(feature = "profiler")]
                    diag_server::DiagRequest::Profile(rpc) => {
                        let (rpc_params, rpc_sender) = rpc.split();
                        // Create profiler host if there is none created before
                        if profiler_host.is_none() {
                            match launch_mesh_host(mesh, "profiler", Some(tracing.tracer()))
                                .await
                                .context("failed to launch profiler host")
                            {
                                Ok(host) => {
                                    profiler_host = Some(host);
                                }
                                Err(e) => {
                                    rpc_sender.complete(Err(RemoteError::new(e)));
                                    continue;
                                }
                            }
                        }

                        let profiling_duration = rpc_params.duration;
                        let host = profiler_host.as_ref().unwrap();
                        let mut profiler_worker;
                        match host
                            .launch_worker(
                                profiler_worker::PROFILER_WORKER,
                                ProfilerWorkerParameters {
                                    profiler_request: rpc_params,
                                },
                            )
                            .await
                        {
                            Ok(worker) => {
                                profiler_worker = worker;
                            }
                            Err(e) => {
                                rpc_sender.complete(Err(RemoteError::new(e)));
                                continue;
                            }
                        }

                        driver
                            .spawn("profiler_worker", async move {
                                let result = CancelContext::new()
                                    .with_timeout(Duration::from_secs(profiling_duration + 30))
                                    .until_cancelled(profiler_worker.join())
                                    .await
                                    .context("profiler worker cancelled")
                                    .and_then(|result| result.context("profiler worker failed"))
                                    .map_err(RemoteError::new);

                                rpc_sender.complete(result);
                            })
                            .detach();
                    }
                }
            }
            Event::Worker(event) => match event {
                WorkerEvent::Started => {
                    if let Some(response) = restart_rpc.take() {
                        tracing::info!(CVM_ALLOWED, "restart complete");
                        response.complete(Ok(()));
                    } else {
                        tracing::info!(CVM_ALLOWED, "vm worker started");
                    }
                    state = ControlState::Started;
                }
                WorkerEvent::Stopped => {
                    anyhow::bail!("worker unexpectedly stopped");
                }
                WorkerEvent::Failed(err) => {
                    return Err(anyhow::Error::from(err)).context("vm worker failed");
                }
                WorkerEvent::RestartFailed(err) => {
                    tracing::error!(
                        CVM_ALLOWED,
                        error = &err as &dyn std::error::Error,
                        "restart failed"
                    );
                    restart_rpc.take().unwrap().complete(Err(err));
                    state = ControlState::Started;
                }
            },
            Event::Control(req) => match req {
                ControlRequest::FlushLogs(rpc) => {
                    rpc.handle(async |mut ctx| {
                        tracing::info!(CVM_ALLOWED, "flushing logs");
                        ctx.until_cancelled(tracing.flush()).await?;
                        Ok(())
                    })
                    .await
                }
            },
        }
    }

    Ok(())
}

async fn signal_vtl0_started(driver: &DefaultDriver) -> anyhow::Result<()> {
    tracing::info!(CVM_ALLOWED, "signaling vtl0 started early");
    let (client, task) = guest_emulation_transport::spawn_get_worker(driver.clone())
        .await
        .context("failed to spawn GET")?;
    client.complete_start_vtl0(None).await;
    // Disconnect the GET so that it can be reused.
    drop(client);
    task.await.unwrap();
    tracing::info!(CVM_ALLOWED, "signaled vtl0 start");
    Ok(())
}

// The "base" workers for Underhill. Other workers are defined in the
// `underhill_resources` crate.
//
// FUTURE: split these workers into separate crates and move them to
// `underhill_resources`, too.
register_workers! {
    UnderhillVmWorker,
    DiagWorker,
    #[cfg(feature = "profiler")]
    ProfilerWorker,
}
