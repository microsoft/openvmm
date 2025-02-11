// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of an admin or IO queue pair.

use super::spec;
use crate::driver::save_restore::Error;
use crate::driver::save_restore::PendingCommandSavedState;
use crate::driver::save_restore::PendingCommandsSavedState;
use crate::driver::save_restore::QueueHandlerSavedState;
use crate::driver::save_restore::QueuePairSavedState;
use crate::page_allocator::PageAllocator;
use crate::page_allocator::ScopedPages;
use crate::queues::CompletionQueue;
use crate::queues::SubmissionQueue;
use crate::registers::DeviceRegisters;
use anyhow::Context;
use futures::StreamExt;
use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use inspect_counters::Counter;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use mesh::Cancel;
use mesh::CancelContext;
use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use safeatomic::AtomicSliceOps;
use slab::Slab;
use std::future::poll_fn;
use std::num::Wrapping;
use std::sync::Arc;
use std::task::Poll;
use thiserror::Error;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;
use user_driver::memory::PAGE_SIZE64;
use user_driver::DeviceBacking;
use zerocopy::FromZeros;

/// Value for unused PRP entries, to catch/mitigate buffer size mismatches.
const INVALID_PAGE_ADDR: u64 = !(PAGE_SIZE as u64 - 1);

pub(crate) struct QueuePair {
    task: Task<QueueHandler>,
    cancel: Cancel,
    issuer: Arc<Issuer>,
    mem: MemoryBlock,
    qid: u16,
    sq_entries: u16,
    cq_entries: u16,
}

impl Inspect for QueuePair {
    fn inspect(&self, req: inspect::Request<'_>) {
        let Self {
            task: _,
            cancel: _,
            issuer,
            mem: _,
            qid: _,
            sq_entries: _,
            cq_entries: _,
        } = self;
        issuer.send.send(Req::Inspect(req.defer()));
    }
}

impl PendingCommands {
    const CID_KEY_BITS: u32 = 10;
    const CID_KEY_MASK: u16 = (1 << Self::CID_KEY_BITS) - 1;
    const MAX_CIDS: usize = 1 << Self::CID_KEY_BITS;
    const CID_SEQ_OFFSET: Wrapping<u16> = Wrapping(1 << Self::CID_KEY_BITS);

    fn new() -> Self {
        Self {
            commands: Slab::new(),
            next_cid_high_bits: Wrapping(0),
        }
    }

    fn is_full(&self) -> bool {
        self.commands.len() >= Self::MAX_CIDS
    }

    fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Inserts a command into the pending list, updating it with a new CID.
    fn insert(&mut self, command: &mut spec::Command, respond: Rpc<(), spec::Completion>) {
        let entry = self.commands.vacant_entry();
        assert!(entry.key() < Self::MAX_CIDS);
        assert_eq!(self.next_cid_high_bits % Self::CID_SEQ_OFFSET, Wrapping(0));
        let cid = entry.key() as u16 | self.next_cid_high_bits.0;
        self.next_cid_high_bits += Self::CID_SEQ_OFFSET;
        command.cdw0.set_cid(cid);
        entry.insert(PendingCommand {
            command: *command,
            respond,
        });
    }

    fn remove(&mut self, cid: u16) -> Rpc<(), spec::Completion> {
        let command = self
            .commands
            .try_remove((cid & Self::CID_KEY_MASK) as usize)
            .expect("completion for unknown cid");
        assert_eq!(
            command.command.cdw0.cid(),
            cid,
            "cid sequence number mismatch"
        );
        command.respond
    }

    /// Save pending commands into a buffer.
    pub fn save(&self) -> PendingCommandsSavedState {
        let commands: Vec<PendingCommandSavedState> = self
            .commands
            .iter()
            .map(|(_index, cmd)| PendingCommandSavedState {
                command: cmd.command,
            })
            .collect();
        PendingCommandsSavedState {
            commands,
            next_cid_high_bits: self.next_cid_high_bits.0,
            // TODO: Not used today, added for future compatibility.
            cid_key_bits: Self::CID_KEY_BITS,
        }
    }

    /// Restore pending commands from the saved state.
    pub fn restore(saved_state: &PendingCommandsSavedState) -> anyhow::Result<Self> {
        let PendingCommandsSavedState {
            commands,
            next_cid_high_bits,
            cid_key_bits: _, // TODO: For future use.
        } = saved_state;

        Ok(Self {
            // Re-create identical Slab where CIDs are correctly mapped.
            commands: commands
                .iter()
                .map(|state| {
                    // To correctly restore Slab we need both the command index,
                    // inherited from command's CID, and the command itself.
                    (
                        // Remove high CID bits to be used as a key.
                        (state.command.cdw0.cid() & Self::CID_KEY_MASK) as usize,
                        PendingCommand {
                            command: state.command,
                            respond: Rpc::detached(()),
                        },
                    )
                })
                .collect::<Slab<PendingCommand>>(),
            next_cid_high_bits: Wrapping(*next_cid_high_bits),
        })
    }
}

