// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]

mod completions;
mod hyperv;
mod rustyline_printer;
mod vm;

use anyhow::Context;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::ValueEnum;
use futures::StreamExt;
use futures::executor::block_on;
use hyperv::run_hvc;
use mesh::rpc::RpcSend;
use pal_async::DefaultDriver;
use rustyline_printer::Printer;
use std::fmt::Display;
use std::path::PathBuf;
use vm::Vm;

#[derive(Parser)]
#[clap(
    disable_help_flag = true,
    disable_version_flag = true,
    no_binary_name = true,
    max_term_width = 100,
    help_template("{subcommands}")
)]
pub(crate) enum InteractiveCommand {
    /// Select the active VM.
    Select {
        /// The VM's name.
        name: String,
    },

    /// List all VMs.
    List,

    /// Detach from the active VM.
    Detach,

    /// Quit the interactive shell.
    #[clap(visible_alias = "q")]
    Quit,

    #[clap(flatten)]
    Vm(VmCommand),
}

#[derive(Parser)]
pub(crate) enum VmCommand {
    /// Start the VM.
    Start,

    /// Paravisor commands.
    #[clap(subcommand, visible_alias = "pv")]
    Paravisor(ParavisorCommand),

    /// Power off the VM.
    Kill {
        /// Force powering off the VM via the HCS API.
        ///
        /// Without this flag, this command uses the Hyper-V WMI interface.
        /// This may fail if the VM is in a transition state that prevents
        /// powering off for whatever reason (usually due to Hyper-V bugs).
        #[clap(short, long)]
        force: bool,
    },

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

    /// Gets or sets the serial output mode.
    Serial {
        /// The serial port to configure (1 = COM1, etc.).
        port: Option<u32>,
        /// The serial output mode.
        mode: Option<SerialMode>,
    },
}

#[derive(Parser)]
pub(crate) struct InspectArgs {
    /// Enumerate state recursively.
    #[clap(short, long)]
    recursive: bool,
    /// The recursive depth limit.
    #[clap(short, long, requires("recursive"))]
    limit: Option<usize>,
    /// Update the path with a new value.
    #[clap(short, long, conflicts_with("recursive"))]
    update: Option<String>,
    /// The element path to inspect.
    element: Option<String>,
}

#[derive(ValueEnum, Copy, Clone)]
pub(crate) enum SerialMode {
    /// The serial port is disconnected.
    Off,
    /// The serial port output is logged to standard output.
    Log,
    /// The serial port input and output are connected to a new terminal
    /// emulator window.
    Term,
    // TODO: add Console mode for interactive console.
}

impl Display for SerialMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.to_possible_value().unwrap().get_name())
    }
}

#[derive(Parser)]
pub(crate) enum ParavisorCommand {
    /// Tell the paravisor to start the VM.
    Start,

    /// Get or set the output mode for paravisor kmsg logs.
    Kmsg { mode: Option<LogMode> },

    /// Inpsect paravisor state.
    #[clap(visible_alias = "x")]
    Inspect(InspectArgs),

    /// Get or set the paravisor command line.
    CommandLine { command_line: Option<String> },
}

#[derive(ValueEnum, Copy, Clone)]
pub enum LogMode {
    /// The log output is disabled.
    Off,
    /// The log is written to standard output.
    Log,
    /// The log is written to a new terminal emulator window.
    Term,
}

impl Display for LogMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.to_possible_value().unwrap().get_name())
    }
}

pub(crate) enum Request {
    Prompt(mesh::rpc::Rpc<(), String>),
    Inspect(mesh::rpc::Rpc<(InspectTarget, String), anyhow::Result<inspect::Node>>),
    Command(mesh::rpc::Rpc<InteractiveCommand, anyhow::Result<bool>>),
}

pub(crate) enum InspectTarget {
    Paravisor,
}

// DEVNOTE: this tool is not intended to have a stable interface for scripting,
// so resist the temptation to add any kind of way to invoke commands directly
// from the command line.
#[derive(Parser)]
struct CommandLine {
    /// The initial VM name. Use select to change the active VM.
    vm: Option<String>,
    #[clap(long, hide(true))]
    relay_console_path: Option<PathBuf>,
}

