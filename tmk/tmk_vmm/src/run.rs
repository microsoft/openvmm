// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for running a VM's VPs.

use crate::Options;
use crate::load;
use anyhow::Context as _;
use futures::StreamExt as _;
use guestmem::GuestMemory;
use hvdef::HvError;
use hvdef::Vtl;
use pal_async::DefaultDriver;
use std::sync::Arc;
use virt::PartitionCapabilities;
use virt::Processor;
use virt::StopVpSource;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::vp::AccessVpState as _;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TopologyBuilder;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use vmcore::vmtime::VmTimeSource;
use zerocopy::TryFromBytes as _;

pub struct CommonState {
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    pub driver: DefaultDriver,
    pub vmtime_keeper: VmTimeKeeper,
    pub vmtime_source: VmTimeSource,
    pub opts: Options,
    pub processor_topology: ProcessorTopology,
    pub memory_layout: MemoryLayout,
}

impl CommonState {
    pub async fn new(driver: DefaultDriver, opts: Options) -> anyhow::Result<Self> {
        let vmtime_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime_source = vmtime_keeper.builder().build(&driver).await.unwrap();
        #[cfg(guest_arch = "x86_64")]
        let processor_topology = TopologyBuilder::new_x86()
            .build(1)
            .context("failed to build processor topology")?;

        #[cfg(guest_arch = "aarch64")]
        let processor_topology =
            TopologyBuilder::new_aarch64(vm_topology::processor::arch::GicInfo {
                gic_distributor_base: 0xff000000,
                gic_redistributors_base: 0xff020000,
            })
            .build(1)
            .context("failed to build processor topology")?;

        let ram_size = 0x400000;
        let memory_layout = MemoryLayout::new(ram_size, &[], None).context("bad memory layout")?;

        Ok(Self {
            driver,
            vmtime_keeper,
            vmtime_source,
            opts,
            processor_topology,
            memory_layout,
        })
    }

    pub async fn run(
        &mut self,
        guest_memory: &GuestMemory,
        caps: &PartitionCapabilities,
        start_vp: impl AsyncFnOnce(&mut Self, RunnerBuilder) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let (event_send, mut event_recv) = mesh::channel();

        // Load the TMK.
        let tmk = fs_err::File::open(&self.opts.tmk).context("failed to open tmk")?;
        let regs = {
            #[cfg(guest_arch = "x86_64")]
            {
                load::load_x86(
                    &self.memory_layout,
                    guest_memory,
                    &self.processor_topology,
                    caps,
                    &tmk,
                )?
            }
            #[cfg(guest_arch = "aarch64")]
            {
                load::load_aarch64(
                    &self.memory_layout,
                    guest_memory,
                    &self.processor_topology,
                    caps,
                    &tmk,
                )?
            }
        };

        self.vmtime_keeper.start().await;

        start_vp(
            self,
            RunnerBuilder::new(
                VpIndex::BSP,
                Arc::clone(&regs),
                guest_memory.clone(),
                event_send.clone(),
            ),
        )
        .await?;

        let event = event_recv.next().await.unwrap();
        match event {
            VpEvent::TestComplete { success } => {
                if !success {
                    anyhow::bail!("test failed");
                }
                tracing::info!("test complete");
                Ok(())
            }
            VpEvent::Halt {
                vp_index,
                reason,
                regs,
            } => {
                anyhow::bail!(
                    "vp {} halted: {}\nregisters:\n{:#x?}",
                    vp_index.index(),
                    reason,
                    regs
                );
            }
        }
    }
}

enum VpEvent {
    TestComplete {
        success: bool,
    },
    Halt {
        vp_index: VpIndex,
        reason: String,
        regs: Option<Box<virt::vp::Registers>>,
    },
}

struct IoHandler<'a> {
    guest_memory: &'a GuestMemory,
    event_send: &'a mesh::Sender<VpEvent>,
    stop: &'a StopVpSource,
}

fn widen(d: &[u8]) -> u64 {
    let mut v = [0; 8];
    v[..d.len()].copy_from_slice(d);
    u64::from_ne_bytes(v)
}

