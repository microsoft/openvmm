// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! I/O queue handler.

use crate::error::CommandResult;
use crate::error::NvmeError;
use crate::namespace::Namespace;
use crate::queue::CompletionQueue;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::queue::ShadowDoorbell;
use crate::queue::SubmissionQueue;
use crate::spec;
use crate::spec::nvm;
use crate::workers::MAX_DATA_TRANSFER_SIZE;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use inspect::Inspect;
use std::collections::BTreeMap;
use std::future::pending;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use thiserror::Error;
use unicycle::FuturesUnordered;
use vmcore::interrupt::Interrupt;

#[derive(Inspect)]
pub struct IoHandler {
    mem: GuestMemory,
    sqid: u16,
    #[inspect(skip)]
    admin_response: mesh::MpscSender<u16>,
}

#[derive(Inspect)]
pub struct IoState {
    sq: SubmissionQueue,
    cq: CompletionQueue,
    #[inspect(skip)]
    namespaces: BTreeMap<u32, Arc<Namespace>>,
    #[inspect(skip)]
    ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoResult> + Send>>>,
    io_count: usize,
    queue_state: IoQueueState,
}

#[derive(Inspect)]
enum IoQueueState {
    Active,
    Deleting,
    Deleted,
}

impl IoState {
    pub fn new(
        sq_gpa: u64,
        sq_len: u16,
        sq_tail: Arc<DoorbellRegister>,
        sq_sdb_idx_gpas: Option<ShadowDoorbell>,
        cq_gpa: u64,
        cq_len: u16,
        cq_head: Arc<DoorbellRegister>,
        cq_sdb_idx_gpas: Option<ShadowDoorbell>,
        interrupt: Option<Interrupt>,
        namespaces: BTreeMap<u32, Arc<Namespace>>,
    ) -> Self {
        Self {
            sq: SubmissionQueue::new(sq_tail, sq_gpa, sq_len, sq_sdb_idx_gpas),
            cq: CompletionQueue::new(cq_head, interrupt, cq_gpa, cq_len, cq_sdb_idx_gpas),
            namespaces,
            ios: FuturesUnordered::new(),
            io_count: 0,
            queue_state: IoQueueState::Active,
        }
    }

    pub fn add_namespace(&mut self, nsid: u32, namespace: Arc<Namespace>) {
        assert!(self.namespaces.insert(nsid, namespace).is_none());
    }

    pub fn remove_namespace(&mut self, nsid: u32) {
        let _ = self.namespaces.remove(&nsid).unwrap();
    }

    /// Drains any pending IOs.
    ///
    /// This future may be dropped and reissued.
    pub async fn drain(&mut self) {
        while self.ios.next().await.is_some() {
            self.io_count -= 1;
        }
    }
}

struct IoResult {
    nsid: u32,
    cid: u16,
    opcode: nvm::NvmOpcode,
    result: Result<CommandResult, NvmeError>,
    advance_evt_idx: bool,
}

impl AsyncRun<IoState> for IoHandler {
    async fn run(&mut self, stop: &mut StopTask<'_>, state: &mut IoState) -> Result<(), Cancelled> {
        let mem = self.mem.clone();
        stop.until_stopped(async {
            if let Err(err) = self.process(state, &mem).await {
                tracing::error!(error = &err as &dyn std::error::Error, "io handler failed");
            }
        })
        .await
    }
}

impl InspectTask<IoState> for IoHandler {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&IoState>) {
        req.respond().merge(self).merge(state);
    }
}

const MAX_IO_QUEUE_DEPTH: usize = 8;

#[derive(Debug, Error)]
enum HandlerError {
    #[error("nvme queue error")]
    Queue(#[from] QueueError),
}

impl IoHandler {
    pub fn new(mem: GuestMemory, sqid: u16, admin_response: mesh::MpscSender<u16>) -> Self {
        Self {
            mem,
            sqid,
            admin_response,
        }
    }

    pub fn delete(&mut self, state: &mut IoState) {
        match state.queue_state {
            IoQueueState::Active => state.queue_state = IoQueueState::Deleting,
            IoQueueState::Deleting | IoQueueState::Deleted => {}
        }
    }

