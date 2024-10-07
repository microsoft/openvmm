// Copyright (C) Microsoft Corporation. All rights reserved.

//! Implementation of an admin or IO queue pair.

use super::spec;
use crate::page_allocator::PageAllocator;
use crate::page_allocator::ScopedPages;
use crate::queues::CompletionQueue;
use crate::queues::CompletionQueueSavedState;
use crate::queues::SubmissionQueue;
use crate::queues::SubmissionQueueSavedState;
use crate::registers::DeviceRegisters;
use anyhow::Context;
use futures::StreamExt;
use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use inspect_counters::Counter;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh::Cancel;
use mesh::CancelContext;
use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use safeatomic::AtomicSliceOps;
use slab::Slab;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;
use thiserror::Error;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;
use user_driver::memory::PAGE_SIZE64;
use user_driver::DeviceBacking;
use user_driver::HostDmaAllocator;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

/// Value for unused PRP entries, to catch/mitigate buffer size mismatches.
const INVALID_PAGE_ADDR: u64 = !(PAGE_SIZE as u64 - 1);

pub(crate) struct QueuePair {
    task: Task<QueueHandler>,
    cancel: Cancel,
    issuer: Arc<Issuer>,
    mem: MemoryBlock,
}

impl Inspect for QueuePair {
    fn inspect(&self, req: inspect::Request<'_>) {
        let Self {
            task: _,
            cancel: _,
            issuer,
            mem: _,
        } = self;
        issuer.send.send(Req::Inspect(req.defer()));
    }
}

impl QueuePair {
    pub const MAX_SQSIZE: u16 = (PAGE_SIZE / 64) as u16; // Maximum SQ size in entries.
    pub const MAX_CQSIZE: u16 = (PAGE_SIZE / 16) as u16; // Maximum CQ size in entries.

    pub fn new(
        spawner: impl SpawnDriver,
        device: &impl DeviceBacking,
        qid: u16,
        sq_size: u16,
        cq_size: u16,
        mut interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
    ) -> anyhow::Result<Self> {
        let mem = device
            .host_allocator()
            .allocate_dma_buffer(PAGE_SIZE * 2)
            .context("failed to allocate memory for queues")?;

        assert!(sq_size <= Self::MAX_SQSIZE);
        assert!(cq_size <= Self::MAX_CQSIZE);

        let sq = SubmissionQueue::new(qid, sq_size, mem.subblock(0, PAGE_SIZE));
        let cq = CompletionQueue::new(qid, cq_size, mem.subblock(PAGE_SIZE, PAGE_SIZE));

        let (send, recv) = mesh::channel();
        let (mut ctx, cancel) = CancelContext::new().with_cancel();
        let mut queue_handler = QueueHandler {
            sq,
            cq,
            commands: Slab::new(),
            max_cids: 1024,
            stats: Default::default(),
        };
        let task = spawner.spawn("nvme-queue", {
            async move {
                ctx.until_cancelled(async {
                    queue_handler.run(&registers, recv, &mut interrupt).await;
                })
                .await
                .ok();
                queue_handler
            }
        });

        const PER_QUEUE_PAGES: usize = 128;
        #[allow(clippy::assertions_on_constants)]
        const _: () = assert!(
            PER_QUEUE_PAGES * PAGE_SIZE >= 128 * 1024 + PAGE_SIZE,
            "not enough room for an ATAPI IO plus a PRP list"
        );
        let alloc = PageAllocator::new(
            device
                .host_allocator()
                .allocate_dma_buffer(PER_QUEUE_PAGES * PAGE_SIZE)
                .context("failed to allocate pages for queue requests")?,
        );

        Ok(Self {
            task,
            cancel,
            issuer: Arc::new(Issuer { send, alloc }),
            mem,
        })
    }

    pub fn sq_addr(&self) -> u64 {
        self.mem.pfns()[0] * PAGE_SIZE64
    }

    pub fn cq_addr(&self) -> u64 {
        self.mem.pfns()[1] * PAGE_SIZE64
    }

    pub fn issuer(&self) -> &Arc<Issuer> {
        &self.issuer
    }

    pub async fn shutdown(mut self) -> impl Send {
        self.cancel.cancel();
        self.task.await
    }

