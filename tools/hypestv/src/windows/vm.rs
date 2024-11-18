// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM command handling.

use super::hyperv::hvc_output;
use super::hyperv::run_hvc;
use super::rustyline_printer::Printer;
use super::InspectTarget;
use super::SerialMode;
use super::VmCommand;
use anyhow::Context as _;
use diag_client::DiagClient;
use futures::io::BufReader;
use futures::AsyncBufReadExt;
use guid::Guid;
use pal_async::pipe::PolledPipe;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::DefaultDriver;
use std::sync::Arc;
use std::time::Duration;

pub struct Vm {
    paravisor_diag: Option<DiagClient>,
    inner: Arc<VmInner>,
    serial: Vec<Option<Task<()>>>,
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
            driver,
            printer,
            name,
            id,
        });
        Ok(Self {
            paravisor_diag: None,
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

    async fn diag_client(&mut self) -> anyhow::Result<&mut DiagClient> {
        if self.paravisor_diag.is_none() {
            let diag = DiagClient::from_hyperv_id(self.inner.driver.clone(), self.inner.id)
                .await
                .context("failed to connect to paravisor")?;
            self.paravisor_diag = Some(diag);
        }
        Ok(self.paravisor_diag.as_mut().unwrap())
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
                self.diag_client()
                    .await?
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
            VmCommand::Kill => self.delay(move |inner| {
                run_hvc(|cmd| cmd.arg("kill").arg(&inner.name))?;
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
            VmCommand::Serial { port, mode } => {
                let port_index = port.checked_sub(1).context("invalid port")? as usize;
                if let Some(task) = self
                    .serial
                    .get_mut(port_index)
                    .context("invalid port")?
                    .take()
                {
                    // TODO: preserve the existing serial port connection if
                    // changing between non-off modes.
                    task.cancel().await;
                }
                match mode {
                    SerialMode::Off => {}
                    SerialMode::Output => {
                        let inner = self.inner.clone();
                        let task = self.inner.driver.spawn("serial", async move {
                            if let Err(err) = inner.handle_serial(port).await {
                                writeln!(
                                    inner.printer.out(),
                                    "serial port {port} failed: {:#}",
                                    err
                                )
                                .ok();
                                return;
                            }
                        });
                        self.serial[port_index] = Some(task);
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
                    let value = diag
                        .update(element.unwrap_or_default(), update)
                        .await
                        .context("update failed")?;

                    println!("{:#}", value);
                } else {
                    let node = diag
                        .inspect(
                            element.unwrap_or_default(),
                            if recursive { limit } else { Some(0) },
                            Some(Duration::from_secs(1)),
                        )
                        .await
                        .inspect_err(|_| {
                            self.paravisor_diag = None;
                        })
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

impl VmInner {
    async fn handle_serial(&self, port: u32) -> anyhow::Result<()> {
        loop {
            let serial = diag_client::hyperv::open_serial_port(
                &self.driver,
                &self.name,
                diag_client::hyperv::ComPortAccessInfo::PortNumber(port),
            )
            .await
            .with_context(|| format!("failed to open serial port"))?;

            writeln!(self.printer.out(), "serial port {port} connected").ok();

            let mut serial = BufReader::new(
                PolledPipe::new(&self.driver, serial).context("failed to create polled pipe")?,
            );

            let mut line = String::new();
            while let Ok(n) = serial.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                write!(self.printer.out(), "{}", line).ok();
                line.clear();
            }
            writeln!(self.printer.out(), "serial port {port} disconnected").ok();
        }
    }
}
