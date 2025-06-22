// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! I/O queue handler.

use crate::error::CommandResult;
use crate::error::NvmeError;
use crate::namespace::Namespace;
use crate::queue::CompletionQueue;
use crate::queue::DoorbellMemory;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
use crate::spec;
use crate::spec::nvm;
use crate::workers::MAX_DATA_TRANSFER_SIZE;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use inspect::Inspect;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::future::Future;
use std::future::pending;
use std::future::poll_fn;
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
    admin_response: mesh::Sender<u16>,
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
        mem: &GuestMemory,
        doorbell: Arc<RwLock<DoorbellMemory>>,
        sq_gpa: u64,
        sq_len: u16,
        sq_id: u16,
        cq_gpa: u64,
        cq_len: u16,
        cq_id: u16,
        interrupt: Option<Interrupt>,
        namespaces: BTreeMap<u32, Arc<Namespace>>,
    ) -> Self {
        Self {
            sq: SubmissionQueue::new(doorbell.clone(), sq_id * 2, mem.clone(), sq_gpa, sq_len),
            cq: CompletionQueue::new(
                doorbell,
                cq_id * 2 + 1,
                mem.clone(),
                interrupt,
                cq_gpa,
                cq_len,
            ),
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
}

impl AsyncRun<IoState> for IoHandler {
    async fn run(&mut self, stop: &mut StopTask<'_>, state: &mut IoState) -> Result<(), Cancelled> {
        stop.until_stopped(async {
            if let Err(err) = self.process(state).await {
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
    pub fn new(mem: GuestMemory, sqid: u16, admin_response: mesh::Sender<u16>) -> Self {
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

    async fn process(&mut self, state: &mut IoState) -> Result<(), HandlerError> {
        loop {
            let deleting = match state.queue_state {
                IoQueueState::Active => {
                    // Wait for a completion to be ready. This will be necessary either
                    // to post an immediate result or to post an IO completion. It's not
                    // strictly necessary to start a new IO, but handling that special
                    // case is not worth the complexity.
                    poll_fn(|cx| state.cq.poll_ready(cx)).await?;
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
                    Event::Sq(poll_fn(|cx| state.sq.poll_next(cx)).await)
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
                        let io = Box::pin(async move {
                            let result = ns.nvm_command(MAX_DATA_TRANSFER_SIZE, &command).await;
                            IoResult {
                                nsid: command.nsid,
                                opcode: nvm::NvmOpcode(command.cdw0.opcode()),
                                cid,
                                result,
                            }
                        });
                        state.ios.push(io);
                        state.io_count += 1;
                        continue;
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
            if !state.cq.write(completion)? {
                assert!(deleting);
                tracelimit::warn_ratelimited!("dropped i/o completion during queue deletion");
            }
        }
        Ok(())
    }
}
