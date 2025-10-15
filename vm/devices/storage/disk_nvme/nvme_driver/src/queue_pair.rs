// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of an admin or IO queue pair.

use super::spec;
use crate::driver::save_restore::AerHandlerSavedState;
use crate::driver::save_restore::Error;
use crate::driver::save_restore::PendingCommandSavedState;
use crate::driver::save_restore::PendingCommandsSavedState;
use crate::driver::save_restore::QueueHandlerSavedState;
use crate::driver::save_restore::QueuePairSavedState;
use crate::queues::CompletionQueue;
use crate::queues::SubmissionQueue;
use crate::registers::DeviceRegisters;
use anyhow::Context;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use inspect_counters::Counter;
use mesh::Cancel;
use mesh::CancelContext;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use nvme_spec::AsynchronousEventRequestDw0;
use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use safeatomic::AtomicSliceOps;
use slab::Slab;
use std::future::poll_fn;
use std::num::Wrapping;
use std::sync::Arc;
use std::task::Poll;
use thiserror::Error;
use user_driver::DeviceBacking;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;
use user_driver::memory::PAGE_SIZE64;
use user_driver::page_allocator::PageAllocator;
use user_driver::page_allocator::ScopedPages;
use zerocopy::FromZeros;

/// Value for unused PRP entries, to catch/mitigate buffer size mismatches.
const INVALID_PAGE_ADDR: u64 = !(PAGE_SIZE as u64 - 1);

const SQ_ENTRY_SIZE: usize = size_of::<spec::Command>();
const CQ_ENTRY_SIZE: usize = size_of::<spec::Completion>();
/// Submission Queue size in bytes.
const SQ_SIZE: usize = PAGE_SIZE * 4;
/// Completion Queue size in bytes.
const CQ_SIZE: usize = PAGE_SIZE;
/// Maximum SQ size in entries.
pub const MAX_SQ_ENTRIES: u16 = (SQ_SIZE / SQ_ENTRY_SIZE) as u16;
/// Maximum CQ size in entries.
pub const MAX_CQ_ENTRIES: u16 = (CQ_SIZE / CQ_ENTRY_SIZE) as u16;
/// Number of pages per queue if bounce buffering.
const PER_QUEUE_PAGES_BOUNCE_BUFFER: usize = 128;
/// Number of pages per queue if not bounce buffering.
const PER_QUEUE_PAGES_NO_BOUNCE_BUFFER: usize = 64;
/// Number of SQ entries per page (64).
const SQ_ENTRIES_PER_PAGE: usize = PAGE_SIZE / SQ_ENTRY_SIZE;

#[derive(Inspect)]
pub(crate) struct QueuePair<T: AerHandler> {
    #[inspect(skip)]
    task: Task<QueueHandler<T>>,
    #[inspect(skip)]
    cancel: Cancel,
    #[inspect(flatten, with = "|x| inspect::send(&x.send, Req::Inspect)")]
    issuer: Arc<Issuer>,
    #[inspect(skip)]
    mem: MemoryBlock,
    #[inspect(skip)]
    qid: u16,
    #[inspect(skip)]
    sq_entries: u16,
    #[inspect(skip)]
    cq_entries: u16,
    sq_addr: u64,
    cq_addr: u64,
}

impl PendingCommands {
    const CID_KEY_BITS: u32 = 10;
    const CID_KEY_MASK: u16 = (1 << Self::CID_KEY_BITS) - 1;
    const MAX_CIDS: usize = 1 << Self::CID_KEY_BITS;
    const CID_SEQ_OFFSET: Wrapping<u16> = Wrapping(1 << Self::CID_KEY_BITS);

