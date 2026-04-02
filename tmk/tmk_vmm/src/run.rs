// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for running a VM's VPs.
use crate::HypervisorOpt;
// use std::{io, os::unix::io::AsRawFd, ptr};
use crate::Options;
use crate::load;
use anyhow::Context as _;
use futures::StreamExt as _;
use guestmem::GuestMemory;
use hvdef::HvError;
use hvdef::Vtl;
use hcl::ioctl::cca::Addresses;
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
use std::fs::OpenOptions;
use vm_topology::memory::MemoryRangeWithNode;
use memory_range::MemoryRange;
use core::ops::Range;
use std::num::NonZeroUsize;
use nix::{
    sys::{
        mman::{MapFlags, ProtFlags, mmap},
        // statfs::statfs,
    },
    // unistd::{ftruncate, mkstemp, unlink},
};

//temp
// use crate::mapped_page::MappedPage;

pub const COMMAND_ADDRESS: u64 = 0xffff_0000;

pub struct Mmemory {
    pub startpa: u64,
    pub endpa: u64,
}

pub struct CommonState {
    pub driver: DefaultDriver,
    pub opts: Options,
    pub processor_topology: ProcessorTopology,
    pub memory_layout: MemoryLayout,
    pub shared_memory_layout: MemoryLayout,
    pub offset_memory: Option<u64>,
    pub mmemory: Option<Mmemory>,
    pub addresses: Option<Addresses>,
    
}

pub struct RunContext<'a> {
    pub state: &'a CommonState,
    pub vmtime_source: &'a VmTimeSource,
}

#[derive(Debug, Clone)]
pub enum TestResult {
    Passed,
    Failed,
    Faulted {
        vp_index: VpIndex,
        reason: String,
        regs: Option<Box<virt::vp::Registers>>,
    },
}

impl CommonState {
    pub async fn new(driver: DefaultDriver, opts: Options, hv: HypervisorOpt) -> anyhow::Result<Self> {
        #[cfg(guest_arch = "x86_64")]
        let processor_topology = TopologyBuilder::new_x86()
            .x2apic(vm_topology::processor::x86::X2ApicState::Supported)
            .build(1)
            .context("failed to build processor topology")?;

        #[cfg(guest_arch = "aarch64")]
        let processor_topology = TopologyBuilder::new_aarch64(
            vm_topology::processor::arch::GicInfo {
                gic_distributor_base: 0xff000000,
                gic_redistributors_base: 0xff020000,
            },
            0,
        )
        .build(1)
        .context("failed to build processor topology")?;

        let ram_size = 0x400000;

        
        let mut memory_layout = MemoryLayout::new(ram_size, &[], &[], &[], None).context("bad memory layout")?;
        let mut shared_memory_layout = MemoryLayout::new(ram_size, &[], &[], &[], None).context("bad memory layout")?;
        let mut mmemory = None;
        let mut offset_memory  = None;

        let addresses = match hv {
            #[cfg(target_os = "linux")]
            HypervisorOpt::Kvm => { None },
            #[cfg(all(target_os = "linux", guest_arch = "x86_64"))]
            HypervisorOpt::Mshv => { None },
            #[cfg(target_os = "linux")]
            HypervisorOpt::MshvVtl => { None }
            #[cfg(all(target_os = "linux", guest_arch = "aarch64"))]
            HypervisorOpt::Cca => { 
                let map_size = ram_size;
                let non_zero_size =NonZeroUsize::new(map_size as usize).expect("Size was already checked to be non-zero");
                let file = OpenOptions::new().read(true).write(true).open("/dev/zero")?;
                #[allow(unsafe_code)]
                let addr = unsafe {
                    mmap(
                        None,
                        non_zero_size,
                        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                        MapFlags::MAP_SHARED,
                        &file,
                        0,
                    )
                }
                .context("Failed to memory-map bytes")?;

                #[allow(unsafe_code)]
                unsafe {
                        std::ptr::write_bytes(addr.as_ptr() as *mut u8, 0, map_size as usize);
                    }

                #[allow(unsafe_code)]
                let pa = unsafe { load::virt_to_phys(addr.as_ptr() as u64) }
                        .map_err(anyhow::Error::msg)
                        .context("failed to get page physical address")?;

                const PAGE: u64 = 4096;
                const ALIGN: u64 = PAGE * 8;

                let raw_start = pa;
                let raw_end = pa + map_size/2;

                let start = (raw_start + ALIGN - 1) & !(ALIGN - 1);
                let end   = raw_end & !(ALIGN - 1);

                // #[cfg(guest_arch = "aarch64")]
                memory_layout = MemoryLayout::new_from_ranges(
                        &[MemoryRangeWithNode {
                            range: MemoryRange::new(Range {
                                start,
                                end,
                            }),
                            vnode: 0,
                        }],
                        &[],
                    )
                    .context("bad memory layout")?;

                offset_memory = Some(start);

                mmemory = Some(Mmemory {
                        startpa: start,
                        endpa: end,
                    });
                
                let aligned = ((addr.as_ptr() as u64) + ALIGN - 1) & !(ALIGN - 1);
                let shared_virtual_address_start = aligned + map_size / 2;
                let shared_virtual_address_start_command = aligned + map_size / 2 + 8;

                #[allow(unsafe_code)]
                let shared_address_start = unsafe { load::virt_to_phys(shared_virtual_address_start) }
                        .map_err(anyhow::Error::msg)
                        .context("failed to get page physical address")?;

                // #[cfg(guest_arch = "aarch64")]
                shared_memory_layout = MemoryLayout::new_from_ranges(
                        &[MemoryRangeWithNode {
                            range: MemoryRange::new(Range {
                                start: shared_address_start,
                                end: shared_address_start + map_size/2,
                            }),
                            vnode: 0,
                        }],
                        &[],
                    )
                    .context("bad memory layout")?;

                let shared_address_start_command = start;

                Some (Addresses {
                    shared_address_start,
                    shared_virtual_address_start,
                    shared_address_start_command,
                    shared_virtual_address_start_command,
                } )
            }
            #[cfg(windows)]
            HypervisorOpt::Whp => { None }
            #[cfg(target_os = "macos")]
            HypervisorOpt::Hvf => { None }
        };

        Ok(Self {
            driver,
            opts,
            processor_topology,
            memory_layout,
            shared_memory_layout,
            offset_memory,
            mmemory,
            addresses,
        })
    }

