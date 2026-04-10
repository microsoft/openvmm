// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interactive REPL (Read-Eval-Print Loop) for openvmm.
//!
//! This module handles user input, command parsing, and dispatching. Commands
//! that operate on shareable resources (e.g., `Sender<VmRpc>`) are executed
//! directly. Commands that need exclusive resources (worker handles,
//! DiagInspector, vtl2_settings) are dispatched via `Sender<VmControllerRpc>`.

use crate::kvp;
use crate::storage_builder;
use crate::vm_controller::AddVtl0ScsiDiskParams;
use crate::vm_controller::InspectTarget;
use crate::vm_controller::RemoveVtl0ScsiDiskByNvmeNsidParams;
use crate::vm_controller::RemoveVtl0ScsiDiskParams;
use crate::vm_controller::ServiceVtl2Params;
use crate::vm_controller::VmControllerEvent;
use crate::vm_controller::VmControllerRpc;
use anyhow::Context;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use console_relay::ConsoleLaunchOptions;
use disk_backend_resources::layer::RamDiskLayerHandle;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use futures::executor::block_on;
use futures_concurrency::stream::Merge;
use inspect::InspectionBuilder;
use mesh::CancelContext;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerRequest;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use std::future::pending;
use std::io;
#[cfg(unix)]
use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use storvsp_resources::ScsiControllerRequest;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use tracing_helpers::AnyhowValueExt;
use vm_resource::IntoResource;
use vm_resource::Resource;

fn maybe_with_radix_u64(s: &str) -> Result<u64, String> {
    let (radix, prefix_len) = if s.starts_with("0x") || s.starts_with("0X") {
        (16, 2)
    } else if s.starts_with("0o") || s.starts_with("0O") {
        (8, 2)
    } else if s.starts_with("0b") || s.starts_with("0B") {
        (2, 2)
    } else {
        (10, 0)
    };

    u64::from_str_radix(&s[prefix_len..], radix).map_err(|e| format!("{e}"))
}

#[derive(Parser)]
#[clap(
    name = "openvmm",
    disable_help_flag = true,
    disable_version_flag = true,
    no_binary_name = true,
    help_template("{subcommands}")
)]
enum InteractiveCommand {
    /// Restart the VM worker (experimental).
    ///
    /// This restarts the VM worker while preserving state.
    #[clap(visible_alias = "R")]
    Restart,

    /// Inject an NMI.
    #[clap(visible_alias = "n")]
    Nmi,

    /// Pause the VM.
    #[clap(visible_alias = "p")]
    Pause,

    /// Resume the VM.
    #[clap(visible_alias = "r")]
    Resume,

    /// Save a snapshot to a directory (requires --memory-backing-file).
    #[clap(visible_alias = "snap")]
    SaveSnapshot {
        /// Directory to write the snapshot to.
        dir: PathBuf,
    },

    /// Do a pulsed save restore (pause, save, reset, restore, resume) to the VM.
    #[clap(visible_alias = "psr")]
    PulseSaveRestore,

    /// Schedule a pulsed save restore (pause, save, reset, restore, resume) to the VM.
    #[clap(visible_alias = "spsr")]
    SchedulePulseSaveRestore {
        /// The interval between pulse save restore operations in seconds.
        /// None or 0 means any previous scheduled pulse save restores will be cleared.
        interval: Option<u64>,
    },

    /// Hot add a disk to the VTL0 guest.
    #[clap(visible_alias = "d")]
    AddDisk {
        #[clap(long = "ro")]
        read_only: bool,
        #[clap(long = "dvd")]
        is_dvd: bool,
        #[clap(long, default_value_t)]
        target: u8,
        #[clap(long, default_value_t)]
        path: u8,
        #[clap(long, default_value_t)]
        lun: u8,
        #[clap(long)]
        ram: Option<u64>,
        file_path: Option<PathBuf>,
    },

    /// Hot remove a disk from the VTL0 guest.
    #[clap(visible_alias = "D")]
    RmDisk {
        #[clap(long)]
        target: u8,
        #[clap(long)]
        path: u8,
        #[clap(long)]
        lun: u8,
    },

    /// Manage VTL2 settings (storage controllers, NICs exposed to VTL0).
    #[clap(subcommand)]
    Vtl2Settings(Vtl2SettingsCommand),

    /// Hot add an NVMe namespace to VTL2, and optionally to VTL0.
    AddNvmeNs {
        #[clap(long = "ro")]
        read_only: bool,
        /// The namespace ID.
        #[clap(long)]
        nsid: u32,
        /// Create a RAM-backed namespace of the specified size in bytes.
        #[clap(long)]
        ram: Option<u64>,
        /// Path to a file to use as the backing store.
        file_path: Option<PathBuf>,
        /// Also expose this namespace to VTL0 via VTL2 settings as a SCSI disk
        /// with the specified LUN number.
        #[clap(long)]
        vtl0_lun: Option<u32>,
    },

    /// Hot remove an NVMe namespace from VTL2.
    RmNvmeNs {
        /// The namespace ID to remove.
        #[clap(long)]
        nsid: u32,
        /// Also remove the VTL0 SCSI disk backed by this namespace.
        #[clap(long)]
        vtl0: bool,
    },

    /// Inspect program state.
    #[clap(visible_alias = "x")]
    Inspect {
        /// Enumerate state recursively.
        #[clap(short, long)]
        recursive: bool,
        /// The recursive depth limit.
        #[clap(short, long, requires("recursive"))]
        limit: Option<usize>,
        /// Target the paravisor.
        #[clap(short = 'v', long)]
        paravisor: bool,
        /// The element path to inspect.
        element: Option<String>,
        /// Update the path with a new value.
        #[clap(short, long, conflicts_with("recursive"))]
        update: Option<String>,
    },

    /// Restart the VNC worker.
    #[clap(visible_alias = "V")]
    RestartVnc,