    async fn process(
        &mut self,
        state: &mut IoState,
        mem: &GuestMemory,
    ) -> Result<(), HandlerError> {
        loop {
            let deleting = match state.queue_state {
                IoQueueState::Active => {
                    // Wait for a completion to be ready. This will be necessary either
                    // to post an immediate result or to post an IO completion. It's not
                    // strictly necessary to start a new IO, but handling that special
                    // case is not worth the complexity.
                    state.cq.wait_ready(mem).await?;
                    false
                }
                IoQueueState::Deleting => {
                    if state.ios.is_empty() {
                        self.admin_response.send(self.sqid);
                        state.queue_state = IoQueueState::Deleted;
                        break;
                    }
                    true
                }
                IoQueueState::Deleted => break,
            };

            enum Event {
                Sq(Result<spec::Command, QueueError>),
                Io(IoResult),
            }

            let next_sqe = async {
                if state.io_count < MAX_IO_QUEUE_DEPTH && !deleting {
                    Event::Sq(state.sq.next(&self.mem).await)
                } else {
                    pending().await
                }
            };

            let next_io_completion = async {
                if state.ios.is_empty() {
                    pending().await
                } else {
                    Event::Io(state.ios.next().await.unwrap())
                }
            };

            let event = (next_sqe, next_io_completion).race().await;
            let (cid, result) = match event {
                Event::Io(io_result) => {
                    if io_result.advance_evt_idx {
                        let result = state.sq.advance_evt_idx(&self.mem);
                        if result.is_err() {
                            tracelimit::warn_ratelimited!("failure to advance evt_idx");
                        }
                    }
                    state.io_count -= 1;
                    let result = match io_result.result {
                        Ok(cr) => cr,
                        Err(err) => {
                            tracelimit::warn_ratelimited!(
                                error = &err as &dyn std::error::Error,
                                cid = io_result.cid,
                                nsid = io_result.nsid,
                                opcode = ?io_result.opcode,
                                "io error"
                            );
                            err.into()
                        }
                    };
                    (io_result.cid, result)
                }
                Event::Sq(r) => {
                    let command = r?;
                    let cid = command.cdw0.cid();

                    if let Some(ns) = state.namespaces.get(&command.nsid) {
                        let ns = ns.clone();
                        // If the queue depth is low, immediately update the evt_idx, so that
                        // the guest driver will ring the doorbell again.  If the queue depth is
                        // high, defer this until I/O completion, on the theory that high queue
                        // depth workloads won't wait before enqueuing more work.
                        //
                        // TODO: Update later after performance testing, perhaps to something
                        // like to 2*(number of VPs)/(number of queue pairs).
                        let mut advance_evt_idx = true;
                        if state.io_count <= 1 {
                            let result = state.sq.advance_evt_idx(&self.mem);
                            if result.is_err() {
                                tracelimit::warn_ratelimited!("failure to advance evt_idx");
                            }
                            advance_evt_idx = false;
                        }
                        let io = Box::pin(async move {
                            let result = ns.nvm_command(MAX_DATA_TRANSFER_SIZE, &command).await;
                            IoResult {
                                nsid: command.nsid,
                                opcode: nvm::NvmOpcode(command.cdw0.opcode()),
                                cid,
                                result,
                                advance_evt_idx,
                            }
                        });
                        state.ios.push(io);
                        state.io_count += 1;
                        continue;
                    }

                    let result = state.sq.advance_evt_idx(&self.mem);
                    if result.is_err() {
                        tracelimit::warn_ratelimited!("failure to advance evt_idx");
                    }
                    (cid, spec::Status::INVALID_NAMESPACE_OR_FORMAT.into())
                }
            };

            let completion = spec::Completion {
                dw0: result.dw[0],
                dw1: result.dw[1],
                sqhd: state.sq.sqhd(),
                sqid: self.sqid,
                cid,
                status: spec::CompletionStatus::new().with_status(result.status.0),
            };
            if !state.cq.write(&self.mem, completion)? {
                assert!(deleting);
                tracelimit::warn_ratelimited!("dropped i/o completion during queue deletion");
            }
            state
                .cq
                .catch_up_evt_idx(false, state.io_count as u32, &self.mem)?;
        }
        Ok(())
    }

    pub fn update_shadow_db(
        &mut self,
        mem: &GuestMemory,
        state: &mut IoState,
        sq_sdb: ShadowDoorbell,
        cq_sdb: ShadowDoorbell,
    ) {
        state.sq.update_shadow_db(mem, sq_sdb);
        state.cq.update_shadow_db(mem, cq_sdb);
    }
}
