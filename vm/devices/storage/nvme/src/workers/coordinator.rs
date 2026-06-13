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

#[derive(Inspect)]
pub struct NvmeWorkers {
    #[inspect(skip)]
    _task: Task<()>,
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
    Enabling(PendingRpc<bool>),
    Enabled,
    Resetting(PendingRpc<()>),
}

/// Result of polling an in-progress enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnablePoll {
    /// The enable is still in progress.
    Pending,
    /// The controller is enabled.
    Enabled,
    /// The enable was rejected (e.g. the secondary controller is offline).
    Rejected,
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
        sriov: Option<super::admin::SriovAdminConfig>,
        controller_id: u16,
        online: bool,
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
                sriov,
                controller_id,
            },
        );
        let coordinator = Coordinator {
            driver: driver.clone(),
            admin: TaskControl::new(handler),
            reset: None,
            online,
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

    pub fn poll_enabled(&mut self) -> EnablePoll {
        if let EnableState::Enabling(recv) = &mut self.state {
            match recv.now_or_never() {
                Some(result) => {
                    if result.unwrap() {
                        self.state = EnableState::Enabled;
                        EnablePoll::Enabled
                    } else {
                        self.state = EnableState::Disabled;
                        EnablePoll::Rejected
                    }
                }
                None => EnablePoll::Pending,
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
                    let accepted = recv.await.unwrap();
                    self.state = if accepted {
                        EnableState::Enabled
                    } else {
                        EnableState::Disabled
                    };
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

    /// Non-blocking poll for drain completion. Returns `true` when workers
    /// have reached the `Disabled` state. Drives the state machine forward
    /// from any state without blocking.
    ///
    /// Registers `cx.waker()` with the underlying channel so `poll_device`
    /// is woken when the drain makes progress.
    pub fn poll_drain(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        loop {
            match &mut self.state {
                EnableState::Disabled => return true,
                EnableState::Enabling(recv) => {
                    match std::pin::Pin::new(recv).poll(cx) {
                        std::task::Poll::Ready(accepted) => {
                            // On rejection the admin task never started, so
                            // the workers are effectively disabled; otherwise
                            // fall through to Enabled → controller_reset.
                            self.state = if accepted.unwrap() {
                                EnableState::Enabled
                            } else {
                                EnableState::Disabled
                            };
                        }
                        std::task::Poll::Pending => return false,
                    }
                }
                EnableState::Enabled => {
                    self.controller_reset();
                    // Fall through to Resetting.
                }
                EnableState::Resetting(recv) => match std::pin::Pin::new(recv).poll(cx) {
                    std::task::Poll::Ready(_) => {
                        self.state = EnableState::Disabled;
                        return true;
                    }
                    std::task::Poll::Pending => return false,
                },
            }
        }
    }
}

/// Client for modifying the NVMe controller state at runtime.
#[derive(Debug, Clone)]
pub struct NvmeControllerClient {
    send: mesh::Sender<CoordinatorRequest>,
}

/// Routing table mapping a 0-based VF index to that VF's controller client.
///
/// Populated only on VF create/destroy (`enable_vfs`/`disable_vfs`); all
/// runtime state (online/offline, namespace attach/detach) still flows as
/// ordered messages *through* the clients.
///
/// It is shared mutable state rather than a message because it needs
/// *synchronous publication*: `enable_vfs` fills it on the device thread inside
/// the guest's VF-enable config-space write, before that write completes. Since
/// the guest must observe that completion before issuing any virt-mgmt or
/// namespace-attach command, this establishes a happens-before edge with the
/// admin task's reads. A message would apply asynchronously with no such
/// ordering, letting a command race ahead and see an empty table.
pub type VfClientTable = Arc<Mutex<Vec<NvmeControllerClient>>>;

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

    /// Sets the online state of the controller.
    ///
    /// Awaiting this guarantees the coordinator has committed the online
    /// change before the caller proceeds, preserving happens-before ordering
    /// with a subsequent CC.EN on the controller.
    pub async fn set_online(&self, online: bool) {
        self.send
            .call(CoordinatorRequest::SetOnline, online)
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
    /// Whether this controller is online and may be enabled. Always true for
    /// the PF and standalone controllers; toggled for SR-IOV VFs via
    /// [`CoordinatorRequest::SetOnline`].
    online: bool,
}

enum CoordinatorRequest {
    EnableAdmin(Rpc<EnableAdminParams, bool>),
    AddNamespace(Rpc<(u32, Disk), Result<(), AddNamespaceError>>),
    RemoveNamespace(Rpc<u32, bool>),
    SetOnline(Rpc<bool, ()>),
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
                            if !self.online {
                                tracelimit::warn_ratelimited!(
                                    "enable attempted while controller is offline"
                                );
                                false
                            } else if !self.admin.has_state() {
                                let state =
                                    AdminState::new(self.admin.task(), asq, asqs, acq, acqs);
                                self.admin.insert(&self.driver, "nvme-admin", state);
                                self.admin.start();
                                true
                            } else {
                                tracelimit::warn_ratelimited!("duplicate attempt to enable admin");
                                true
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
                    CoordinatorRequest::SetOnline(rpc) => {
                        rpc.handle_sync(|online| {
                            self.online = online;
                        });
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