    /// Save queue pair state for servicing.
    pub async fn save(&self) -> anyhow::Result<QueuePairSavedState> {
        // Send an RPC request to QueueHandler thread to save its data.
        let queue_data = self.issuer.send.call(Req::Save, ()).await?;

        // Add more data to the returned response.
        let mut local_queue_data = queue_data.unwrap();
        local_queue_data.sq_addr = self.sq_addr();
        local_queue_data.cq_addr = self.cq_addr();

        local_queue_data.base_mem = Some(self.mem.base_va());
        local_queue_data.mem_len = Some(self.mem.len());
        local_queue_data.pfns = Some(self.mem.pfns().to_vec());

        Ok(local_queue_data)
    }
}

/// An error issuing an NVMe request.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum RequestError {
    #[error("queue pair is gone")]
    Gone(#[source] mesh::RecvError),
    #[error("nvme error")]
    Nvme(#[source] NvmeError),
    #[error("memory error")]
    Memory(#[source] GuestMemoryError),
    #[error("i/o too large for double buffering")]
    TooLarge,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NvmeError(spec::Status);

impl NvmeError {
    pub fn status(&self) -> spec::Status {
        self.0
    }
}

impl From<spec::Status> for NvmeError {
    fn from(value: spec::Status) -> Self {
        Self(value)
    }
}

impl std::error::Error for NvmeError {}

impl std::fmt::Display for NvmeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.status_code_type() {
            spec::StatusCodeType::GENERIC => write!(f, "general error {:#x?}", self.0),
            spec::StatusCodeType::COMMAND_SPECIFIC => {
                write!(f, "command-specific error {:#x?}", self.0)
            }
            spec::StatusCodeType::MEDIA_ERROR => {
                write!(f, "media error {:#x?}", self.0)
            }
            _ => write!(f, "{:#x?}", self.0),
        }
    }
}

#[derive(Debug, Inspect)]
pub struct Issuer {
    #[inspect(skip)]
    send: mesh::Sender<Req>,
    alloc: PageAllocator,
}

impl Issuer {
    pub async fn issue_raw(
        &self,
        command: spec::Command,
    ) -> Result<spec::Completion, RequestError> {
        match self.send.call(Req::Command, command).await {
            Ok(completion) if completion.status.status() == 0 => Ok(completion),
            Ok(completion) => Err(RequestError::Nvme(NvmeError(spec::Status(
                completion.status.status(),
            )))),
            Err(err) => Err(RequestError::Gone(err)),
        }
    }

    pub async fn issue_external(
        &self,
        mut command: spec::Command,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<spec::Completion, RequestError> {
        let mut double_buffer_pages = None;
        let opcode = spec::Opcode(command.cdw0.opcode());
        assert!(
            opcode.transfer_controller_to_host()
                || opcode.transfer_host_to_controller()
                || mem.is_empty()
        );

        // Ensure the memory is currently mapped.
        guest_memory
            .probe_gpns(mem.gpns())
            .map_err(RequestError::Memory)?;

        let prp = if mem
            .gpns()
            .iter()
            .all(|&gpn| guest_memory.iova(gpn * PAGE_SIZE64).is_some())
        {
            // Guest memory is available to the device, so issue the IO directly.
            self.make_prp(
                mem.offset() as u64,
                mem.gpns()
                    .iter()
                    .map(|&gpn| guest_memory.iova(gpn * PAGE_SIZE64).unwrap()),
            )
            .await
        } else {
            tracing::debug!(opcode = opcode.0, size = mem.len(), "double buffering");

            // Guest memory is not accessible by the device. Double buffer
            // through an allocation.
            let double_buffer_pages = double_buffer_pages.insert(
                self.alloc
                    .alloc_bytes(mem.len())
                    .await
                    .ok_or(RequestError::TooLarge)?,
            );

            if opcode.transfer_host_to_controller() {
                double_buffer_pages
                    .copy_from_guest_memory(guest_memory, mem)
                    .map_err(RequestError::Memory)?;
            }

            self.make_prp(
                0,
                (0..double_buffer_pages.page_count())
                    .map(|i| double_buffer_pages.physical_address(i)),
            )
            .await
        };

        command.dptr = prp.dptr;
        let r = self.issue_raw(command).await;
        if let Some(double_buffer_pages) = double_buffer_pages {
            if r.is_ok() && opcode.transfer_controller_to_host() {
                double_buffer_pages
                    .copy_to_guest_memory(guest_memory, mem)
                    .map_err(RequestError::Memory)?;
            }
        }
        r
    }

    async fn make_prp(
        &self,
        offset: u64,
        mut iovas: impl ExactSizeIterator<Item = u64>,
    ) -> Prp<'_> {
        let mut prp_pages = None;
        let dptr = match iovas.len() {
            0 => [INVALID_PAGE_ADDR; 2],
            1 => [iovas.next().unwrap() + offset, INVALID_PAGE_ADDR],
            2 => [iovas.next().unwrap() + offset, iovas.next().unwrap()],
            _ => {
                let a = iovas.next().unwrap();
                assert!(iovas.len() <= 4096);
                let prp = self
                    .alloc
                    .alloc_pages(1)
                    .await
                    .expect("pool cap is >= 1 page");

                let prp_addr = prp.physical_address(0);
                let page = prp.page_as_slice(0);
                for (iova, dest) in iovas.zip(page.chunks_exact(8)) {
                    dest.atomic_write_obj(&iova.to_le_bytes());
                }
                prp_pages = Some(prp);
                [a + offset, prp_addr]
            }
        };
        Prp {
            dptr,
            _pages: prp_pages,
        }
    }

    pub async fn issue_neither(
        &self,
        mut command: spec::Command,
    ) -> Result<spec::Completion, RequestError> {
        command.dptr = [INVALID_PAGE_ADDR; 2];
        self.issue_raw(command).await
    }

    pub async fn issue_in(
        &self,
        mut command: spec::Command,
        data: &[u8],
    ) -> Result<spec::Completion, RequestError> {
        let mem = self
            .alloc
            .alloc_bytes(data.len())
            .await
            .expect("pool cap is >= 1 page");

        mem.write(data);
        let prp = mem.prp();
        command.dptr = prp.dptr;
        self.issue_raw(command).await
    }

    pub async fn issue_out(
        &self,
        mut command: spec::Command,
        data: &mut [u8],
    ) -> Result<spec::Completion, RequestError> {
        let mem = self
            .alloc
            .alloc_bytes(data.len())
            .await
            .expect("pool cap is sufficient");

        let prp = mem.prp();
        command.dptr = prp.dptr;
        let completion = self.issue_raw(command).await;
        mem.read(data);
        completion
    }
}

impl ScopedPages<'_> {
    fn prp(&self) -> Prp<'_> {
        assert_eq!(
            self.page_count(),
            1,
            "larger requests not currently supported"
        );
        Prp {
            dptr: [self.physical_address(0), INVALID_PAGE_ADDR],
            _pages: None,
        }
    }
}

