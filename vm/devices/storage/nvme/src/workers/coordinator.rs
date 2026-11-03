// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coordinator between queues and hot add/remove of namespaces.

use super::IoQueueEntrySizes;
use super::admin::AddNamespaceError;
use super::admin::AdminConfig;
use super::admin::AdminHandler;
use super::admin::AdminState;
use crate::queue::DoorbellMemory;
use crate::queue::InvalidDoorbell;
use disk_backend::Disk;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::future::pending;
use std::sync::Arc;
use task_control::TaskControl;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(InspectMut)]
pub struct NvmeWorkers {
    #[inspect(skip)]
    task: Task<()>,
    #[inspect(flatten, send = "CoordinatorRequest::Inspect")]
    send: mesh::Sender<CoordinatorRequest>,
    #[inspect(skip)]
    doorbells: Arc<RwLock<DoorbellMemory>>,
    #[inspect(skip)]
    state: EnableState,
}

#[derive(Debug)]
enum EnableState {
    Disabled,
    Enabling(PendingRpc<()>),
    Enabled,
    Resetting(PendingRpc<()>),
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
        let doorbells = Arc::new(RwLock::new(DoorbellMemory::new(num_qids)));
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
            paused: false,
        };
        let (send, recv) = mesh::mpsc_channel();
        let task = driver.spawn("nvme-coord", coordinator.run(recv));
        Self {
            task,
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

    pub fn start(&mut self) {
        self.send.send(CoordinatorRequest::Start);
    }

    pub async fn stop(&mut self) {
        self.send
            .call(CoordinatorRequest::Stop, ())
            .await
            .expect("NVMe coordinator is gone");
    }

    pub async fn teardown(self) {
        self.send
            .call(CoordinatorRequest::Teardown, ())
            .await
            .expect("NVMe coordinator is gone");
        self.task.await;
    }

    pub fn doorbell(&self, db_id: u16, value: u32) {
        if let Err(InvalidDoorbell) = self.doorbells.read().try_write(db_id, value) {
            tracelimit::error_ratelimited!(db_id, "write to invalid doorbell index");
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
        let Self {
            task: _,
            send: _,
            doorbells,
            state,
        } = self;
        if let EnableState::Resetting(recv) = state {
            if recv.now_or_never().is_some() {
                *state = EnableState::Disabled;
                doorbells.write().reset();
                true
            } else {
                false
            }
        } else {
            panic!("not resetting: {:?}", state)
        }
    }

    // Reset the workers from whatever state they are in.
    pub async fn reset(&mut self) {
        self.send
            .call(CoordinatorRequest::Reset, ())
            .await
            .expect("NVMe coordinator is gone");
        self.state = EnableState::Disabled;
        self.doorbells.write().reset();
    }
}

/// Client for modifying the NVMe controller state at runtime.
#[derive(Debug)]
pub struct NvmeControllerClient {
    send: mesh::Sender<CoordinatorRequest>,
}

impl NvmeControllerClient {
    /// Adds a namespace.
    pub async fn add_namespace(&self, nsid: u32, disk: Disk) -> Result<(), AddNamespaceError> {
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
    paused: bool,
}

enum CoordinatorRequest {
    Start,
    Stop(Rpc<(), ()>),
    Teardown(Rpc<(), ()>),
    Reset(Rpc<(), ()>),
    EnableAdmin(Rpc<EnableAdminParams, ()>),
    AddNamespace(Rpc<(u32, Disk), Result<(), AddNamespaceError>>),
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
    async fn run(mut self, mut recv: mesh::Receiver<CoordinatorRequest>) {
        loop {
            enum Event {
                Request(Option<CoordinatorRequest>),
                ResetComplete,
            }

            let controller_reset = async {
                if self.reset.is_some() && !self.paused {
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
                    CoordinatorRequest::Start => {
                        self.paused = false;
                        if self.reset.is_none() {
                            if let Some(state) = self.admin.state_mut() {
                                state.start();
                            }
                            self.admin.start();
                        }
                    }
                    CoordinatorRequest::Stop(rpc) => {
                        self.paused = true;
                        self.admin.stop().await;
                        if let Some(state) = self.admin.state_mut() {
                            state.stop().await;
                        }
                        rpc.complete(());
                    }
                    CoordinatorRequest::Reset(rpc) => {
                        self.admin.stop().await;
                        if let Some(state) = self.admin.state_mut() {
                            state.drain().await;
                            self.admin.remove();
                        }
                        if let Some(reset) = self.reset.take() {
                            reset.complete(());
                        }
                        rpc.complete(());
                    }
                    CoordinatorRequest::Teardown(rpc) => {
                        assert!(
                            !self.admin.is_running(),
                            "NVMe must be stopped before teardown"
                        );
                        if let Some(state) = self.admin.state_mut() {
                            state.teardown().await;
                        }
                        drop(self);
                        rpc.complete(());
                        return;
                    }
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
                        rpc.handle(async |(nsid, disk)| {
                            let running = self.admin.stop().await;
                            let (admin, state) = self.admin.get_mut();
                            let r = admin.add_namespace(state, nsid, disk).await;
                            if running {
                                self.admin.start();
                            }
                            r
                        })
                        .await
                    }
                    CoordinatorRequest::RemoveNamespace(rpc) => {
                        rpc.handle(async |nsid| {
                            let running = self.admin.stop().await;
                            let (admin, state) = self.admin.get_mut();
                            let r = admin.remove_namespace(state, nsid).await;
                            if running {
                                self.admin.start();
                            }
                            r
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

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vmcore::vm_task::SingleDriverBackend;

    #[async_test]
    async fn teardown_joins_workers_with_a_retained_client(driver: DefaultDriver) {
        let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut workers = NvmeWorkers::new(
            &source,
            GuestMemory::allocate(0x4000),
            vec![Interrupt::from_fn(|| {})],
            1,
            1,
            Arc::new(Mutex::new(IoQueueEntrySizes::default())),
            Guid::new_random(),
        );
        let client = workers.client();
        workers.start();
        workers.enable(0, 8, 0x1000, 8);
        if let EnableState::Enabling(recv) = &mut workers.state {
            recv.await.unwrap();
        }
        workers.state = EnableState::Enabled;
        workers.stop().await;
        workers.start();
        workers.stop().await;
        workers.controller_reset();
        workers.stop().await;
        let EnableState::Resetting(recv) = &mut workers.state else {
            unreachable!()
        };
        assert!(recv.now_or_never().is_none());
        workers.reset().await;
        assert!(matches!(workers.state, EnableState::Disabled));
        workers.teardown().await;
        assert!(
            client
                .send
                .call(CoordinatorRequest::RemoveNamespace, 1)
                .await
                .is_err()
        );
    }
}