    /// Start an hvsocket terminal window.
    #[clap(visible_alias = "v")]
    Hvsock {
        /// the terminal emulator to run (defaults to conhost.exe or xterm)
        #[clap(short, long)]
        term: Option<PathBuf>,
        /// the vsock port to connect to
        port: u32,
    },

    /// Quit the program.
    #[clap(visible_alias = "q")]
    Quit,

    /// Write input to the VM console.
    ///
    /// This will write each input parameter to the console's associated serial
    /// port, separated by spaces.
    #[clap(visible_alias = "i")]
    Input { data: Vec<String> },

    /// Switch to input mode.
    ///
    /// Once in input mode, Ctrl-Q returns to command mode.
    #[clap(visible_alias = "I")]
    InputMode,

    /// Reset the VM.
    Reset,

    /// Send a request to the VM to shut it down.
    Shutdown {
        /// Reboot the VM instead of powering it off.
        #[clap(long, short = 'r')]
        reboot: bool,
        /// Hibernate the VM instead of powering it off.
        #[clap(long, short = 'h', conflicts_with = "reboot")]
        hibernate: bool,
        /// Tell the guest to force the power state transition.
        #[clap(long, short = 'f')]
        force: bool,
    },

    /// Clears the current halt condition, resuming the VPs if the VM is
    /// running.
    #[clap(visible_alias = "ch")]
    ClearHalt,

    /// Update the image in VTL2.
    ServiceVtl2 {
        /// Just restart the user-mode paravisor process, not the full
        /// firmware.
        #[clap(long, short = 'u')]
        user_mode_only: bool,
        /// The path to the new IGVM file. If missing, use the originally
        /// configured path.
        #[clap(long, conflicts_with("user_mode_only"))]
        igvm: Option<PathBuf>,
        /// Enable keepalive when servicing VTL2 devices.
        /// Default is `true`.
        #[clap(long, short = 'n', default_missing_value = "true")]
        nvme_keepalive: bool,
        /// Enable keepalive when servicing VTL2 devices.
        /// Default is `false`.
        #[clap(long)]
        mana_keepalive: bool,
    },

    /// Read guest memory
    ReadMemory {
        /// Guest physical address to start at.
        #[clap(value_parser=maybe_with_radix_u64)]
        gpa: u64,
        /// How many bytes to dump.
        #[clap(value_parser=maybe_with_radix_u64)]
        size: u64,
        /// File to save the data to. If omitted,
        /// the data will be presented as a hex dump.
        #[clap(long, short = 'f')]
        file: Option<PathBuf>,
    },

    /// Write guest memory
    WriteMemory {
        /// Guest physical address to start at
        #[clap(value_parser=maybe_with_radix_u64)]
        gpa: u64,
        /// Hex string encoding data, with no `0x` radix.
        /// If omitted, the source file must be specified.
        hex: Option<String>,
        /// File to write the data from.
        #[clap(long, short = 'f')]
        file: Option<PathBuf>,
    },

    /// Inject an artificial panic into OpenVMM
    Panic,

    /// Use KVP to interact with the guest.
    Kvp(kvp::KvpCommand),
}

/// Subcommands for managing VTL2 settings.
#[derive(clap::Subcommand)]
enum Vtl2SettingsCommand {
    /// Show the current VTL2 settings.
    Show,

    /// Add a SCSI disk to VTL0 backed by a VTL2 storage device.
    ///
    /// The backing device can be either a VTL2 NVMe namespace or a VTL2 SCSI disk.
    AddScsiDisk {
        /// The VTL0 SCSI controller instance ID (GUID). Defaults to the standard
        /// OpenVMM VTL0 SCSI instance.
        #[clap(long)]
        controller: Option<String>,
        /// The SCSI LUN to expose to VTL0.
        #[clap(long)]
        lun: u32,
        /// The backing VTL2 NVMe namespace ID.
        #[clap(
            long,
            conflicts_with = "backing_scsi_lun",
            required_unless_present = "backing_scsi_lun"
        )]
        backing_nvme_nsid: Option<u32>,
        /// The backing VTL2 SCSI LUN.
        #[clap(
            long,
            conflicts_with = "backing_nvme_nsid",
            required_unless_present = "backing_nvme_nsid"
        )]
        backing_scsi_lun: Option<u32>,
    },

    /// Remove a SCSI disk from VTL0.
    RmScsiDisk {
        /// The SCSI controller instance ID (GUID). Defaults to the standard
        /// OpenVMM VTL0 SCSI instance.
        #[clap(long)]
        controller: Option<String>,
        /// The SCSI LUN to remove.
        #[clap(long)]
        lun: u32,
    },
}

struct CommandParser {
    app: clap::Command,
}

impl CommandParser {
    fn new() -> Self {
        // Update the help template for each subcommand.
        let mut app = InteractiveCommand::command();
        for sc in app.get_subcommands_mut() {
            *sc = sc
                .clone()
                .help_template("{about-with-newline}\n{usage-heading}\n    {usage}\n\n{all-args}");
        }
        Self { app }
    }

    fn parse(&mut self, line: &str) -> clap::error::Result<InteractiveCommand> {
        let args = shell_words::split(line)
            .map_err(|err| self.app.error(clap::error::ErrorKind::ValueValidation, err))?;
        let matches = self.app.try_get_matches_from_mut(args)?;
        InteractiveCommand::from_arg_matches(&matches).map_err(|err| err.format(&mut self.app))
    }
}

