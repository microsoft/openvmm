// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM command handling.

use super::hyperv::hvc_output;
use super::hyperv::run_hcsdiag;
use super::hyperv::run_hvc;
use super::rustyline_printer::Printer;
use super::InspectTarget;
use super::SerialMode;
use super::VmCommand;
use anyhow::Context as _;
use diag_client::DiagClient;
use futures::io::BufReader;
use futures::AsyncBufReadExt;
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
    paravisor_diag: DiagClient,
    inner: Arc<VmInner>,
    serial: Vec<Option<SerialTask>>,
}

struct SerialTask {
    mode: SerialMode,
    task: Task<()>,
    req: mesh::Sender<SerialRequest>,
}

struct VmInner {
    driver: DefaultDriver,
    name: String,
    id: Guid,
    printer: Printer,
}

impl Vm {
    pub fn new(driver: DefaultDriver, name: String, printer: Printer) -> anyhow::Result<Self> {
        let id = diag_client::hyperv::vm_id_from_name(&name).context("failed to get vm id")?;
        let inner = Arc::new(VmInner {
            driver: driver.clone(),
            printer,
            name,
            id,
        });
        Ok(Self {
            paravisor_diag: DiagClient::from_hyperv_id(driver, id),
            serial: (0..4).map(|_| None).collect(),
            inner,
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
            InspectTarget::Host => {
                anyhow::bail!("host inspect not supported yet");
            }
            InspectTarget::Paravisor => {
                self.paravisor_diag
                    .inspect(path, Some(0), Some(Duration::from_secs(1)))
                    .await
            }
        }
    }

    pub async fn handle_command(&mut self, cmd: VmCommand) -> anyhow::Result<()> {
        match cmd {
            VmCommand::Start { paravisor } => {
                if paravisor {
                    let diag = self.diag_client().await?;
                    diag.start([], []).await.context("start failed")?;
                    writeln!(self.inner.printer.out(), "guest started within paravisor")?;
                } else {
                    self.delay(move |inner| {
                        run_hvc(|cmd| cmd.arg("start").arg(&inner.name))?;
                        writeln!(inner.printer.out(), "VM started")?;
                        Ok(())
                    })
                }
            }
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
                    SerialMode::Log => Some(SerialTarget::Printer),
                    SerialMode::Term => Some(SerialTarget::Console(
                        console_relay::Console::new(self.inner.driver.clone(), None)
                            .context("failed to launch console")?,
                    )),
                };
                if let Some(target) = target {
                    if let Some(task) = task {
                        task.mode = mode;
                        task.req.send(SerialRequest::NewTarget(target));
                    } else {
                        let (req, recv) = mesh::channel();
                        let inner = self.inner.clone();
                        let t = self.inner.driver.spawn("serial", async move {
                            if let Err(err) = inner.handle_serial(recv, target, port).await {
                                writeln!(inner.printer.out(), "COM{port} failed: {:#}", err).ok();
                            }
                        });
                        *task = Some(SerialTask { task: t, mode, req });
                    }
                }
            }
            VmCommand::Inspect {
                recursive,
                limit,
                paravisor,
                update,
                element,
            } => {
                if !paravisor {
                    anyhow::bail!("no host inspect yet");
                }
                let diag = self.diag_client().await?;
                if let Some(update) = update {
                    let value = self
                        .paravisor_diag
                        .update(element.unwrap_or_default(), update)
                        .await
                        .context("update failed")?;

                    println!("{:#}", value);
                } else {
                    let node = self
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

enum SerialRequest {
    NewTarget(SerialTarget),
}

enum SerialTarget {
    Printer,
    Console(console_relay::Console),
}

impl VmInner {
    async fn handle_serial(
        &self,
        mut req: mesh::Receiver<SerialRequest>,
        mut target: SerialTarget,
        port: u32,
    ) -> anyhow::Result<()> {
        let mut current_serial = None;

        enum Event {
            TaskDone(anyhow::Result<()>),
            Request(Option<SerialRequest>),
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

                    current_serial.insert(BufReader::new(
                        PolledPipe::new(&self.driver, new_serial)
                            .context("failed to create polled pipe")?,
                    ))
                };

                writeln!(self.printer.out(), "COM{port} connected").ok();

                match &mut target {
                    SerialTarget::Printer => {
                        let mut line = String::new();
                        while let Ok(n) = serial.read_line(&mut line).await {
                            if n == 0 {
                                break;
                            }
                            write!(self.printer.out(), "[COM{port}]: {}", line).ok();
                            line.clear();
                        }
                    }
                    SerialTarget::Console(console) => {
                        console.relay(serial).await?;
                    }
                }

                writeln!(self.printer.out(), "COM{port} disconnected").ok();
                current_serial = None;
                Ok(())
            };

            let event = (task.map(Event::TaskDone), req.next().map(Event::Request))
                .race()
                .await;
            match event {
                Event::TaskDone(r) => r?,
                Event::Request(Some(y)) => match y {
                    SerialRequest::NewTarget(new_target) => {
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
            writeln!(self.printer.out(), "COM{port} disconnected").ok();
        }

        Ok(())
    }
}