    fn new(qid: u16) -> Self {
        Self {
            commands: Slab::new(),
            next_cid_high_bits: Wrapping(0),
            qid,
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
            .unwrap_or_else(|| panic!("completion for unknown cid: qid={}, cid={}", self.qid, cid));
        assert_eq!(
            command.command.cdw0.cid(),
            cid,
            "cid sequence number mismatch: qid={}, command_opcode={:#x}",
            self.qid,
            command.command.cdw0.opcode(),
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
    pub fn restore(saved_state: &PendingCommandsSavedState, qid: u16) -> anyhow::Result<Self> {
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
            qid,
        })
    }
}

impl<T: AerHandler> QueuePair<T> {
    /// Create a new queue pair.
    ///
    /// `sq_entries` and `cq_entries` are the requested sizes in entries.
    /// Calling code should request the largest size it thinks the device
    /// will support (see `CAP.MQES`). These may be clamped down to what will
    /// fit in one page should this routine fail to allocate physically
    /// contiguous memory to back the queues.
    /// IMPORTANT: Calling code should check the actual sizes via corresponding
    /// calls to [`QueuePair::sq_entries`] and [`QueuePair::cq_entries`] AFTER calling this routine.
    pub fn new(
        spawner: impl SpawnDriver,
        device: &impl DeviceBacking,
        qid: u16,
        sq_entries: u16,
        cq_entries: u16,
        interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
        bounce_buffer: bool,
        aer_handler: T,
    ) -> anyhow::Result<Self> {
        // FUTURE: Consider splitting this into several allocations, rather than
        // allocating the sum total together. This can increase the likelihood
        // of getting contiguous memory when falling back to the LockedMem
        // allocator, but this is not the expected path. Be careful that any
        // changes you make here work with already established save state.
        let total_size = SQ_SIZE
            + CQ_SIZE
            + if bounce_buffer {
                PER_QUEUE_PAGES_BOUNCE_BUFFER * PAGE_SIZE
            } else {
                PER_QUEUE_PAGES_NO_BOUNCE_BUFFER * PAGE_SIZE
            };
        let dma_client = device.dma_client();

        // TODO: Keepalive: Detect when the allocation came from outside
        // the private pool and put the device in a degraded state, so it
        // is possible to inspect that a servicing with keepalive will fail.
        let mem = dma_client
            .allocate_dma_buffer(total_size)
            .context("failed to allocate memory for queues")?;

        assert!(sq_entries <= MAX_SQ_ENTRIES);
        assert!(cq_entries <= MAX_CQ_ENTRIES);

        QueuePair::new_or_restore(
            spawner,
            qid,
            sq_entries,
            cq_entries,
            interrupt,
            registers,
            mem,
            None,
            bounce_buffer,
            aer_handler,
        )
    }

