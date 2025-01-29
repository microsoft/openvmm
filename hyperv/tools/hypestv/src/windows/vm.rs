// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM command handling.

use super::hyperv::hvc_output;
use super::hyperv::powershell_script;
use super::hyperv::run_hcsdiag;
use super::hyperv::run_hvc;
use super::rustyline_printer::Printer;
use super::InspectArgs;
use super::InspectTarget;
use super::LogMode;
use super::ParavisorCommand;
use super::SerialMode;
use super::VmCommand;
use anyhow::Context as _;
use diag_client::DiagClient;
use futures::io::BufReader;
use futures::AsyncBufReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guid::Guid;
use pal_async::pipe::PolledPipe;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::DefaultDriver;
use std::sync::Arc;
use std::time::Duration;

pub struct Vm {
    inner: Arc<VmInner>,
    serial: Vec<Option<SerialTask>>,
    pv_kmsg: Option<KmsgTask>,
}

struct SerialTask {
    mode: SerialMode,
    task: Task<()>,
    req: mesh::Sender<IoRequest>,
}

struct KmsgTask {
    mode: LogMode,
    task: Task<()>,
    req: mesh::Sender<IoRequest>,
}

struct VmInner {
    driver: DefaultDriver,
    paravisor_diag: DiagClient,
    name: String,
    id: Guid,
    printer: Printer,
}

impl Vm {
    pub fn new(driver: DefaultDriver, name: String, printer: Printer) -> anyhow::Result<Self> {
        let id = diag_client::hyperv::vm_id_from_name(&name).context("failed to get vm id")?;
        let inner = Arc::new(VmInner {
            driver: driver.clone(),
            paravisor_diag: DiagClient::from_hyperv_id(driver, id),
            printer,
            name,
            id,
        });
        Ok(Self {
            serial: (0..4).map(|_| None).collect(),
            inner,
            pv_kmsg: None,
        })
    }