/// Resources shared with the REPL (cloneable senders and handles).
pub(crate) struct ReplResources {
    pub vm_rpc: mesh::Sender<VmRpc>,
    pub vm_controller: mesh::Sender<VmControllerRpc>,
    pub vm_controller_events: mesh::Receiver<VmControllerEvent>,
    pub scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    pub nvme_vtl2_rpc: Option<mesh::Sender<NvmeControllerRequest>>,
    pub shutdown_ic: Option<mesh::Sender<hyperv_ic_resources::shutdown::ShutdownRpc>>,
    pub kvp_ic: Option<mesh::Sender<hyperv_ic_resources::kvp::KvpConnectRpc>>,
    pub console_in: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    pub has_vtl2: bool,
}

/// Run the interactive REPL.
pub(crate) async fn run_repl(
    driver: &DefaultDriver,
    resources: ReplResources,
) -> anyhow::Result<()> {
    let ReplResources {
        vm_rpc,
        vm_controller,
        mut vm_controller_events,
        mut scsi_rpc,
        mut nvme_vtl2_rpc,
        shutdown_ic,
        kvp_ic,
        console_in,
        has_vtl2,
    } = resources;

    let (console_command_send, console_command_recv) = mesh::channel();
    let (inspect_completion_engine_send, inspect_completion_engine_recv) = mesh::channel();

    let mut console_in = console_in;
    thread::Builder::new()
        .name("stdio-thread".to_string())
        .spawn(move || {
            // install panic hook to restore cooked terminal (linux)
            #[cfg(unix)]
            if io::stderr().is_terminal() {
                term::revert_terminal_on_panic()
            }

            let mut rl = rustyline::Editor::<
                OpenvmmRustylineEditor,
                rustyline::history::FileHistory,
            >::with_config(
                rustyline::Config::builder()
                    .completion_type(rustyline::CompletionType::List)
                    .build(),
            )
            .unwrap();

            rl.set_helper(Some(OpenvmmRustylineEditor {
                openvmm_inspect_req: Arc::new(inspect_completion_engine_send),
            }));

            let history_file = {
                const HISTORY_FILE: &str = ".openvmm_history";

                let history_folder = None
                    .or_else(dirs::state_dir)
                    .or_else(dirs::data_local_dir)
                    .map(|path| path.join("openvmm"));

                if let Some(history_folder) = history_folder {
                    if let Err(err) = std::fs::create_dir_all(&history_folder) {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "could not create directory: {}",
                            history_folder.display()
                        )
                    }

                    Some(history_folder.join(HISTORY_FILE))
                } else {
                    None
                }
            };

            if let Some(history_file) = &history_file {
                tracing::info!("restoring history from {}", history_file.display());
                if rl.load_history(history_file).is_err() {
                    tracing::info!("could not find existing {}", history_file.display());
                }
            }

            // Enable Ctrl-Backspace to delete the current word.
            rl.bind_sequence(
                rustyline::KeyEvent::new('\x08', rustyline::Modifiers::CTRL),
                rustyline::Cmd::Kill(rustyline::Movement::BackwardWord(1, rustyline::Word::Emacs)),
            );

            let mut parser = CommandParser::new();

            let mut stdin = io::stdin();
            loop {
                // Raw console text until Ctrl-Q.
                term::set_raw_console(true).expect("failed to set raw console mode");

                if let Some(input) = console_in.as_mut() {
                    let mut buf = [0; 32];
                    loop {
                        let n = stdin.read(&mut buf).unwrap();
                        let mut b = &buf[..n];
                        let stop = if let Some(ctrlq) = b.iter().position(|x| *x == 0x11) {
                            b = &b[..ctrlq];
                            true
                        } else {
                            false
                        };
                        block_on(input.as_mut().write_all(b)).expect("BUGBUG");
                        if stop {
                            break;
                        }
                    }
                }

                term::set_raw_console(false).expect("failed to set raw console mode");

                loop {
                    let line = rl.readline("openvmm> ");
                    if line.is_err() {
                        break;
                    }
                    let line = line.unwrap();
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Err(err) = rl.add_history_entry(&line) {
                        tracing::warn!(
                            err = &err as &dyn std::error::Error,
                            "error adding to .openvmm_history"
                        )
                    }

                    match parser.parse(trimmed) {
                        Ok(cmd) => match cmd {
                            InteractiveCommand::Input { data } => {
                                let mut data = data.join(" ");
                                data.push('\n');
                                if let Some(input) = console_in.as_mut() {
                                    block_on(input.write_all(data.as_bytes())).expect("BUGBUG");
                                }
                            }
                            InteractiveCommand::InputMode => break,
                            cmd => {
                                // Send the command to the main thread for processing.
                                let (processing_done_send, processing_done_recv) =
                                    mesh::oneshot::<()>();
                                console_command_send.send((cmd, processing_done_send));
                                let _ = block_on(processing_done_recv);
                            }
                        },
                        Err(err) => {
                            err.print().unwrap();
                        }
                    }

                    if let Some(history_file) = &history_file {
                        rl.append_history(history_file).unwrap();
                    }
                }
            }
        })
        .unwrap();

    let mut state_change_task = None::<Task<Result<StateChange, RpcError>>>;
    let mut pulse_save_restore_interval: Option<Duration> = None;
    let mut pending_shutdown = None;
    let mut snapshot_saved = false;

    enum StateChange {
        Pause(bool),
        Resume(bool),
        Reset(Result<(), RemoteError>),
        PulseSaveRestore(Result<(), PulseSaveRestoreError>),
        ServiceVtl2(anyhow::Result<Duration>),
    }

    enum Event {
        Command((InteractiveCommand, mesh::OneshotSender<()>)),
        InspectRequestFromCompletionEngine(
            (InspectTarget, String, mesh::OneshotSender<inspect::Node>),
        ),
        Quit,
        PulseSaveRestore,
        StateChange(Result<StateChange, RpcError>),
        ShutdownResult(Result<hyperv_ic_resources::shutdown::ShutdownResult, RpcError>),
        Controller(VmControllerEvent),
    }

    let mut console_command_recv = console_command_recv
        .map(Event::Command)
        .chain(futures::stream::repeat_with(|| Event::Quit));

    let mut inspect_completion_engine_recv =
        inspect_completion_engine_recv.map(Event::InspectRequestFromCompletionEngine);

    loop {
        let event = {
            let pulse_save_restore = pin!(async {
                match pulse_save_restore_interval {
                    Some(wait) => {
                        PolledTimer::new(driver).sleep(wait).await;
                        Event::PulseSaveRestore
                    }
                    None => pending().await,
                }
            });

            let change = futures::stream::iter(state_change_task.as_mut().map(|x| x.into_stream()))
                .flatten()
                .map(Event::StateChange);
            let shutdown = pin!(async {
                if let Some(s) = &mut pending_shutdown {
                    Event::ShutdownResult(s.await)
                } else {
                    pending().await
                }
            });
            let controller_events = (&mut vm_controller_events).map(Event::Controller);

            (
                &mut console_command_recv,
                &mut inspect_completion_engine_recv,
                pulse_save_restore.into_stream(),
                change,
                shutdown.into_stream(),
                controller_events,
            )
                .merge()
                .next()
                .await
                .unwrap()
        };

        let (cmd, _processing_done_send) = match event {
            Event::Command(message) => message,
            Event::InspectRequestFromCompletionEngine((target, path, res)) => {
                let depth = Some(1);
                let element = if path.is_empty() { None } else { Some(path) };
                let deferred = {
                    let element_ref = element.as_deref().unwrap_or("");
                    let mut inspection = InspectionBuilder::new(element_ref).depth(depth).inspect(
                        inspect::adhoc_mut(|req| {
                            vm_controller.send(VmControllerRpc::Inspect(target, req.defer()));
                        }),
                    );
                    let _ = CancelContext::new()
                        .with_timeout(Duration::from_secs(1))
                        .until_cancelled(inspection.resolve())
                        .await;
                    inspection.results()
                };
                res.send(deferred);
                continue;
            }
            Event::Quit => break,
            Event::PulseSaveRestore => {
                vm_rpc.call(VmRpc::PulseSaveRestore, ()).await??;
                continue;
            }
            Event::StateChange(r) => {
                match r {
                    Ok(sc) => match sc {
                        StateChange::Pause(success) => {
                            if success {
                                tracing::info!("pause complete");
                            } else {
                                tracing::warn!("already paused");
                            }
                        }
                        StateChange::Resume(success) => {
                            if success {
                                tracing::info!("resumed complete");
                            } else {
                                tracing::warn!("already running");
                            }
                        }
                        StateChange::Reset(r) => match r {
                            Ok(()) => tracing::info!("reset complete"),
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "reset failed"
                            ),
                        },
                        StateChange::PulseSaveRestore(r) => match r {
                            Ok(()) => tracing::info!("pulse save/restore complete"),
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "pulse save/restore failed"
                            ),
                        },
                        StateChange::ServiceVtl2(r) => match r {
                            Ok(dur) => {
                                tracing::info!(
                                    duration = dur.as_millis() as i64,
                                    "vtl2 servicing complete"
                                )
                            }
                            Err(err) => tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "vtl2 servicing failed"
                            ),
                        },
                    },
                    Err(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "communication failure during state change"
                        );
                    }
                }
                state_change_task = None;
                continue;
            }
            Event::ShutdownResult(r) => {
                match r {
                    Ok(r) => match r {
                        hyperv_ic_resources::shutdown::ShutdownResult::Ok => {
                            tracing::info!("shutdown initiated");
                        }
                        hyperv_ic_resources::shutdown::ShutdownResult::NotReady => {
                            tracing::error!("shutdown ic not ready");
                        }
                        hyperv_ic_resources::shutdown::ShutdownResult::AlreadyInProgress => {
                            tracing::error!("shutdown already in progress");
                        }
                        hyperv_ic_resources::shutdown::ShutdownResult::Failed(hr) => {
                            tracing::error!("shutdown failed with error code {hr:#x}");
                        }
                    },
                    Err(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "communication failure during shutdown"
                        );
                    }
                }
                pending_shutdown = None;
                continue;
            }
            Event::Controller(event) => {
                match event {
                    VmControllerEvent::WorkerStopped { error } => {
                        if let Some(err) = &error {
                            tracing::error!(error = err.as_str(), "vm worker stopped");
                        }
                        break;
                    }
                    VmControllerEvent::VncWorkerStopped { .. } => {
                        // VNC stopped but VM is still running, continue.
                    }
                    VmControllerEvent::GuestHalt(reason) => {
                        tracing::info!(reason = reason.as_str(), "guest halted");
                    }
                }
                continue;
            }
        };

        fn state_change<U: 'static + Send>(
            driver: impl Spawn,
            vm_rpc: &mesh::Sender<VmRpc>,
            state_change_task: &mut Option<Task<Result<StateChange, RpcError>>>,
            f: impl FnOnce(Rpc<(), U>) -> VmRpc,
            g: impl FnOnce(U) -> StateChange + 'static + Send,
        ) {
            if state_change_task.is_some() {
                tracing::error!("state change already in progress");
            } else {
                let rpc = vm_rpc.call(f, ());
                *state_change_task =
                    Some(driver.spawn("state-change", async move { Ok(g(rpc.await?)) }));
            }
        }

        match cmd {
            InteractiveCommand::Panic => {
                panic!("injected panic")
            }
            InteractiveCommand::Restart => {
                match vm_controller
                    .call(VmControllerRpc::Restart, ())
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| Ok(r?))
                {
                    Ok(()) => {}
                    Err(err) => {
                        eprintln!("error: {err:#}");
                    }
                }
            }
            InteractiveCommand::Pause => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::Pause,
                    StateChange::Pause,
                );
            }
            InteractiveCommand::Resume => {
                if snapshot_saved {
                    eprintln!(
                        "error: cannot resume after snapshot save — resuming would corrupt the snapshot. Use 'shutdown' to exit."
                    );
                } else {
                    state_change(
                        driver,
                        &vm_rpc,
                        &mut state_change_task,
                        VmRpc::Resume,
                        StateChange::Resume,
                    );
                }
            }
            InteractiveCommand::Reset => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::Reset,
                    StateChange::Reset,
                );
            }
            InteractiveCommand::SaveSnapshot { dir } => {
                match vm_controller
                    .call(
                        VmControllerRpc::SaveSnapshot,
                        dir.to_string_lossy().into_owned(),
                    )
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| Ok(r?))
                {
                    Ok(()) => {
                        snapshot_saved = true;
                        tracing::info!(
                            dir = %dir.display(),
                            "snapshot saved; VM is paused. \
                             Resume is blocked to prevent snapshot corruption. \
                             Use 'shutdown' to exit."
                        );
                    }
                    Err(err) => {
                        eprintln!("error: save-snapshot failed: {err:#}");
                    }
                }
            }
            InteractiveCommand::PulseSaveRestore => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::PulseSaveRestore,
                    StateChange::PulseSaveRestore,
                );
            }
            InteractiveCommand::SchedulePulseSaveRestore { interval } => {
                pulse_save_restore_interval = match interval {
                    Some(seconds) if seconds != 0 => Some(Duration::from_secs(seconds)),
                    _ => None,
                }
            }
            InteractiveCommand::Shutdown {
                reboot,
                hibernate,
                force,
            } => {
                if pending_shutdown.is_some() {
                    println!("shutdown already in progress");
                } else if let Some(ic) = &shutdown_ic {
                    let params = hyperv_ic_resources::shutdown::ShutdownParams {
                        shutdown_type: if hibernate {
                            hyperv_ic_resources::shutdown::ShutdownType::Hibernate
                        } else if reboot {
                            hyperv_ic_resources::shutdown::ShutdownType::Reboot
                        } else {
                            hyperv_ic_resources::shutdown::ShutdownType::PowerOff
                        },
                        force,
                    };
                    pending_shutdown =
                        Some(ic.call(hyperv_ic_resources::shutdown::ShutdownRpc::Shutdown, params));
                } else {
                    println!("no shutdown ic configured");
                }
            }
            InteractiveCommand::Nmi => {
                let _ = vm_rpc.call(VmRpc::Nmi, 0).await;
            }
            InteractiveCommand::ClearHalt => {
                vm_rpc.call(VmRpc::ClearHalt, ()).await.ok();
            }
            InteractiveCommand::AddDisk {
                read_only,
                target,
                path,
                lun,
                ram,
                file_path,
                is_dvd,
            } => {
                let action = async {
                    let scsi = scsi_rpc.as_ref().context("no scsi controller")?;
                    let disk_type = match ram {
                        None => {
                            let path = file_path.context("no filename passed")?;
                            openvmm_helpers::disk::open_disk_type(path.as_ref(), read_only)
                                .with_context(|| format!("failed to open {}", path.display()))?
                        }
                        Some(size) => {
                            Resource::new(disk_backend_resources::LayeredDiskHandle::single_layer(
                                RamDiskLayerHandle {
                                    len: Some(size),
                                    sector_size: None,
                                },
                            ))
                        }
                    };

                    let device = if is_dvd {
                        SimpleScsiDvdHandle {
                            media: Some(disk_type),
                            requests: None,
                        }
                        .into_resource()
                    } else {
                        SimpleScsiDiskHandle {
                            disk: disk_type,
                            read_only,
                            parameters: Default::default(),
                        }
                        .into_resource()
                    };

                    let cfg = ScsiDeviceAndPath {
                        path: ScsiPath { path, target, lun },
                        device,
                    };

                    scsi.call_failable(ScsiControllerRequest::AddDevice, cfg)
                        .await?;

                    anyhow::Result::<_>::Ok(())
                };

                if let Err(error) = action.await {
                    tracing::error!(error = error.as_error(), "error adding disk")
                }
            }
            InteractiveCommand::RmDisk { target, path, lun } => {
                let action = async {
                    let scsi = scsi_rpc.as_ref().context("no scsi controller")?;
                    scsi.call_failable(
                        ScsiControllerRequest::RemoveDevice,
                        ScsiPath { target, path, lun },
                    )
                    .await?;
                    anyhow::Ok(())
                };

                if let Err(error) = action.await {
                    tracing::error!(error = error.as_error(), "error removing disk")
                }
            }
            InteractiveCommand::Vtl2Settings(cmd) => {
                if !has_vtl2 {
                    eprintln!("error: no VTL2 settings (not running with VTL2?)");
                    continue;
                }
                let action = async {
                    match cmd {
                        Vtl2SettingsCommand::Show => {
                            let encoded = vm_controller
                                .call(VmControllerRpc::GetVtl2Settings, ())
                                .await
                                .map_err(anyhow::Error::from)?;
                            if let Some(bytes) = encoded {
                                let settings: vtl2_settings_proto::Vtl2Settings =
                                    prost::Message::decode(bytes.as_slice())
                                        .context("failed to decode vtl2 settings")?;
                                println!("{:#?}", settings);
                            } else {
                                println!("(no VTL2 settings)");
                            }
                        }
                        Vtl2SettingsCommand::AddScsiDisk {
                            controller,
                            lun,
                            backing_nvme_nsid,
                            backing_scsi_lun,
                        } => {
                            let (device_type, device_path, sub_device_path) = match (
                                backing_nvme_nsid,
                                backing_scsi_lun,
                            ) {
                                (Some(nsid), None) => (
                                    vtl2_settings_proto::physical_device::DeviceType::Nvme,
                                    storage_builder::NVME_VTL2_INSTANCE_ID,
                                    nsid,
                                ),
                                (None, Some(scsi_lun)) => (
                                    vtl2_settings_proto::physical_device::DeviceType::Vscsi,
                                    storage_builder::SCSI_VTL2_INSTANCE_ID,
                                    scsi_lun,
                                ),
                                (Some(_), Some(_)) => {
                                    anyhow::bail!(
                                        "can't specify both --backing-nvme-nsid and --backing-scsi-lun"
                                    );
                                }
                                (None, None) => {
                                    anyhow::bail!(
                                        "must specify either --backing-nvme-nsid or --backing-scsi-lun"
                                    );
                                }
                            };

                            let controller_guid = controller
                                .map(|s| s.parse())
                                .transpose()
                                .context("invalid controller GUID")?
                                .unwrap_or(storage_builder::UNDERHILL_VTL0_SCSI_INSTANCE);

                            vm_controller
                                .call(
                                    VmControllerRpc::AddVtl0ScsiDisk,
                                    AddVtl0ScsiDiskParams {
                                        controller_guid,
                                        lun,
                                        device_type: device_type as i32,
                                        device_path,
                                        sub_device_path,
                                    },
                                )
                                .await
                                .map_err(anyhow::Error::from)?
                                .map_err(anyhow::Error::from)?;

                            let backing_desc = if backing_nvme_nsid.is_some() {
                                format!("nvme_nsid={}", sub_device_path)
                            } else {
                                format!("scsi_lun={}", sub_device_path)
                            };
                            println!(
                                "Added VTL0 SCSI disk: controller={}, lun={}, backing={}",
                                controller_guid, lun, backing_desc
                            );
                        }
                        Vtl2SettingsCommand::RmScsiDisk { controller, lun } => {
                            let controller_guid = controller
                                .map(|s| s.parse())
                                .transpose()
                                .context("invalid controller GUID")?
                                .unwrap_or(storage_builder::UNDERHILL_VTL0_SCSI_INSTANCE);

                            vm_controller
                                .call(
                                    VmControllerRpc::RemoveVtl0ScsiDisk,
                                    RemoveVtl0ScsiDiskParams {
                                        controller_guid,
                                        lun,
                                    },
                                )
                                .await
                                .map_err(anyhow::Error::from)?
                                .map_err(anyhow::Error::from)?;

                            println!(
                                "Removed VTL0 SCSI disk: controller={}, lun={}",
                                controller_guid, lun
                            );
                        }
                    }
                    anyhow::Ok(())
                };

                if let Err(error) = action.await {
                    eprintln!("error: {}", error);
                }
            }
            InteractiveCommand::AddNvmeNs {
                read_only,
                nsid,
                ram,
                file_path,
                vtl0_lun,
            } => {
                if !has_vtl2 {
                    eprintln!("error: add-nvme-ns requires --vtl2 mode");
                    continue;
                }
                let action = async {
                    let nvme = nvme_vtl2_rpc.as_ref().context("no vtl2 nvme controller")?;
                    let disk_type = match (ram, file_path) {
                        (None, Some(path)) => {
                            openvmm_helpers::disk::open_disk_type(path.as_ref(), read_only)
                                .with_context(|| format!("failed to open {}", path.display()))?
                        }
                        (Some(size), None) => {
                            Resource::new(disk_backend_resources::LayeredDiskHandle::single_layer(
                                RamDiskLayerHandle {
                                    len: Some(size),
                                    sector_size: None,
                                },
                            ))
                        }
                        (None, None) => {
                            anyhow::bail!("must specify either file path or --ram");
                        }
                        (Some(_), Some(_)) => {
                            anyhow::bail!("cannot specify both file path and --ram");
                        }
                    };

                    let ns = NamespaceDefinition {
                        nsid,
                        read_only,
                        disk: disk_type,
                    };

                    nvme.call_failable(NvmeControllerRequest::AddNamespace, ns)
                        .await?;
                    println!("Added namespace {}", nsid);

                    if let Some(lun) = vtl0_lun {
                        vm_controller
                            .call(
                                VmControllerRpc::AddVtl0ScsiDisk,
                                AddVtl0ScsiDiskParams {
                                    controller_guid: storage_builder::UNDERHILL_VTL0_SCSI_INSTANCE,
                                    lun,
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Nvme
                                            as i32,
                                    device_path: storage_builder::NVME_VTL2_INSTANCE_ID,
                                    sub_device_path: nsid,
                                },
                            )
                            .await
                            .map_err(anyhow::Error::from)?
                            .map_err(anyhow::Error::from)?;
                        println!("Exposed namespace {} to VTL0 as SCSI lun={}", nsid, lun);
                    }

                    Ok(())
                };

                if let Err(error) = action.await {
                    eprintln!("error adding nvme namespace: {}", error);
                }
            }
            InteractiveCommand::RmNvmeNs { nsid, vtl0 } => {
                if !has_vtl2 {
                    eprintln!("error: rm-nvme-ns requires --vtl2 mode");
                    continue;
                }
                let action = async {
                    if vtl0 {
                        let removed_lun = vm_controller
                            .call(
                                VmControllerRpc::RemoveVtl0ScsiDiskByNvmeNsid,
                                RemoveVtl0ScsiDiskByNvmeNsidParams {
                                    controller_guid: storage_builder::UNDERHILL_VTL0_SCSI_INSTANCE,
                                    nvme_controller_guid: storage_builder::NVME_VTL2_INSTANCE_ID,
                                    nsid,
                                },
                            )
                            .await
                            .map_err(anyhow::Error::from)?
                            .map_err(anyhow::Error::from)?;
                        if let Some(lun) = removed_lun {
                            println!("Removed VTL0 SCSI lun={}", lun);
                        } else {
                            println!("No VTL0 SCSI disk found backed by NVMe nsid={}", nsid);
                        }
                    }

                    let nvme = nvme_vtl2_rpc.as_ref().context("no vtl2 nvme controller")?;
                    nvme.call_failable(NvmeControllerRequest::RemoveNamespace, nsid)
                        .await?;
                    println!("Removed NVMe namespace {}", nsid);
                    anyhow::Ok(())
                };

                if let Err(error) = action.await {
                    eprintln!("error removing nvme namespace: {}", error);
                }
            }
            InteractiveCommand::Inspect {
                recursive,
                limit,
                paravisor,
                element,
                update,
            } => {
                let target = if paravisor {
                    InspectTarget::Paravisor
                } else {
                    InspectTarget::Host
                };

                let obj = inspect::adhoc_mut(|req| {
                    vm_controller.send(VmControllerRpc::Inspect(target, req.defer()));
                });

                if let Some(value) = update {
                    let Some(element) = element else {
                        eprintln!("error: must provide element for update");
                        continue;
                    };

                    let value = async {
                        let update = inspect::update(&element, &value, obj);
                        let value = CancelContext::new()
                            .with_timeout(Duration::from_secs(1))
                            .until_cancelled(update)
                            .await??;
                        anyhow::Ok(value)
                    }
                    .await;
                    match value {
                        Ok(node) => match &node.kind {
                            inspect::ValueKind::String(s) => println!("{s}"),
                            _ => println!("{:#}", node),
                        },
                        Err(err) => println!("error: {:#}", err),
                    }
                } else {
                    let element = element.unwrap_or_default();
                    let depth = if recursive { limit } else { Some(0) };
                    let node = async {
                        let mut inspection =
                            InspectionBuilder::new(&element).depth(depth).inspect(obj);
                        let _ = CancelContext::new()
                            .with_timeout(Duration::from_secs(1))
                            .until_cancelled(inspection.resolve())
                            .await;
                        inspection.results()
                    }
                    .await;

                    println!("{:#}", node);
                }
            }
            InteractiveCommand::RestartVnc => {
                match vm_controller
                    .call(VmControllerRpc::RestartVnc, ())
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| Ok(r?))
                {
                    Ok(()) => {}
                    Err(err) => {
                        eprintln!("error: {err:#}");
                    }
                }
            }
            InteractiveCommand::Hvsock { term, port } => {
                let vm_rpc = &vm_rpc;
                let action = async || {
                    let service_id = crate::new_hvsock_service_id(port);
                    let socket = vm_rpc
                        .call_failable(
                            VmRpc::ConnectHvsock,
                            (
                                CancelContext::new().with_timeout(Duration::from_secs(2)),
                                service_id,
                                DeviceVtl::Vtl0,
                            ),
                        )
                        .await?;
                    let socket = PolledSocket::new(driver, socket)?;
                    let mut console = console_relay::Console::new(
                        driver.clone(),
                        term.or_else(crate::openvmm_terminal_app).as_deref(),
                        Some(ConsoleLaunchOptions {
                            window_title: Some(format!("HVSock{} [OpenVMM]", port)),
                        }),
                    )?;
                    driver
                        .spawn("console-relay", async move { console.relay(socket).await })
                        .detach();
                    anyhow::Result::<_>::Ok(())
                };

                if let Err(error) = (action)().await {
                    eprintln!("error: {}", error);
                }
            }
            InteractiveCommand::ServiceVtl2 {
                user_mode_only,
                igvm,
                mana_keepalive,
                nvme_keepalive,
            } => {
                let vm_controller = vm_controller.clone();
                let r = async move {
                    let millis = vm_controller
                        .call(
                            VmControllerRpc::ServiceVtl2,
                            ServiceVtl2Params {
                                user_mode_only,
                                igvm: igvm.map(|p| p.to_string_lossy().into_owned()),
                                nvme_keepalive,
                                mana_keepalive,
                            },
                        )
                        .await??;
                    Ok(Duration::from_millis(millis))
                }
                .map(|r| Ok(StateChange::ServiceVtl2(r)));
                if state_change_task.is_some() {
                    tracing::error!("state change already in progress");
                } else {
                    state_change_task = Some(driver.spawn("state-change", r));
                }
            }
            InteractiveCommand::Quit => {
                tracing::info!("quitting");
                // Work around the detached SCSI task holding up worker stop.
                // TODO: Fix the underlying bug
                drop(scsi_rpc.take());
                drop(nvme_vtl2_rpc.take());
                vm_controller.send(VmControllerRpc::Quit);
            }
            InteractiveCommand::ReadMemory { gpa, size, file } => {
                let size = size as usize;
                let data = vm_rpc.call(VmRpc::ReadMemory, (gpa, size)).await?;

                match data {
                    Ok(bytes) => {
                        if let Some(file) = file {
                            if let Err(err) = fs_err::write(file, bytes) {
                                eprintln!("error: {err:?}");
                            }
                        } else {
                            let width = 16;
                            let show_ascii = true;

                            let mut dump = String::new();
                            for (i, chunk) in bytes.chunks(width).enumerate() {
                                let hex_part: Vec<String> =
                                    chunk.iter().map(|byte| format!("{:02x}", byte)).collect();
                                let hex_line = hex_part.join(" ");

                                if show_ascii {
                                    let ascii_part: String = chunk
                                        .iter()
                                        .map(|&byte| {
                                            if byte.is_ascii_graphic() || byte == b' ' {
                                                byte as char
                                            } else {
                                                '.'
                                            }
                                        })
                                        .collect();
                                    std::fmt::Write::write_fmt(
                                        &mut dump,
                                        format_args!(
                                            "{:04x}: {:<width$}  {}\n",
                                            i * width,
                                            hex_line,
                                            ascii_part,
                                            width = width * 3 - 1
                                        ),
                                    )
                                    .unwrap();
                                } else {
                                    std::fmt::Write::write_fmt(
                                        &mut dump,
                                        format_args!("{:04x}: {}\n", i * width, hex_line),
                                    )
                                    .unwrap();
                                }
                            }

                            println!("{dump}");
                        }
                    }
                    Err(err) => {
                        eprintln!("error: {err:?}");
                    }
                }
            }
            InteractiveCommand::WriteMemory { gpa, hex, file } => {
                if hex.is_some() == file.is_some() {
                    eprintln!("error: either path to the file or the hex string must be specified");
                    continue;
                }

                let data = if let Some(file) = file {
                    let data = fs_err::read(file);
                    match data {
                        Ok(data) => data,
                        Err(err) => {
                            eprintln!("error: {err:?}");
                            continue;
                        }
                    }
                } else if let Some(hex) = hex {
                    if hex.len() & 1 != 0 {
                        eprintln!(
                            "error: expected even number of hex digits (2 hex digits per byte)"
                        );
                        continue;
                    }
                    let data: Result<Vec<u8>, String> = (0..hex.len())
                        .step_by(2)
                        .map(|i| {
                            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| {
                                format!("invalid hex character at position {}: {}", i, e)
                            })
                        })
                        .collect();

                    match data {
                        Ok(data) => data,
                        Err(err) => {
                            eprintln!("error: {err}");
                            continue;
                        }
                    }
                } else {
                    unreachable!();
                };

                if data.is_empty() {
                    eprintln!("error: no data to write");
                    continue;
                }

                if let Err(err) = vm_rpc.call(VmRpc::WriteMemory, (gpa, data)).await? {
                    eprintln!("error: {err:?}");
                }
            }
            InteractiveCommand::Kvp(command) => {
                let Some(kvp) = &kvp_ic else {
                    eprintln!("error: no kvp ic configured");
                    continue;
                };
                if let Err(err) = kvp::handle_kvp(kvp, command).await {
                    eprintln!("error: {err:#}");
                }
            }
            InteractiveCommand::Input { .. } | InteractiveCommand::InputMode => unreachable!(),
        }
    }

    Ok(())
}