pub async fn main(driver: DefaultDriver) -> anyhow::Result<()> {
    let command_line = CommandLine::parse();
    if let Some(relay_console_path) = command_line.relay_console_path {
        return console_relay::relay_console(&relay_console_path, "Serial [OpenVMM]");
    }

    let mut rl = rustyline::Editor::<_, rustyline::history::FileHistory>::with_config(
        rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .build(),
    )
    .unwrap();

    let printer = Printer::new(
        rl.create_external_printer()
            .context("failed to create external printer")?,
    );

    let mut vm = if let Some(name) = command_line.vm {
        Some(Vm::new(driver.clone(), name, printer.clone())?)
    } else {
        None
    };

    let (send, mut recv) = mesh::channel();

    rl.set_helper(Some(completions::OpenvmmRustylineEditor {
        req: send.clone(),
    }));

    let history_file = {
        const HISTORY_FILE: &str = ".hypestv_history";

        // using a `None` to kick off the `.or()` chain in order to make
        // it a bit easier to visually inspect the fallback chain.
        let history_folder = None
            .or_else(dirs::state_dir)
            .or_else(dirs::data_local_dir)
            .map(|path| path.join("hypestv"));

        if let Some(history_folder) = history_folder {
            if let Err(err) = std::fs::create_dir_all(&history_folder) {
                eprintln!(
                    "could not create directory: {}: {}",
                    history_folder.display(),
                    err
                )
            }

            Some(history_folder.join(HISTORY_FILE))
        } else {
            None
        }
    };

    if let Some(history_file) = &history_file {
        println!("restoring history from {}", history_file.display());
        if rl.load_history(history_file).is_err() {
            println!("could not find existing {}", history_file.display());
        }
    }

    // Update the help template for each subcommand.
    let mut template = InteractiveCommand::command();
    for sc in template.get_subcommands_mut() {
        *sc = sc
            .clone()
            .help_template("{about-with-newline}\n{usage-heading}\n    {usage}\n\n{all-args}");
    }

    std::thread::spawn(move || {
        while let Ok(prompt) = block_on(send.call(Request::Prompt, ())) {
            let Ok(line) = rl.readline(&prompt) else {
                break;
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Err(err) = rl.add_history_entry(&line) {
                eprintln!("error adding to history: {}", err);
            }

            match parse(&mut template, trimmed) {
                Ok(cmd) => match block_on(send.call_failable(Request::Command, cmd)) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(err) => {
                        eprintln!("{:#}", err);
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
    });

    while let Some(request) = recv.next().await {
        match request {
            Request::Prompt(rpc) => rpc.handle_sync(|()| {
                if let Some(vm) = &vm {
                    format!(
                        "{vm_name} [{state}]> ",
                        vm_name = vm.name(),
                        state = vm.state()
                    )
                } else {
                    "> ".to_string()
                }
            }),
            Request::Inspect(rpc) => {
                let vm = &mut vm;
                rpc.handle(async |(target, path)| {
                    vm.as_mut()
                        .context("no active VM")?
                        .handle_inspect(target, &path)
                        .await
                })
                .await
            }
            Request::Command(rpc) => {
                rpc.handle(async |cmd| {
                    match cmd {
                        InteractiveCommand::Detach => {
                            vm = None;
                        }
                        InteractiveCommand::Select { name } => {
                            if Some(name.as_ref()) != vm.as_ref().map(|vm| vm.name()) {
                                let new_vm = Vm::new(driver.clone(), name, printer.clone())?;
                                vm = Some(new_vm);
                            }
                        }
                        InteractiveCommand::List => {
                            run_hvc(|cmd| cmd.arg("list")).context("failed to list VMs")?;
                        }
                        InteractiveCommand::Vm(cmd) => {
                            vm.as_mut()
                                .context("no active VM")?
                                .handle_command(cmd)
                                .await?;
                        }
                        InteractiveCommand::Quit => {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })
                .await
            }
        }
    }

    Ok(())
}

fn parse(template: &mut clap::Command, line: &str) -> clap::error::Result<InteractiveCommand> {
    let args = shell_words::split(line)
        .map_err(|err| template.error(clap::error::ErrorKind::ValueValidation, err))?;
    let matches = template.try_get_matches_from_mut(args)?;
    InteractiveCommand::from_arg_matches(&matches).map_err(|err| err.format(template))
}