impl CpuIo for IoHandler<'_> {
    fn is_mmio(&self, _address: u64) -> bool {
        false
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        None
    }

    fn handle_eoi(&self, irq: u32) {
        tracing::info!(irq, "eoi");
    }

    fn signal_synic_event(&self, vtl: Vtl, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        let _ = (vtl, connection_id, flag);
        Err(HvError::InvalidConnectionId)
    }

    fn post_synic_message(
        &self,
        vtl: Vtl,
        connection_id: u32,
        secure: bool,
        message: &[u8],
    ) -> hvdef::HvResult<()> {
        let _ = (vtl, connection_id, secure, message);
        Err(HvError::InvalidConnectionId)
    }

    async fn read_mmio(&self, vp: VpIndex, address: u64, data: &mut [u8]) {
        tracing::info!(vp = vp.index(), address, "read mmio");
        data.fill(!0);
    }

    async fn write_mmio(&self, vp: VpIndex, address: u64, data: &[u8]) {
        if address == tmk_protocol::COMMAND_ADDRESS {
            let p = widen(data);
            let r = self.handle_command(p);
            if let Err(e) = r {
                tracing::error!(
                    error = e.as_ref() as &dyn std::error::Error,
                    p,
                    "failed to handle command"
                );
            }
        } else {
            tracing::info!(vp = vp.index(), address, data = widen(data), "write mmio");
        }
    }

    async fn read_io(&self, vp: VpIndex, port: u16, data: &mut [u8]) {
        tracing::info!(vp = vp.index(), port, "read io");
        data.fill(!0);
    }

    async fn write_io(&self, vp: VpIndex, port: u16, data: &[u8]) {
        tracing::info!(vp = vp.index(), port, data = widen(data), "write io");
    }
}

impl IoHandler<'_> {
    fn read_str(&self, s: tmk_protocol::Str) -> anyhow::Result<String> {
        let mut buf = vec![0; s.len as usize];
        self.guest_memory
            .read_at(s.gpa, &mut buf)
            .context("failed to read string")?;
        String::from_utf8(buf).context("string not utf-8")
    }

    fn handle_command(&self, gpa: u64) -> anyhow::Result<()> {
        let buf = self
            .guest_memory
            .read_plain::<[u8; size_of::<tmk_protocol::Command>()]>(gpa)
            .context("failed to read command")?;
        let cmd = tmk_protocol::Command::try_read_from_bytes(&buf)
            .ok()
            .context("bad command")?;
        match cmd {
            tmk_protocol::Command::Log(s) => {
                let message = self.read_str(s)?;
                tracing::info!(target: "tmk", message);
            }
            tmk_protocol::Command::Panic {
                message,
                filename,
                line,
            } => {
                let message = self.read_str(message)?;
                let location = if filename.len > 0 {
                    Some(format!("{}:{}", self.read_str(filename)?, line))
                } else {
                    None
                };
                tracing::error!(target: "tmk", location, panic = message);
                self.event_send
                    .send(VpEvent::TestComplete { success: false });
                self.stop.stop();
            }
            tmk_protocol::Command::Complete { success } => {
                self.event_send.send(VpEvent::TestComplete { success });
                self.stop.stop();
            }
        }
        Ok(())
    }
}

pub struct RunnerBuilder {
    vp_index: VpIndex,
    regs: Arc<virt::InitialRegs>,
    guest_memory: GuestMemory,
    event_send: mesh::Sender<VpEvent>,
}

impl RunnerBuilder {
    fn new(
        vp_index: VpIndex,
        regs: Arc<virt::InitialRegs>,
        guest_memory: GuestMemory,
        event_send: mesh::Sender<VpEvent>,
    ) -> Self {
        Self {
            vp_index,
            regs,
            guest_memory,
            event_send,
        }
    }

    pub fn build<P: Processor>(&mut self, mut vp: P) -> anyhow::Result<Runner<'_, P>> {
        {
            let mut state = vp.access_state(Vtl::Vtl0);
            #[cfg(guest_arch = "x86_64")]
            {
                let virt::x86::X86InitialRegs {
                    registers,
                    mtrrs,
                    pat,
                } = self.regs.as_ref();
                state.set_registers(registers)?;
                state.set_mtrrs(mtrrs)?;
                state.set_pat(pat)?;
            }
            #[cfg(guest_arch = "aarch64")]
            {
                let virt::aarch64::Aarch64InitialRegs {
                    registers,
                    system_registers,
                } = self.regs.as_ref();
                state.set_registers(registers)?;
                state.set_system_registers(system_registers)?;
            }
            state.commit()?;
        }
        Ok(Runner {
            vp,
            vp_index: self.vp_index,
            guest_memory: &self.guest_memory,
            event_send: &self.event_send,
        })
    }
}

pub struct Runner<'a, P> {
    vp: P,
    vp_index: VpIndex,
    guest_memory: &'a GuestMemory,
    event_send: &'a mesh::Sender<VpEvent>,
}

impl<P: Processor> Runner<'_, P> {
    pub async fn run_vp(&mut self) {
        let stop = StopVpSource::new();
        let Err(err) = self
            .vp
            .run_vp(
                stop.checker(),
                &IoHandler {
                    guest_memory: self.guest_memory,
                    event_send: self.event_send,
                    stop: &stop,
                },
            )
            .await;
        let regs = self
            .vp
            .access_state(Vtl::Vtl0)
            .registers()
            .map(Box::new)
            .ok();
        self.event_send.send(VpEvent::Halt {
            vp_index: self.vp_index,
            reason: format!("{:?}", err),
            regs,
        });
    }
}