// -- Rustyline helpers --

use rustyline::Helper;
use rustyline::Highlighter;
use rustyline::Hinter;
use rustyline::Validator;

#[derive(Helper, Highlighter, Hinter, Validator)]
struct OpenvmmRustylineEditor {
    openvmm_inspect_req:
        Arc<mesh::Sender<(InspectTarget, String, mesh::OneshotSender<inspect::Node>)>>,
}

impl rustyline::completion::Completer for OpenvmmRustylineEditor {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let Ok(cmd) = shell_words::split(line) else {
            return Ok((0, Vec::with_capacity(0)));
        };

        let completions = block_on(
            clap_dyn_complete::Complete {
                cmd,
                raw: Some(line.into()),
                position: Some(pos),
            }
            .generate_completions::<InteractiveCommand>(None, self),
        );

        let pos_from_end = {
            let line = line.chars().take(pos).collect::<String>();

            let trailing_ws = line.len() - line.trim_end().len();

            if trailing_ws > 0 {
                line.len() - trailing_ws + 1
            } else {
                let last_word = shell_words::split(&line)
                    .unwrap_or_default()
                    .last()
                    .cloned()
                    .unwrap_or_default();

                line.len() - last_word.len()
            }
        };

        Ok((pos_from_end, completions))
    }
}