    fn delay(&self, f: impl 'static + FnOnce(&VmInner) -> anyhow::Result<()> + Send) {
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            if let Err(err) = f(&inner) {
                writeln!(inner.printer.out(), "{:#}", err).ok();
            };
        });
    }

    pub async fn handle_inspect(
        &mut self,
        target: InspectTarget,
        path: &str,
    ) -> anyhow::Result<inspect::Node> {
        match target {
            InspectTarget::Paravisor => {
                self.inner
                    .paravisor_diag
                    .inspect(path, Some(0), Some(Duration::from_secs(1)))
                    .await
            }
        }
    }

    pub async fn handle_command(&mut self, cmd: VmCommand) -> anyhow::Result<()> {
        match cmd {
            VmCommand::Start => self.delay(move |inner| {
                run_hvc(|cmd| cmd.arg("start").arg(&inner.name))?;
                writeln!(inner.printer.out(), "VM started")?;
                Ok(())
            }),
            VmCommand::Kill { force } => self.delay(move |inner| {
                if force {
                    run_hcsdiag(|cmd| cmd.arg("kill").arg(inner.id.to_string()))?;
                } else {
                    run_hvc(|cmd| cmd.arg("kill").arg(&inner.name))?;
                }
                writeln!(inner.printer.out(), "VM killed")?;
                Ok(())
            }),
            VmCommand::Reset => self.delay(move |inner| {
                run_hvc(|cmd| cmd.arg("reset").arg(&inner.name))?;
                writeln!(inner.printer.out(), "VM reset")?;
                Ok(())
            }),
            VmCommand::Shutdown {
                reboot,
                hibernate,
                force,
            } => {
                if hibernate {
                    anyhow::bail!("hibernate not supported");
                }
                self.delay(move |inner| {
                    run_hvc(|cmd| {
                        cmd.arg(if reboot { "restart" } else { "stop" });
                        if force {
                            cmd.arg("-f");
                        }
                        cmd.arg(&inner.name)
                    })?;
                    if reboot {
                        writeln!(inner.printer.out(), "VM restarted")?;
                    } else {
                        writeln!(inner.printer.out(), "VM shutdown")?;
                    }
                    Ok(())
                });
            }
            VmCommand::Serial {
                port: None,
                mode: _,
            } => {
                for (i, port) in self.serial.iter().enumerate() {
                    println!(
                        "COM{}: {}",
                        i + 1,
                        port.as_ref().map_or(SerialMode::Off, |t| t.mode)
                    );
                }
            }
            VmCommand::Serial {
                port: Some(port),
                mode: None,
            } => {
                let port_index = port.checked_sub(1).context("invalid port")? as usize;
                let task = self.serial.get_mut(port_index).context("invalid port")?;
                println!("{}", task.as_ref().map_or(SerialMode::Off, |t| t.mode));
            }
            VmCommand::Serial {
                port: Some(port),
                mode: Some(mode),
            } => {
                let port_index = port.checked_sub(1).context("invalid port")? as usize;
                let task = self.serial.get_mut(port_index).context("invalid port")?;

                let target = match mode {
                    SerialMode::Off => {
                        if let Some(task) = task.take() {
                            drop(task.req);
                            task.task.await;
                        }
                        None
                    }
                    SerialMode::Log => Some(IoTarget::Printer),
                    SerialMode::Term => Some(IoTarget::Console(
                        console_relay::Console::new(self.inner.driver.clone(), None)
                            .context("failed to launch console")?,
                    )),
                };
                if let Some(target) = target {
                    if task.as_ref().is_some_and(|task| task.task.is_finished()) {
                        *task = None;
                    }
                    if let Some(task) = task {
                        task.mode = mode;
                        task.req.send(IoRequest::NewTarget(target));
                    } else {
                        let (req, recv) = mesh::channel();
                        let inner = self.inner.clone();
                        let t = self.inner.driver.spawn("serial", async move {
                            if let Err(err) = inner.handle_serial(recv, target, port).await {
                                writeln!(inner.printer.out(), "com{port} failed: {:#}", err).ok();
                            }
                        });
                        *task = Some(SerialTask { task: t, mode, req });
                    }
                }
            }
            VmCommand::Paravisor(cmd) => self.handle_paravisor_command(cmd).await?,
        }
        Ok(())
    }

    async fn handle_paravisor_command(&mut self, cmd: ParavisorCommand) -> anyhow::Result<()> {
        match cmd {
            ParavisorCommand::Start => {
                self.inner
                    .paravisor_diag
                    .start([], [])
                    .await
                    .context("start failed")?;

                writeln!(self.inner.printer.out(), "guest started within paravisor")?;
            }
            ParavisorCommand::Kmsg { mode: None } => {
                println!("{}", self.pv_kmsg.as_ref().map_or(LogMode::Off, |t| t.mode));
            }
            ParavisorCommand::Kmsg { mode: Some(mode) } => {
                let target = match mode {
                    LogMode::Off => {
                        if let Some(task) = self.pv_kmsg.take() {
                            drop(task.req);
                            task.task.await;
                        }
                        None
                    }
                    LogMode::Log => Some(IoTarget::Printer),
                    LogMode::Term => Some(IoTarget::Console(
                        console_relay::Console::new(self.inner.driver.clone(), None)
                            .context("failed to launch console")?,
                    )),
                };
                if let Some(target) = target {
                    if self
                        .pv_kmsg
                        .as_ref()
                        .is_some_and(|task| task.task.is_finished())
                    {
                        self.pv_kmsg = None;
                    }
                    if let Some(task) = &mut self.pv_kmsg {
                        task.mode = mode;
                        task.req.send(IoRequest::NewTarget(target));
                    } else {
                        let (req, recv) = mesh::channel();
                        let inner = self.inner.clone();
                        let t = self.inner.driver.spawn("kmsg", async move {
                            if let Err(err) = inner.handle_kmsg(recv, target).await {
                                writeln!(inner.printer.out(), "kmsg failed: {:#}", err).ok();
                            }
                        });
                        self.pv_kmsg = Some(KmsgTask { task: t, mode, req });
                    }
                }
            }
            ParavisorCommand::Inspect(InspectArgs {
                recursive,
                limit,
                update,
                element,
            }) => {
                if let Some(update) = update {
                    let value = self
                        .inner
                        .paravisor_diag
                        .update(element.unwrap_or_default(), update)
                        .await
                        .context("update failed")?;

                    println!("{:#}", value);
                } else {
                    let node = self
                        .inner
                        .paravisor_diag
                        .inspect(
                            element.unwrap_or_default(),
                            if recursive { limit } else { Some(0) },
                            Some(Duration::from_secs(1)),
                        )
                        .await
                        .context("inspect failed")?;

                    println!("{:#}", node);
                }
            }
            ParavisorCommand::CommandLine { command_line: None } => {
                let output = powershell_script(
                    r#"
                    param([string]$id)
                    $ErrorActionPreference = "Stop"
                    $vm = Get-CimInstance -namespace "root\virtualization\v2" -query "select * from Msvm_ComputerSystem where Name = '$id'"
                    $vssd = $vm | Get-CimAssociatedInstance -ResultClass "Msvm_VirtualSystemSettingData" -Association "Msvm_SettingsDefineState"
                    [System.Text.Encoding]::Default.GetString($vssd.FirmwareParameters)
                    "#,
                    &[&self.inner.id.to_string()],
                )
                .context("failed to query vssd")?;
                println!("{}", output.trim());
            }
            ParavisorCommand::CommandLine {
                command_line: Some(command_line),
            } => {
                let output = powershell_script(
                    r#"
                    param([string]$id, [string]$command_line)
                    $ErrorActionPreference = "Stop"
                    $vm = Get-CimInstance -namespace "root\virtualization\v2" -query "select * from Msvm_ComputerSystem where Name = '$id'"
                    $vssd = $vm | Get-CimAssociatedInstance -ResultClass "Msvm_VirtualSystemSettingData" -Association "Msvm_SettingsDefineState"
                    $vssd.FirmwareParameters = [System.Text.Encoding]::UTF8.GetBytes($command_line)
                    $vmms = Get-CimInstance -Namespace "root\virtualization\v2" -Class "Msvm_VirtualSystemManagementService"
                    $vmms | Invoke-CimMethod -Name "ModifySystemSettings" -Arguments @{"SystemSettings" = ($vssd | ConvertTo-CimEmbeddedString)}
                    $command_line
                    "#,
                    &[&self.inner.id.to_string(), &command_line],
                )
                .context("failed to update vssd")?;
                println!("{}", output.trim());
            }
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn state(&self) -> String {
        hvc_output(|cmd| cmd.arg("state").arg(&self.inner.name)).map_or_else(
            |_| "unknown".to_string(),
            |mut s| {
                s.truncate(s.trim_end().len());
                s
            },
        )
    }
}

enum IoRequest {
    NewTarget(IoTarget),
}

enum IoTarget {
    Printer,
    Console(console_relay::Console),
}

impl VmInner {
    async fn handle_serial(
        &self,
        mut req: mesh::Receiver<IoRequest>,
        mut target: IoTarget,
        port: u32,
    ) -> anyhow::Result<()> {
        let mut current_serial = None;

        enum Event {
            TaskDone(anyhow::Result<()>),
            Request(Option<IoRequest>),
        }

        loop {
            let task = async {
                let serial = if let Some(serial) = &mut current_serial {
                    serial
                } else {
                    let new_serial = diag_client::hyperv::open_serial_port(
                        &self.driver,
                        &self.name,
                        diag_client::hyperv::ComPortAccessInfo::PortNumber(port),
                    )
                    .await
                    .context("failed to open serial port")?;

                    writeln!(self.printer.out(), "com{port} connected").ok();

                    current_serial.insert(BufReader::new(
                        PolledPipe::new(&self.driver, new_serial)
                            .context("failed to create polled pipe")?,
                    ))
                };

                match &mut target {
                    IoTarget::Printer => {
                        let mut line = String::new();
                        while let Ok(n) = serial.read_line(&mut line).await {
                            if n == 0 {
                                break;
                            }
                            write!(self.printer.out(), "[com{port}]: {}", line).ok();
                            line.clear();
                        }
                    }
                    IoTarget::Console(console) => {
                        console.relay(serial).await?;
                    }
                }

                writeln!(self.printer.out(), "com{port} disconnected").ok();
                Ok(())
            };

            let event = (task.map(Event::TaskDone), req.next().map(Event::Request))
                .race()
                .await;
            match event {
                Event::TaskDone(r) => {
                    r?;
                    current_serial = None;
                }
                Event::Request(Some(y)) => match y {
                    IoRequest::NewTarget(new_target) => {
                        target = new_target;
                    }
                },
                Event::Request(None) => {
                    break;
                }
            }
        }

        if let Some(serial) = current_serial {
            drop(serial);
            writeln!(self.printer.out(), "com{port} disconnected").ok();
        }

        Ok(())
    }

    async fn handle_kmsg(
        &self,
        mut req: mesh::Receiver<IoRequest>,
        mut target: IoTarget,
    ) -> anyhow::Result<()> {
        let mut current = None;

        enum Event {
            TaskDone(anyhow::Result<()>),
            Request(Option<IoRequest>),
        }

        loop {
            let task = async {
                let kmsg = if let Some(kmsg) = &mut current {
                    kmsg
                } else {
                    self.paravisor_diag.wait_for_server().await?;
                    let new_kmsg = self
                        .paravisor_diag
                        .kmsg(true)
                        .await
                        .context("failed to open kmsg stream")?;

                    writeln!(self.printer.out(), "kmsg connected").ok();

                    current.insert(new_kmsg)
                };

                while let Some(data) = kmsg.next().await {
                    match data {
                        Ok(data) => {
                            let message = kmsg::KmsgParsedEntry::new(&data)?;
                            match &mut target {
                                IoTarget::Printer => {
                                    writeln!(
                                        self.printer.out(),
                                        "[kmsg]: {}",
                                        message.display(true)
                                    )
                                    .ok();
                                }
                                IoTarget::Console(console) => {
                                    let line = format!("{}\r\n", message.display(true));
                                    console.write_all(line.as_bytes()).await?;
                                }
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                            break;
                        }
                        Err(err) => {
                            writeln!(
                                self.printer.out(),
                                "kmsg failure: {:#}",
                                anyhow::Error::from(err)
                            )
                            .ok();
                            return Ok(());
                        }
                    }
                }

                writeln!(self.printer.out(), "kmsg disconnected").ok();
                Ok(())
            };

            let event = (task.map(Event::TaskDone), req.next().map(Event::Request))
                .race()
                .await;
            match event {
                Event::TaskDone(r) => {
                    current = None;
                    r?;
                }
                Event::Request(Some(y)) => match y {
                    IoRequest::NewTarget(new_target) => {
                        target = new_target;
                    }
                },
                Event::Request(None) => {
                    break;
                }
            }
        }

        if let Some(kmsg) = current {
            drop(kmsg);
            writeln!(self.printer.out(), "kmsg disconnected").ok();
        }

        Ok(())
    }
}
