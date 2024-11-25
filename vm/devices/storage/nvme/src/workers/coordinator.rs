// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coordinator between queues and hot add/remove of namespaces.

use super::admin::AdminConfig;
use super::admin::AdminHandler;
use super::admin::AdminState;
use super::admin::NsidConflict;
use super::IoQueueEntrySizes;
use crate::queue::DoorbellRegister;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::future::pending;
use std::sync::Arc;
use task_control::TaskControl;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use disk_backend::SimpleDisk;

pub struct NvmeWorkers {
    _task: Task<()>,
    send: mesh::MpscSender<CoordinatorRequest>,
    doorbells: Vec<Arc<DoorbellRegister>>,
    state: EnableState,
}

#[derive(Debug)]
enum EnableState {
    Disabled,
    Enabling(mesh::OneshotReceiver<()>),
    Enabled,
    Resetting(mesh::OneshotReceiver<()>),
}

impl InspectMut for NvmeWorkers {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.send.send(CoordinatorRequest::Inspect(req.defer()));
    }
}

impl NvmeWorkers {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        mem: GuestMemory,
        interrupts: Vec<Interrupt>,
        max_sqs: u16,
        max_cqs: u16,
        qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
        subsystem_id: Guid,
    ) -> Self {
        let num_qids = 2 + max_sqs.max(max_cqs) * 2;
        let doorbells: Vec<_> = (0..num_qids)
            .map(|_| Arc::new(DoorbellRegister::new()))
            .collect();

        let driver = driver_source.simple();
        let handler: AdminHandler = AdminHandler::new(
            driver.clone(),
            AdminConfig {
                driver_source: driver_source.clone(),
                mem,
                interrupts,
                doorbells: doorbells.clone(),
                subsystem_id,
                max_sqs,
                max_cqs,
                qe_sizes,
            },
        );
        let coordinator = Coordinator {
            driver: driver.clone(),
            admin: TaskControl::new(handler),
            reset: None,
        };
        let (send, recv) = mesh::mpsc_channel();
        let task = driver.spawn("nvme-coord", coordinator.run(recv));
        Self {
            _task: task,
            send,
            doorbells,
            state: EnableState::Disabled,
        }
    }

    pub fn client(&self) -> NvmeControllerClient {
        NvmeControllerClient {
            send: self.send.clone(),
        }
    }

    pub fn doorbell(&self, index: u16, value: u32) {
        if let Some(doorbell) = self.doorbells.get(index as usize) {
            doorbell.write(value);
        } else {
            tracelimit::warn_ratelimited!(index, value, "unknown doorbell");
        }
    }

    pub fn enable(&mut self, asq: u64, asqs: u16, acq: u64, acqs: u16) {
        if let EnableState::Disabled = self.state {
            self.state = EnableState::Enabling(self.send.call(
                CoordinatorRequest::EnableAdmin,
                EnableAdminParams {
                    asq,
                    asqs,
                    acq,
                    acqs,
                },
            ));
        } else {
            panic!("not disabled: {:?}", self.state);
        }
    }

    pub fn poll_enabled(&mut self) -> bool {
        if let EnableState::Enabling(recv) = &mut self.state {
            if recv.now_or_never().is_some() {
                self.state = EnableState::Enabled;
                true
            } else {
                false
            }
        } else {
            panic!("not enabling: {:?}", self.state)
        }
    }

    pub fn controller_reset(&mut self) {
        if let EnableState::Enabled = self.state {
            self.state =
                EnableState::Resetting(self.send.call(CoordinatorRequest::ControllerReset, ()));
        } else {
            panic!("not enabled: {:?}", self.state);
        }
    }

    pub fn poll_controller_reset(&mut self) -> bool {
        if let EnableState::Resetting(recv) = &mut self.state {
            if recv.now_or_never().is_some() {
                self.state = EnableState::Disabled;
                true
            } else {
                false
            }
        } else {
            panic!("not resetting: {:?}", self.state)
        }
    }

    // Reset the workers from whatever state they are in.
    pub async fn reset(&mut self) {
        loop {
            match &mut self.state {
                EnableState::Disabled => break,
                EnableState::Enabling(recv) => {
                    recv.await.unwrap();
                    self.state = EnableState::Enabled;
                }
                EnableState::Enabled => {
                    self.controller_reset();
                }
                EnableState::Resetting(recv) => {
                    recv.await.unwrap();
                    self.state = EnableState::Disabled;
                }
            }
        }
    }
}