    fn new_or_restore(
        spawner: impl SpawnDriver,
        qid: u16,
        sq_entries: u16,
        cq_entries: u16,
        mut interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
        mem: MemoryBlock,
        saved_state: Option<&QueueHandlerSavedState>,
        bounce_buffer: bool,
        aer_handler: T,
    ) -> anyhow::Result<Self> {
        // MemoryBlock is either allocated or restored prior calling here.
        let sq_mem_block = mem.subblock(0, SQ_SIZE);
        let cq_mem_block = mem.subblock(SQ_SIZE, CQ_SIZE);
        let data_offset = SQ_SIZE + CQ_SIZE;

        // Make sure that the queue memory is physically contiguous. While the
        // NVMe spec allows for some provisions of queue memory to be
        // non-contiguous, this depends on device support. At least one device
        // that we must support requires that the memory is contiguous (via the
        // CAP.CQR bit). Because of that, just simplify the code paths to use
        // contiguous memory.
        //
        // We could also seek through the memory block to find contiguous pages
        // (for example, if the first 4 pages are not contiguous, but pages 5-8
        // are, use those), but other parts of this driver already assume the
        // math to get the correct offsets.
        //
        // N.B. It is expected that allocations from the private pool will
        // always be contiguous, and that is the normal path. That can fail in
        // some cases (e.g. if we got some guesses about memory size wrong), and
        // we prefer to operate in a perf degraded state rather than fail
        // completely.

        let (sq_is_contiguous, cq_is_contiguous) = (
            sq_mem_block.contiguous_pfns(),
            cq_mem_block.contiguous_pfns(),
        );

        let (sq_entries, cq_entries) = if !sq_is_contiguous || !cq_is_contiguous {
            tracing::warn!(
                qid,
                sq_is_contiguous,
                sq_mem_block.pfns = ?sq_mem_block.pfns(),
                cq_is_contiguous,
                cq_mem_block.pfns = ?cq_mem_block.pfns(),
                "non-contiguous queue memory detected, falling back to single page queues"
            );
            // Clamp both queues to the number of entries that will fit in a
            // single SQ page (since this will be the smaller between the SQ and
            // CQ capacity).
            (SQ_ENTRIES_PER_PAGE as u16, SQ_ENTRIES_PER_PAGE as u16)
        } else {
            (sq_entries, cq_entries)
        };

        let sq_addr = sq_mem_block.pfns()[0] * PAGE_SIZE64;
        let cq_addr = cq_mem_block.pfns()[0] * PAGE_SIZE64;

        let mut queue_handler = match saved_state {
            Some(s) => QueueHandler::restore(sq_mem_block, cq_mem_block, s, aer_handler)?,
            None => {
                // Create a new one.
                QueueHandler {
                    sq: SubmissionQueue::new(qid, sq_entries, sq_mem_block),
                    cq: CompletionQueue::new(qid, cq_entries, cq_mem_block),
                    commands: PendingCommands::new(qid),
                    stats: Default::default(),
                    drain_after_restore: false,
                    aer_handler,
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

        // Convert the queue pages to bytes, and assert that queue size is large
        // enough.
        const fn pages_to_size_bytes(pages: usize) -> usize {
            let size = pages * PAGE_SIZE;
            assert!(
                size >= 128 * 1024 + PAGE_SIZE,
                "not enough room for an ATAPI IO plus a PRP list"
            );
            size
        }

        // Page allocator uses remaining part of the buffer for dynamic
        // allocation. The length of the page allocator depends on if bounce
        // buffering / double buffering is needed.
        //
        // NOTE: Do not remove the `const` blocks below. This is to force
        // compile time evaluation of the assertion described above.
        let alloc_len = if bounce_buffer {
            const { pages_to_size_bytes(PER_QUEUE_PAGES_BOUNCE_BUFFER) }
        } else {
            const { pages_to_size_bytes(PER_QUEUE_PAGES_NO_BOUNCE_BUFFER) }
        };

        let alloc = PageAllocator::new(mem.subblock(data_offset, alloc_len));

        Ok(Self {
            task,
            cancel,
            issuer: Arc::new(Issuer { send, alloc }),
            mem,
            qid,
            sq_entries,
            cq_entries,
            sq_addr,
            cq_addr,
        })
    }

    /// Returns the actual number of SQ entries supported by this queue pair.
    pub fn sq_entries(&self) -> u16 {
        self.sq_entries
    }

    /// Returns the actual number of CQ entries supported by this queue pair.
    pub fn cq_entries(&self) -> u16 {
        self.cq_entries
    }

    pub fn sq_addr(&self) -> u64 {
        self.sq_addr
    }

    pub fn cq_addr(&self) -> u64 {
        self.cq_addr
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
        bounce_buffer: bool,
        aer_handler: T,
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
            bounce_buffer,
            aer_handler,
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

    pub async fn issue_get_aen(&self) -> Result<AsynchronousEventRequestDw0, RequestError> {
        match self.send.call(Req::NextAen, ()).await {
            Ok(aen_completion) => Ok(aen_completion),
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
        assert_eq!(
            mem.page_count(),
            1,
            "larger requests not currently supported"
        );
        let prp = Prp {
            dptr: [mem.physical_address(0), INVALID_PAGE_ADDR],
            _pages: None,
        };
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

        assert_eq!(
            mem.page_count(),
            1,
            "larger requests not currently supported"
        );
        let prp = Prp {
            dptr: [mem.physical_address(0), INVALID_PAGE_ADDR],
            _pages: None,
        };
        command.dptr = prp.dptr;
        let completion = self.issue_raw(command).await;
        mem.read(data);
        completion
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
    qid: u16,
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
    SendAer(),
    NextAen(Rpc<(), AsynchronousEventRequestDw0>),
}

/// Functionality for an AER handler. The default implementation
/// represents a NoOp handler with functions on the critical path compiled out
/// for efficiency and should be used for IO Queues.
pub trait AerHandler: Send + Sync + 'static {
    /// Given a completion command, if the command pertains to a pending AEN,
    /// process it.
    #[inline]
    fn handle_completion(&mut self, _completion: &nvme_spec::Completion) {}
    /// Handle a request from the driver to get the most-recent undelivered AEN
    /// or wait for the next one.
    fn handle_aen_request(&mut self, _rpc: Rpc<(), AsynchronousEventRequestDw0>) {}
    /// Update the CID that the handler is awaiting an AEN on.
    fn update_awaiting_cid(&mut self, _cid: u16) {}
    /// Returns whether an AER needs to sent to the controller or not. Since
    /// this is the only function on the critical path, attempt to inline it.
    #[inline]
    fn poll_send_aer(&self) -> bool {
        false
    }
    fn save(&self) -> Option<AerHandlerSavedState> {
        None
    }
    fn restore(&mut self, _state: &Option<AerHandlerSavedState>) {}
}

/// Admin queue AER handler. Ensures a single outstanding AER and persists state
/// across save/restore to process AENs received during servicing.
pub struct AdminAerHandler {
    last_aen: Option<AsynchronousEventRequestDw0>,
    await_aen_cid: Option<u16>,
    send_aen: Option<Rpc<(), AsynchronousEventRequestDw0>>, // Channel to return AENs on.
    failed: bool, // If the failed state is reached, it will stop looping until save/restore.
}

impl AdminAerHandler {
    pub fn new() -> Self {
        Self {
            last_aen: None,
            await_aen_cid: None,
            send_aen: None,
            failed: false,
        }
    }
}

impl AerHandler for AdminAerHandler {
    fn handle_completion(&mut self, completion: &nvme_spec::Completion) {
        if let Some(await_aen_cid) = self.await_aen_cid
            && completion.cid == await_aen_cid
            && !self.failed
        {
            self.await_aen_cid = None;

            // If error, cleanup and stop processing AENs.
            if completion.status.status() != 0 {
                self.failed = true;
                self.last_aen = None;
                return;
            }
            // Complete the AEN or pend it.
            let aen = AsynchronousEventRequestDw0::from_bits(completion.dw0);
            if let Some(send_aen) = self.send_aen.take() {
                send_aen.complete(aen);
            } else {
                self.last_aen = Some(aen);
            }
        }
    }

    fn handle_aen_request(&mut self, rpc: Rpc<(), AsynchronousEventRequestDw0>) {
        if let Some(aen) = self.last_aen.take() {
            rpc.complete(aen);
        } else {
            self.send_aen = Some(rpc); // Save driver request to be completed later.
        }
    }

    fn poll_send_aer(&self) -> bool {
        self.await_aen_cid.is_none() && !self.failed
    }

    fn update_awaiting_cid(&mut self, cid: u16) {
        if self.await_aen_cid.is_some() {
            panic!(
                "already awaiting on AEN with cid {}",
                self.await_aen_cid.unwrap()
            );
        }
        self.await_aen_cid = Some(cid);
    }

    fn save(&self) -> Option<AerHandlerSavedState> {
        Some(AerHandlerSavedState {
            last_aen: self.last_aen.map(AsynchronousEventRequestDw0::into_bits), // Save as u32
            await_aen_cid: self.await_aen_cid,
        })
    }

    fn restore(&mut self, state: &Option<AerHandlerSavedState>) {
        if let Some(state) = state {
            let AerHandlerSavedState {
                last_aen,
                await_aen_cid,
            } = state;
            self.last_aen = last_aen.map(AsynchronousEventRequestDw0::from_bits); // Restore from u32
            self.await_aen_cid = *await_aen_cid;
        }
    }
}

/// No-op AER handler. Should be only used for IO queues.
pub struct NoOpAerHandler;
impl AerHandler for NoOpAerHandler {
    fn handle_aen_request(&mut self, _rpc: Rpc<(), AsynchronousEventRequestDw0>) {
        panic!(
            "no-op aer handler should never receive an aen request. This is likely a bug in the driver."
        );
    }

    fn update_awaiting_cid(&mut self, _cid: u16) {
        panic!(
            "no-op aer handler should never be passed a cid to await. This is likely a bug in the driver."
        );
    }
}

#[derive(Inspect)]
struct QueueHandler<T: AerHandler> {
    sq: SubmissionQueue,
    cq: CompletionQueue,
    commands: PendingCommands,
    stats: QueueStats,
    drain_after_restore: bool,
    #[inspect(skip)]
    aer_handler: T,
}

#[derive(Inspect, Default)]
struct QueueStats {
    issued: Counter,
    completed: Counter,
    interrupts: Counter,
}

impl<T: AerHandler> QueueHandler<T> {
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
                        // Prioritize sending AERs to keep the cycle going
                        if self.aer_handler.poll_send_aer() {
                            return Poll::Ready(Event::Request(Req::SendAer()));
                        }
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
                    Req::NextAen(rpc) => {
                        self.aer_handler.handle_aen_request(rpc);
                    }
                    Req::SendAer() => {
                        let mut command = admin_cmd(spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST);
                        self.commands.insert(&mut command, Rpc::detached(()));
                        self.aer_handler.update_awaiting_cid(command.cdw0.cid());
                        self.sq.write(command).unwrap();
                        self.stats.issued.increment();
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
                    self.aer_handler.handle_completion(&completion);
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
            aer_handler: self.aer_handler.save(),
        })
    }

    /// Restore queue data after servicing.
    pub fn restore(
        sq_mem_block: MemoryBlock,
        cq_mem_block: MemoryBlock,
        saved_state: &QueueHandlerSavedState,
        mut aer_handler: T,
    ) -> anyhow::Result<Self> {
        let QueueHandlerSavedState {
            sq_state,
            cq_state,
            pending_cmds,
            aer_handler: aer_handler_saved_state,
        } = saved_state;

        aer_handler.restore(aer_handler_saved_state);

        Ok(Self {
            sq: SubmissionQueue::restore(sq_mem_block, sq_state)?,
            cq: CompletionQueue::restore(cq_mem_block, cq_state)?,
            commands: PendingCommands::restore(pending_cmds, sq_state.sqid)?,
            stats: Default::default(),
            // Only drain pending commands for I/O queues.
            // Admin queue is expected to have pending Async Event requests.
            drain_after_restore: sq_state.sqid != 0 && !pending_cmds.commands.is_empty(),
            aer_handler,
        })
    }
}

pub(crate) fn admin_cmd(opcode: spec::AdminOpcode) -> spec::Command {
    spec::Command {
        cdw0: spec::Cdw0::new().with_opcode(opcode.0),
        ..FromZeros::new_zeroed()
    }
}