impl clap_dyn_complete::CustomCompleterFactory for &OpenvmmRustylineEditor {
    type CustomCompleter = OpenvmmComplete;
    async fn build(&self, _ctx: &clap_dyn_complete::RootCtx<'_>) -> Self::CustomCompleter {
        OpenvmmComplete {
            openvmm_inspect_req: self.openvmm_inspect_req.clone(),
        }
    }
}

struct OpenvmmComplete {
    openvmm_inspect_req:
        Arc<mesh::Sender<(InspectTarget, String, mesh::OneshotSender<inspect::Node>)>>,
}

impl clap_dyn_complete::CustomCompleter for OpenvmmComplete {
    async fn complete(
        &self,
        ctx: &clap_dyn_complete::RootCtx<'_>,
        subcommand_path: &[&str],
        arg_id: &str,
    ) -> Vec<String> {
        match (subcommand_path, arg_id) {
            (["openvmm", "inspect"], "element") => {
                let on_error = vec!["failed/to/connect".into()];

                let (parent_path, to_complete) = (ctx.to_complete)
                    .rsplit_once('/')
                    .unwrap_or(("", ctx.to_complete));

                let node = {
                    let paravisor = {
                        let raw_arg = ctx
                            .matches
                            .subcommand()
                            .unwrap()
                            .1
                            .get_one::<String>("paravisor")
                            .map(|x| x.as_str())
                            .unwrap_or_default();
                        raw_arg == "true"
                    };

                    let (tx, rx) = mesh::oneshot();
                    self.openvmm_inspect_req.send((
                        if paravisor {
                            InspectTarget::Paravisor
                        } else {
                            InspectTarget::Host
                        },
                        parent_path.to_owned(),
                        tx,
                    ));
                    let Ok(node) = rx.await else {
                        return on_error;
                    };

                    node
                };

                let mut completions = Vec::new();

                if let inspect::Node::Dir(dir) = node {
                    for entry in dir {
                        if entry.name.starts_with(to_complete) {
                            if parent_path.is_empty() {
                                completions.push(format!("{}/", entry.name))
                            } else {
                                completions.push(format!(
                                    "{}/{}{}",
                                    parent_path,
                                    entry.name,
                                    if matches!(entry.node, inspect::Node::Dir(..)) {
                                        "/"
                                    } else {
                                        ""
                                    }
                                ))
                            }
                        }
                    }
                } else {
                    return on_error;
                }

                completions
            }
            _ => Vec::new(),
        }
    }
}