impl QueuePair {
    pub const MAX_SQ_ENTRIES: u16 = (PAGE_SIZE / 64) as u16; // Maximum SQ size in entries.
    pub const MAX_CQ_ENTRIES: u16 = (PAGE_SIZE / 16) as u16; // Maximum CQ size in entries.
    const SQ_SIZE: usize = PAGE_SIZE; // Submission Queue size in bytes.
    const CQ_SIZE: usize = PAGE_SIZE; // Completion Queue size in bytes.
    const PER_QUEUE_PAGES: usize = 128;

    pub fn new(
        spawner: impl SpawnDriver,
        device: &impl DeviceBacking,
        qid: u16,
        sq_entries: u16, // Requested SQ size in entries.
        cq_entries: u16, // Requested CQ size in entries.
        interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
    ) -> anyhow::Result<Self> {
        let total_size =
            QueuePair::SQ_SIZE + QueuePair::CQ_SIZE + QueuePair::PER_QUEUE_PAGES * PAGE_SIZE;
        let dma_client = device.dma_client();
        let mem = dma_client
            .allocate_dma_buffer(total_size)
            .context("failed to allocate memory for queues")?;

        assert!(sq_entries <= Self::MAX_SQ_ENTRIES);
        assert!(cq_entries <= Self::MAX_CQ_ENTRIES);

        QueuePair::new_or_restore(
            spawner, qid, sq_entries, cq_entries, interrupt, registers, mem, None,
        )
    }