struct Prp<'a> {
    dptr: [u64; 2],
    _pages: Option<ScopedPages<'a>>,
}

#[derive(Inspect)]
struct PendingCommand {
    // Keep the command around for diagnostics.
    command: spec::Command,
    #[inspect(skip)]
    respond: mesh::OneshotSender<spec::Completion>,
}

enum Req {
    Command(Rpc<spec::Command, spec::Completion>),
    Inspect(inspect::Deferred),
    Save(Rpc<(), Result<QueuePairSavedState, anyhow::Error>>),
}

#[derive(Inspect)]
struct QueueHandler {
    sq: SubmissionQueue,
    cq: CompletionQueue,
    /// Mapping from cid to pending command.
    #[inspect(iter_by_key)]
    commands: Slab<PendingCommand>,
    max_cids: usize,
    stats: QueueStats,
}

#[derive(Inspect, Default)]
struct QueueStats {
    issued: Counter,
    completed: Counter,
    interrupts: Counter,
}

impl QueueHandler {
    async fn run(
        &mut self,
        registers: &DeviceRegisters<impl DeviceBacking>,
        mut recv: mesh::Receiver<Req>,
        interrupt: &mut DeviceInterrupt,
    ) {
        loop {
            enum Event {
                Request(Req),
                Completion(spec::Completion),
            }

            let event = poll_fn(|cx| {
                if !self.sq.is_full() && self.commands.len() < self.max_cids {
                    if let Poll::Ready(Some(req)) = recv.poll_next_unpin(cx) {
                        return Event::Request(req).into();
                    }
                }
                while !self.commands.is_empty() {
                    if let Some(completion) = self.cq.read() {
                        return Event::Completion(completion).into();
                    }
                    if interrupt.poll(cx).is_pending() {
                        break;
                    }
                    self.stats.interrupts.increment();
                }
                self.sq.commit(registers);
                self.cq.commit(registers);
                Poll::Pending
            })
            .await;

            match event {
                Event::Request(req) => match req {
                    Req::Command(Rpc(mut command, respond)) => {
                        let entry = self.commands.vacant_entry();
                        command.cdw0.set_cid(entry.key() as u16);
                        entry.insert(PendingCommand { command, respond });
                        self.sq.write(command).unwrap();
                        self.stats.issued.increment();
                    }
                    Req::Inspect(deferred) => deferred.inspect(&self),
                    Req::Save(queue_state) => {
                        queue_state.complete(self.save().await);
                    }
                },
                Event::Completion(completion) => {
                    let command = self.commands.remove(completion.cid.into());
                    assert_eq!(completion.sqid, self.sq.id());
                    self.sq.update_head(completion.sqhd);
                    command.respond.send(completion);
                    self.stats.completed.increment();
                }
            }
        }
    }