    pub async fn for_each_test(
        &mut self,
        mut f: impl AsyncFnMut(&mut RunContext<'_>, &load::TestInfo) -> anyhow::Result<TestResult>,
    ) -> anyhow::Result<()> {
        let tmk = fs_err::File::open(&self.opts.tmk).context("failed to open tmk")?;
        let available_tests = load::enumerate_tests(&tmk)?;
        let tests = if self.opts.tests.is_empty() {
            available_tests
        } else {
            self.opts
                .tests
                .iter()
                .map(|name| {
                    available_tests
                        .iter()
                        .find(|test| test.name == *name)
                        .cloned()
                        .with_context(|| format!("test {} not found", name))
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        let mut success = true;
        for test in &tests {
            tracing::info!(target: "test", name = test.name, "test started");

            let mut vmtime_keeper = VmTimeKeeper::new(&self.driver, VmTime::from_100ns(0));
            let vmtime_source = vmtime_keeper.builder().build(&self.driver).await.unwrap();
            let mut ctx = RunContext {
                state: self,
                vmtime_source: &vmtime_source,
            };

            vmtime_keeper.start().await;

            let r = f(&mut ctx, test)
                .await
                .with_context(|| format!("failed to run test {}", test.name))?;

            vmtime_keeper.stop().await;

            match r {
                TestResult::Passed => {
                    tracing::info!(target: "test", name = test.name, "test passed");
                }
                TestResult::Failed => {
                    tracing::error!(target: "test", name = test.name, reason = "explicit failure", "test failed");
                    success = false;
                }
                TestResult::Faulted {
                    vp_index,
                    reason,
                    regs,
                } => {
                    tracing::error!(
                        target: "test",
                        name = test.name,
                        vp_index = vp_index.index(),
                        reason,
                        regs = format_args!("{:#x?}", regs),
                        "test failed"
                    );
                    success = false;
                }
            }
        }
        if !success {
            anyhow::bail!("some tests failed");
        }
        Ok(())
    }
}

impl RunContext<'_> {
    pub async fn run(
        &mut self,
        guest_memory: &GuestMemory,
        caps: &PartitionCapabilities,
        test: &load::TestInfo,
        start_vp: impl AsyncFnOnce(&mut Self, RunnerBuilder) -> anyhow::Result<()>,
    ) -> anyhow::Result<TestResult> {
        let (event_send, mut event_recv) = mesh::channel();

        // Load the TMK.
        let tmk = fs_err::File::open(&self.state.opts.tmk).context("failed to open tmk")?;

        let regs = {
            #[cfg(guest_arch = "x86_64")]
            {
                load::load_x86(
                    &self.state.memory_layout,
                    guest_memory,
                    &self.state.processor_topology,
                    caps,
                    &tmk,
                    test,
                )?
            }
            #[cfg(guest_arch = "aarch64")]
            {
                load::load_aarch64(
                    self.state.offset_memory,
                    &self.state.memory_layout,
                    guest_memory,
                    &self.state.processor_topology,
                    caps,
                    &tmk,
                    test,
                )?
            }
        };

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
        let r = match event {
            VpEvent::TestComplete { success } => {
                if success {
                    TestResult::Passed
                } else {
                    TestResult::Failed
                }
            }
            VpEvent::Halt {
                vp_index,
                reason,
                regs,
            } => TestResult::Faulted {
                vp_index,
                reason,
                regs,
            },
        };

        Ok(r)
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
        if address == COMMAND_ADDRESS {
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

    #[track_caller]
    fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> virt::VpHaltReason {
        tracing::error!(
            err = error.as_ref() as &dyn std::error::Error,
            "fatal error"
        );
        virt::VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
    }
}

impl IoHandler<'_> {
    fn read_str(&self, s: tmk_protocol::StrDescriptor) -> anyhow::Result<String> {
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