    /// Create new object or restore from saved state.
    fn new_or_restore(
        spawner: impl SpawnDriver,
        qid: u16,
        sq_entries: u16, // Submission queue entries.
        cq_entries: u16, // Completion queue entries.
        mut interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
        mem: MemoryBlock,
        saved_state: Option<&QueueHandlerSavedState>,
    ) -> anyhow::Result<Self> {
        // MemoryBlock is either allocated or restored prior calling here.
        let sq_mem_block = mem.subblock(0, QueuePair::SQ_SIZE);
        let cq_mem_block = mem.subblock(QueuePair::SQ_SIZE, QueuePair::CQ_SIZE);
        let data_offset = QueuePair::SQ_SIZE + QueuePair::CQ_SIZE;

        let mut queue_handler = match saved_state {
            Some(s) => QueueHandler::restore(sq_mem_block, cq_mem_block, s)?,
            None => {
                // Create a new one.
                QueueHandler {
                    sq: SubmissionQueue::new(qid, sq_entries, sq_mem_block),
                    cq: CompletionQueue::new(qid, cq_entries, cq_mem_block),
                    commands: PendingCommands::new(),
                    stats: Default::default(),
                    drain_after_restore: false,
                }
            }
        };

        let (send, recv) = mesh::channel();
        let (mut ctx, cancel) = CancelContext::new().with_cancel();
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

        // Page allocator uses remaining part of the buffer for dynamic allocation.
        #[allow(clippy::assertions_on_constants)]
        const _: () = assert!(
            QueuePair::PER_QUEUE_PAGES * PAGE_SIZE >= 128 * 1024 + PAGE_SIZE,
            "not enough room for an ATAPI IO plus a PRP list"
        );
        let alloc: PageAllocator =
            PageAllocator::new(mem.subblock(data_offset, QueuePair::PER_QUEUE_PAGES * PAGE_SIZE));

        Ok(Self {
            task,
            cancel,
            issuer: Arc::new(Issuer { send, alloc }),
            mem,
            qid,
            sq_entries,
            cq_entries,
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
        // Return error if the queue does not have any memory allocated.
        if self.mem.pfns().is_empty() {
            return Err(Error::InvalidState.into());
        }
        // Send an RPC request to QueueHandler thread to save its data.
        // QueueHandler stops any other processing after completing Save request.
        let handler_data = self.issuer.send.call(Req::Save, ()).await??;

        Ok(QueuePairSavedState {
            mem_len: self.mem.len(),
            base_pfn: self.mem.pfns()[0],
            qid: self.qid,
            sq_entries: self.sq_entries,
            cq_entries: self.cq_entries,
            handler_data,
        })
    }

    /// Restore queue pair state after servicing.
    pub fn restore(
        spawner: impl SpawnDriver,
        interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
        mem: MemoryBlock,
        saved_state: &QueuePairSavedState,
    ) -> anyhow::Result<Self> {
        let QueuePairSavedState {
            mem_len: _,  // Used to restore DMA buffer before calling this.
            base_pfn: _, // Used to restore DMA buffer before calling this.
            qid,
            sq_entries,
            cq_entries,
            handler_data,
        } = saved_state;

        QueuePair::new_or_restore(
            spawner,
            *qid,
            *sq_entries,
            *cq_entries,
            interrupt,
            registers,
            mem,
            Some(handler_data),
        )
    }
}

/// An error issuing an NVMe request.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum RequestError {
    #[error("queue pair is gone")]
    Gone(#[source] RpcError),
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
struct PendingCommands {
    /// Mapping from the low bits of cid to pending command.
    #[inspect(iter_by_key)]
    commands: Slab<PendingCommand>,
    #[inspect(hex)]
    next_cid_high_bits: Wrapping<u16>,
}

#[derive(Inspect)]
struct PendingCommand {
    // Keep the command around for diagnostics.
    command: spec::Command,
    #[inspect(skip)]
    respond: Rpc<(), spec::Completion>,
}

enum Req {
    Command(Rpc<spec::Command, spec::Completion>),
    Inspect(inspect::Deferred),
    Save(Rpc<(), Result<QueueHandlerSavedState, anyhow::Error>>),
}

#[derive(Inspect)]
struct QueueHandler {
    sq: SubmissionQueue,
    cq: CompletionQueue,
    commands: PendingCommands,
    stats: QueueStats,
    drain_after_restore: bool,
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

            let event = if !self.drain_after_restore {
                // Normal processing of the requests and completions.
                poll_fn(|cx| {
                    if !self.sq.is_full() && !self.commands.is_full() {
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
                .await
            } else {
                // Only process in-flight completions.
                poll_fn(|cx| {
                    while !self.commands.is_empty() {
                        if let Some(completion) = self.cq.read() {
                            return Event::Completion(completion).into();
                        }
                        if interrupt.poll(cx).is_pending() {
                            break;
                        }
                        self.stats.interrupts.increment();
                    }
                    self.cq.commit(registers);
                    Poll::Pending
                })
                .await
            };

            match event {
                Event::Request(req) => match req {
                    Req::Command(rpc) => {
                        let (mut command, respond) = rpc.split();
                        self.commands.insert(&mut command, respond);
                        self.sq.write(command).unwrap();
                        self.stats.issued.increment();
                    }
                    Req::Inspect(deferred) => deferred.inspect(&self),
                    Req::Save(queue_state) => {
                        queue_state.complete(self.save().await);
                        // Do not allow any more processing after save completed.
                        break;
                    }
                },
                Event::Completion(completion) => {
                    assert_eq!(completion.sqid, self.sq.id());
                    let respond = self.commands.remove(completion.cid);
                    if self.drain_after_restore && self.commands.is_empty() {
                        // Switch to normal processing mode once all in-flight commands completed.
                        self.drain_after_restore = false;
                    }
                    self.sq.update_head(completion.sqhd);
                    respond.complete(completion);
                    self.stats.completed.increment();
                }
            }
        }
    }

    /// Save queue data for servicing.
    pub async fn save(&self) -> anyhow::Result<QueueHandlerSavedState> {
        // The data is collected from both QueuePair and QueueHandler.
        Ok(QueueHandlerSavedState {
            sq_state: self.sq.save(),
            cq_state: self.cq.save(),
            pending_cmds: self.commands.save(),
        })
    }

    /// Restore queue data after servicing.
    pub fn restore(
        sq_mem_block: MemoryBlock,
        cq_mem_block: MemoryBlock,
        saved_state: &QueueHandlerSavedState,
    ) -> anyhow::Result<Self> {
        let QueueHandlerSavedState {
            sq_state,
            cq_state,
            pending_cmds,
        } = saved_state;

        Ok(Self {
            sq: SubmissionQueue::restore(sq_mem_block, sq_state)?,
            cq: CompletionQueue::restore(cq_mem_block, cq_state)?,
            commands: PendingCommands::restore(pending_cmds)?,
            stats: Default::default(),
            // Only drain pending commands for I/O queues.
            // Admin queue is expected to have pending Async Event requests.
            drain_after_restore: sq_state.sqid != 0 && !pending_cmds.commands.is_empty(),
        })
    }
}

pub(crate) fn admin_cmd(opcode: spec::AdminOpcode) -> spec::Command {
    spec::Command {
        cdw0: spec::Cdw0::new().with_opcode(opcode.0),
        ..FromZeros::new_zeroed()
    }
}