    /// Save queue data for servicing.
    pub async fn save(&self) -> anyhow::Result<QueuePairSavedState> {
        let mut pending_cmds: Vec<PendingCommandSavedState> = Vec::new();
        for cmd in &self.commands {
            let mut command: [u8; 64] = [0; 64];
            command.copy_from_slice(cmd.1.command.as_bytes());
            let command = PendingCommandSavedState {
                command,
                cid: cmd.0 as u16,
            };
            pending_cmds.push(
                //cmd.1.command.as_bytes().into()
                command,
            );
        }
        // The data is collected from both QueuePair and QueueHandler.
        Ok(QueuePairSavedState {
            max_cids: self.max_cids,
            sq_state: self.sq.save(),
            cq_state: self.cq.save(),
            pending_cmds,
            cpu: 0,         // Will be updated by the caller.
            msix: 0,        // Will be updated by the caller.
            sq_addr: 0,     // Will be updated by the caller.
            cq_addr: 0,     // Will be updated by the caller.
            base_mem: None, // Will be updated by the caller.
            mem_len: None,  // Will be updated by the caller.
            pfns: None,     // Will be updated by the caller.
        })
    }
}

pub(crate) fn admin_cmd(opcode: spec::AdminOpcode) -> spec::Command {
    spec::Command {
        cdw0: spec::Cdw0::new().with_opcode(opcode.0),
        ..FromZeroes::new_zeroed()
    }
}

#[repr(C)]
#[derive(Protobuf, Clone, Debug, AsBytes, FromBytes, FromZeroes)]
#[mesh(package = "openhcl.nvme")]
pub struct PendingCommandSavedState {
    #[mesh(1)]
    pub command: [u8; 64],
    #[mesh(2)]
    pub cid: u16,
}

impl From<&[u8]> for PendingCommandSavedState {
    fn from(value: &[u8]) -> Self {
        let mut command: [u8; 64] = [0; 64];
        command.copy_from_slice(value);
        let cid = ((command[0] as u16) << 8) | command[1] as u16;
        Self { command, cid }
    }
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "openhcl.nvme")]
pub struct QueuePairSavedState {
    #[mesh(1)]
    /// Which CPU handles requests.
    pub cpu: u32,
    #[mesh(2)]
    /// Interrupt vector (MSI-X)
    pub msix: u32,
    #[mesh(3)]
    pub max_cids: usize,
    #[mesh(4)]
    pub sq_state: SubmissionQueueSavedState,
    #[mesh(5)]
    pub cq_state: CompletionQueueSavedState,
    #[mesh(6)]
    pub sq_addr: u64,
    #[mesh(7)]
    pub cq_addr: u64,
    #[mesh(8)]
    pub base_mem: Option<u64>,
    #[mesh(9)]
    pub mem_len: Option<usize>,
    #[mesh(10)]
    pub pfns: Option<Vec<u64>>,
    #[mesh(11)]
    pub pending_cmds: Vec<PendingCommandSavedState>,
}
