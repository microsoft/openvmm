// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! I/O queue handler.
//!
//! Each handler task is responsible for a single completion queue and multiple
//! submission queues.
//!
//! This approach simplifies synchronization of the completion queue, but it
//! does limit parallelism across submission queues. This is probably fine,
//! though--most operating systems will only use multiple submission queues with
//! a single completion queue in order to effect multiple IO classes, not to
//! increase throughput.

use crate::error::CommandResult;
use crate::namespace::Namespace;
use crate::queue::CompletionQueue;
use crate::queue::DoorbellMemory;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
use crate::spec;
use crate::spec::nvm;
use crate::workers::MAX_DATA_TRANSFER_SIZE;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::Inspect;
use parking_lot::RwLock;
use slab::Slab;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
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
    #[inspect(skip)]
    admin_response: mesh::Sender<u16>,
}

#[derive(Inspect)]
pub struct IoState {
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|(_, sq)| (sq.sqid, sq)))")]
    sqs: Slab<SqState>,
    cq: CompletionQueue,
    #[inspect(skip)]
    namespaces: BTreeMap<u32, Arc<Namespace>>,
    #[inspect(skip)]
    ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoResult> + Send>>>,
    #[inspect(with = "VecDeque::len")]
    completions: VecDeque<IoResult>,
}

#[derive(Inspect)]
struct SqState {
    sqid: u16,
    sq: SubmissionQueue,
    io_count: usize,
    deleting: bool,
}

impl IoState {
    pub fn new(
        mem: &GuestMemory,
        doorbell: Arc<RwLock<DoorbellMemory>>,
        cq_gpa: u64,
        cq_len: u16,
        cqid: u16,
        interrupt: Option<Interrupt>,
        namespaces: BTreeMap<u32, Arc<Namespace>>,
    ) -> Self {
        Self {
            sqs: Slab::new(),
            cq: CompletionQueue::new(
                doorbell,
                cqid * 2 + 1,
                mem.clone(),
                interrupt,
                cq_gpa,
                cq_len,
            ),
            namespaces,
            ios: FuturesUnordered::new(),
            completions: VecDeque::new(),
        }
    }

    pub fn add_namespace(&mut self, nsid: u32, namespace: Arc<Namespace>) {
        assert!(self.namespaces.insert(nsid, namespace).is_none());
    }

    pub fn remove_namespace(&mut self, nsid: u32) {
        let _ = self.namespaces.remove(&nsid).unwrap();
    }

    pub fn has_sqs(&self) -> bool {
        !self.sqs.is_empty()
    }

    pub fn create_sq(&mut self, sqid: u16, sq_gpa: u64, sq_len: u16) -> usize {
        self.sqs.insert(SqState {
            sq: SubmissionQueue::new(&self.cq, sqid * 2, sq_gpa, sq_len),
            deleting: false,
            sqid,
            io_count: 0,
        })
    }

    pub fn delete_sq(&mut self, sq_idx: usize) {
        let sq = &mut self.sqs[sq_idx];
        sq.deleting = true;
        self.completions.retain(|io_result| {
            if io_result.sq_idx != sq_idx {
                return true;
            }
            sq.io_count -= 1;
            tracelimit::warn_ratelimited!("dropped i/o completion during queue deletion");
            false
        });
    }

    /// Drains any pending IOs.
    ///
    /// This future may be dropped and reissued.
    pub async fn drain(&mut self) {
        while let Some(io_result) = self.ios.next().await {
            self.sqs[io_result.sq_idx].io_count -= 1;
        }
    }
}

struct IoResult {
    sq_idx: usize,
    cid: u16,
    result: CommandResult,
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
    pub fn new(mem: GuestMemory, admin_response: mesh::Sender<u16>) -> Self {
        Self {
            mem,
            admin_response,
        }
    }

    async fn process(&mut self, state: &mut IoState) -> Result<(), HandlerError> {
        loop {
            enum Event {
                Sq(usize, Result<spec::Command, QueueError>),
                Io(IoResult),
                CompletionReady(Result<IoResult, QueueError>),
                Deleted(usize),
            }

            let event = poll_fn(|cx| {
                for (sq_idx, sq) in &mut state.sqs {
                    if !sq.deleting && sq.io_count < MAX_IO_QUEUE_DEPTH {
                        if let Poll::Ready(r) = sq.sq.poll_next(cx) {
                            return Poll::Ready(Event::Sq(sq_idx, r));
                        }
                    } else if sq.deleting && sq.io_count == 0 {
                        return Poll::Ready(Event::Deleted(sq_idx));
                    }
                }
                if let Poll::Ready(Some(r)) = state.ios.poll_next_unpin(cx) {
                    return Poll::Ready(Event::Io(r));
                }
                if !state.completions.is_empty() {
                    if let Poll::Ready(r) = state.cq.poll_ready(cx) {
                        return Poll::Ready(Event::CompletionReady(
                            r.map(|()| state.completions.pop_front().unwrap()),
                        ));
                    }
                }
                Poll::Pending
            })
            .await;

            let io_result = match event {
                Event::Io(io_result) => io_result,
                Event::CompletionReady(r) => r?,
                Event::Deleted(sq_idx) => {
                    let sq = state.sqs.remove(sq_idx);
                    self.admin_response.send(sq.sqid);
                    continue;
                }
                Event::Sq(sq_idx, r) => {
                    let command = r?;
                    let cid = command.cdw0.cid();

                    if let Some(ns) = state.namespaces.get(&command.nsid) {
                        let ns = ns.clone();
                        let io = Box::pin(async move {
                            let result = ns
                                .nvm_command(MAX_DATA_TRANSFER_SIZE, &command)
                                .await
                                .unwrap_or_else(|err| {
                                    tracelimit::warn_ratelimited!(
                                        error = &err as &dyn std::error::Error,
                                        cid,
                                        nsid = command.nsid,
                                        opcode = ?nvm::NvmOpcode(command.cdw0.opcode()),
                                        "io error"
                                    );
                                    err.into()
                                });
                            IoResult {
                                sq_idx,
                                cid,
                                result,
                            }
                        });
                        state.ios.push(io);
                        state.sqs[sq_idx].io_count += 1;
                        continue;
                    }

                    IoResult {
                        cid,
                        sq_idx,
                        result: spec::Status::INVALID_NAMESPACE_OR_FORMAT.into(),
                    }
                }
            };

            let sq = &mut state.sqs[io_result.sq_idx];
            let completion = spec::Completion {
                dw0: io_result.result.dw[0],
                dw1: io_result.result.dw[1],
                sqhd: sq.sq.sqhd(),
                sqid: sq.sqid,
                cid: io_result.cid,
                status: spec::CompletionStatus::new().with_status(io_result.result.status.0),
            };

            match state.cq.write(completion) {
                Ok(true) => {}
                Ok(false) => {
                    if !sq.deleting {
                        state.completions.push_back(io_result);
                        continue;
                    }
                    tracelimit::warn_ratelimited!("dropped i/o completion during queue deletion");
                }
                Err(err) => {
                    state.completions.push_back(io_result);
                    return Err(err.into());
                }
            }

            sq.io_count -= 1;
        }
    }
}