/// Client for modifying the NVMe controller state at runtime.
#[derive(Debug)]
pub struct NvmeControllerClient {
    send: mesh::MpscSender<CoordinatorRequest>,
}

impl NvmeControllerClient {
    /// Adds a namespace.
    pub async fn add_namespace(&self, nsid: u32, disk: SimpleDisk) -> Result<(), NsidConflict> {
        self.send
            .call(CoordinatorRequest::AddNamespace, (nsid, disk))
            .await
            .unwrap()
    }

    /// Removes a namespace.
    pub async fn remove_namespace(&self, nsid: u32) -> bool {
        self.send
            .call(CoordinatorRequest::RemoveNamespace, nsid)
            .await
            .unwrap()
    }
}

#[derive(Inspect)]
struct Coordinator {
    driver: VmTaskDriver,
    #[inspect(flatten)]
    admin: TaskControl<AdminHandler, AdminState>,
    #[inspect(with = "Option::is_some")]
    reset: Option<Rpc<(), ()>>,
}

enum CoordinatorRequest {
    EnableAdmin(Rpc<EnableAdminParams, ()>),
    AddNamespace(Rpc<(u32, SimpleDisk), Result<(), NsidConflict>>),
    RemoveNamespace(Rpc<u32, bool>),
    Inspect(inspect::Deferred),
    ControllerReset(Rpc<(), ()>),
}

struct EnableAdminParams {
    asq: u64,
    asqs: u16,
    acq: u64,
    acqs: u16,
}

impl Coordinator {
    async fn run(mut self, mut recv: mesh::MpscReceiver<CoordinatorRequest>) {
        loop {
            enum Event {
                Request(Option<CoordinatorRequest>),
                ResetComplete,
            }

            let controller_reset = async {
                if self.reset.is_some() {
                    self.admin.stop().await;
                    if let Some(state) = self.admin.state_mut() {
                        state.drain().await;
                        self.admin.remove();
                    }
                } else {
                    pending().await
                }
            };

            let event = (
                recv.next().map(Event::Request),
                controller_reset.map(|_| Event::ResetComplete),
            )
                .race()
                .await;

            match event {
                Event::Request(Some(req)) => match req {
                    CoordinatorRequest::EnableAdmin(rpc) => rpc.handle_sync(
                        |EnableAdminParams {
                             asq,
                             asqs,
                             acq,
                             acqs,
                         }| {
                            if !self.admin.has_state() {
                                let state =
                                    AdminState::new(self.admin.task(), asq, asqs, acq, acqs);
                                self.admin.insert(&self.driver, "nvme-admin", state);
                                self.admin.start();
                            } else {
                                tracelimit::warn_ratelimited!("duplicate attempt to enable admin");
                            }
                        },
                    ),
                    CoordinatorRequest::AddNamespace(rpc) => {
                        rpc.handle(|(nsid, disk)| {
                            let this = &mut self;
                            async move {
                                let running = this.admin.stop().await;
                                let (admin, state) = this.admin.get_mut();
                                let r = admin.add_namespace(state, nsid, disk).await;
                                if running {
                                    this.admin.start();
                                }
                                r
                            }
                        })
                        .await
                    }
                    CoordinatorRequest::RemoveNamespace(rpc) => {
                        rpc.handle(|nsid| {
                            let this = &mut self;
                            async move {
                                let running = this.admin.stop().await;
                                let (admin, state) = this.admin.get_mut();
                                let r = admin.remove_namespace(state, nsid).await;
                                if running {
                                    this.admin.start();
                                }
                                r
                            }
                        })
                        .await
                    }
                    CoordinatorRequest::ControllerReset(rpc) => {
                        assert!(self.reset.is_none());
                        self.reset = Some(rpc);
                    }
                    CoordinatorRequest::Inspect(req) => req.inspect(&self),
                },
                Event::Request(None) => break,
                Event::ResetComplete => {
                    self.reset.take().unwrap().complete(());
                }
            }
        }
    }
}
