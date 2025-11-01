// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Admin queue handler.

use super::IoQueueEntrySizes;
use super::MAX_DATA_TRANSFER_SIZE;
use super::io::IoHandler;
use super::io::IoState;
use crate::DOORBELL_STRIDE_BITS;
use crate::MAX_QES;
use crate::NVME_VERSION;
use crate::PAGE_MASK;
use crate::PAGE_SIZE;
use crate::VENDOR_ID;
use crate::command_match::match_command_pattern;
use crate::error::CommandResult;
use crate::error::NvmeError;
use crate::namespace::Namespace;
use crate::prp::PrpRange;
use crate::queue::CompletionQueue;
use crate::queue::DoorbellMemory;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
use crate::spec;
use disk_backend::Disk;
use futures::FutureExt;
use futures::SinkExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use mesh::rpc::Rpc;
use nvme_resources::fault::AdminQueueFaultBehavior;
use nvme_resources::fault::CommandMatch;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::NamespaceChange;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::collections::btree_map;
use std::future::pending;
use std::future::poll_fn;
use std::io::Cursor;
use std::io::Write;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use thiserror::Error;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const IOSQES: u8 = 6;
const IOCQES: u8 = 4;
const MAX_ASYNC_EVENT_REQUESTS: u8 = 4; // minimum recommended by spec
const ERROR_LOG_PAGE_ENTRIES: u8 = 1;

#[derive(Inspect)]
pub struct AdminConfig {
    #[inspect(skip)]
    pub driver_source: VmTaskDriverSource,
    #[inspect(skip)]
    pub mem: GuestMemory,
    #[inspect(skip)]
    pub interrupts: Vec<Interrupt>,
    #[inspect(skip)]
    pub doorbells: Arc<RwLock<DoorbellMemory>>,
    #[inspect(display)]
    pub subsystem_id: Guid,
    pub max_sqs: u16,
    pub max_cqs: u16,
    pub qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(skip)]
    pub fault_configuration: FaultConfiguration,
}

#[derive(Inspect)]
pub struct AdminHandler {
    driver: VmTaskDriver,
    config: AdminConfig,
    #[inspect(iter_by_key)]
    namespaces: BTreeMap<u32, Arc<Namespace>>,
    #[inspect(skip)]
    timer: PolledTimer,
}

#[derive(Inspect)]
pub struct AdminState {
    admin_sq: SubmissionQueue,
    admin_cq: CompletionQueue,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_key(|x| x + 1)")]
    io_sqs: Vec<IoSq>,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_key(|x| x + 1)")]
    io_cqs: Vec<Option<IoCq>>,
    #[inspect(skip)]
    sq_delete_response: mesh::Receiver<u16>,
    #[inspect(iter_by_index)]
    asynchronous_event_requests: Vec<u16>,
    #[inspect(
        rename = "namespaces",
        with = "|x| inspect::iter_by_key(x.iter().map(|v| (v, ChangedNamespace { changed: true })))"
    )]
    changed_namespaces: Vec<u32>,
    notified_changed_namespaces: bool,
    #[inspect(skip)]
    recv_changed_namespace: futures::channel::mpsc::Receiver<u32>,
    #[inspect(skip)]
    send_changed_namespace: futures::channel::mpsc::Sender<u32>,
    #[inspect(skip)]
    poll_namespace_change: BTreeMap<u32, Task<()>>,
    #[inspect(skip)]
    confirm_namespace_change_fault: Option<Rpc<(), ()>>,
}

#[derive(Inspect)]
struct ChangedNamespace {
    changed: bool,
}

#[derive(Inspect)]
struct IoSq {
    #[inspect(flatten)]
    task: TaskControl<IoHandler, IoState>,
    driver: VmTaskDriver,
    pending_delete_cid: Option<u16>,
    cqid: Option<u16>,
}

#[derive(Inspect)]
struct IoCq {
    #[inspect(hex)]
    gpa: u64,
    #[inspect(hex)]
    len: u16,
    interrupt: Option<u16>,
    sqid: Option<u16>,
}

impl AdminState {
    pub fn new(handler: &AdminHandler, asq: u64, asqs: u16, acq: u64, acqs: u16) -> Self {
        // Start polling for namespace changes. Use a bounded channel to avoid
        // unbounded memory allocation when the queue is stuck.
        #[expect(clippy::disallowed_methods)] // TODO
        let (send_changed_namespace, recv_changed_namespace) = futures::channel::mpsc::channel(256);
        let poll_namespace_change = handler
            .namespaces
            .iter()
            .map(|(&nsid, namespace)| {
                (
                    nsid,
                    spawn_namespace_notifier(
                        &handler.driver,
                        nsid,
                        namespace.clone(),
                        send_changed_namespace.clone(),
                    ),
                )
            })
            .collect();

        let mut state = Self {
            admin_sq: SubmissionQueue::new(
                handler.config.doorbells.clone(),
                0,
                asq,
                asqs,
                handler.config.mem.clone(),
            ),
            admin_cq: CompletionQueue::new(
                handler.config.doorbells.clone(),
                1,
                handler.config.mem.clone(),
                Some(handler.config.interrupts[0].clone()),
                acq,
                acqs,
            ),
            io_sqs: Vec::new(),
            io_cqs: Vec::new(),
            sq_delete_response: Default::default(),
            asynchronous_event_requests: Vec::new(),
            changed_namespaces: Vec::new(),
            notified_changed_namespaces: false,
            recv_changed_namespace,
            send_changed_namespace,
            poll_namespace_change,
            confirm_namespace_change_fault: None,
        };
        state.set_max_queues(handler, handler.config.max_sqs, handler.config.max_cqs);
        state
    }

    /// Stops all submission queues and drains them of any pending IO.
    ///
    /// This future may be dropped and reissued.
    pub async fn drain(&mut self) {
        for sq in &mut self.io_sqs {
            sq.task.stop().await;
            if let Some(state) = sq.task.state_mut() {
                state.drain().await;
                sq.task.remove();
            }
        }
    }

    /// Caller must ensure that no queues are active.
    fn set_max_queues(&mut self, handler: &AdminHandler, num_sqs: u16, num_cqs: u16) {
        self.io_sqs.truncate(num_sqs.into());
        self.io_sqs
            .extend((self.io_sqs.len()..num_sqs.into()).map(|i| {
                // This driver doesn't explicitly do any IO (that's handled by
                // the storage backends), so the target VP doesn't matter. But
                // set it anyway as a hint to the backend that this queue needs
                // its own thread.
                let driver = handler
                    .config
                    .driver_source
                    .builder()
                    .run_on_target(false)
                    .target_vp(0)
                    .build("nvme");

                IoSq {
                    task: TaskControl::new(IoHandler::new(
                        handler.config.mem.clone(),
                        i as u16 + 1,
                        self.sq_delete_response.sender(),
                    )),
                    pending_delete_cid: None,
                    cqid: None,
                    driver,
                }
            }));
        self.io_cqs.resize_with(num_cqs.into(), || None);
    }

    fn add_changed_namespace(&mut self, nsid: u32) {
        if let Err(i) = self.changed_namespaces.binary_search(&nsid) {
            self.changed_namespaces.insert(i, nsid);
        }
    }

    async fn add_namespace(
        &mut self,
        driver: &VmTaskDriver,
        nsid: u32,
        namespace: &Arc<Namespace>,
    ) {
        // Update the IO queues.
        for sq in &mut self.io_sqs {
            let io_running = sq.task.stop().await;
            if let Some(io_state) = sq.task.state_mut() {
                io_state.add_namespace(nsid, namespace.clone());
            }
            if io_running {
                sq.task.start();
            }
        }

        // Start polling.
        let old = self.poll_namespace_change.insert(
            nsid,
            spawn_namespace_notifier(
                driver,
                nsid,
                namespace.clone(),
                self.send_changed_namespace.clone(),
            ),
        );
        assert!(old.is_none());

        // Notify the guest driver of the change.
        self.add_changed_namespace(nsid);
    }

    async fn remove_namespace(&mut self, nsid: u32) {
        // Update the IO queues.
        for sq in &mut self.io_sqs {
            let io_running = sq.task.stop().await;
            if let Some(io_state) = sq.task.state_mut() {
                io_state.remove_namespace(nsid);
            }
            if io_running {
                sq.task.start();
            }
        }

        // Stop polling.
        self.poll_namespace_change
            .remove(&nsid)
            .unwrap()
            .cancel()
            .await;

        // Notify the guest driver of the change.
        self.add_changed_namespace(nsid);

        self.poll_namespace_change
            .remove(&nsid)
            .unwrap()
            .cancel()
            .await;
    }
}

fn spawn_namespace_notifier(
    driver: &VmTaskDriver,
    nsid: u32,
    namespace: Arc<Namespace>,
    mut send_changed_namespace: futures::channel::mpsc::Sender<u32>,
) -> Task<()> {
    driver.spawn("wait_resize", async move {
        let mut counter = None;
        loop {
            counter = Some(namespace.wait_change(counter).await);
            tracing::info!(nsid, "namespace changed");
            if send_changed_namespace.send(nsid).await.is_err() {
                break;
            }
        }
    })
}

#[derive(Debug, Error)]
#[error("invalid queue identifier {qid}")]
struct InvalidQueueIdentifier {
    qid: u16,
    #[source]
    reason: InvalidQueueIdentifierReason,
}

#[derive(Debug, Error)]
enum InvalidQueueIdentifierReason {
    #[error("queue id is out of bounds")]
    Oob,
    #[error("queue id is in use")]
    InUse,
    #[error("queue id is not in use")]
    NotInUse,
}

impl From<InvalidQueueIdentifier> for NvmeError {
    fn from(err: InvalidQueueIdentifier) -> Self {
        Self::new(spec::Status::INVALID_QUEUE_IDENTIFIER, err)
    }
}

enum Event {
    Command(Result<spec::Command, QueueError>),
    SqDeleteComplete(u16),
    NamespaceChange(u32),
}

/// Error returned when adding a namespace with a conflicting ID.
#[derive(Debug, Error)]
#[error("namespace id conflict for {0}")]
pub struct NsidConflict(u32);

impl AdminHandler {
    pub fn new(driver: VmTaskDriver, config: AdminConfig) -> Self {
        Self {
            driver: driver.clone(),
            config,
            namespaces: Default::default(),
            timer: PolledTimer::new(&driver),
        }
    }

    pub async fn add_namespace(
        &mut self,
        state: Option<&mut AdminState>,
        nsid: u32,
        disk: Disk,
    ) -> Result<(), NsidConflict> {
        let namespace = &*match self.namespaces.entry(nsid) {
            btree_map::Entry::Vacant(entry) => entry.insert(Arc::new(Namespace::new(
                self.config.mem.clone(),
                nsid,
                disk,
            ))),
            btree_map::Entry::Occupied(_) => return Err(NsidConflict(nsid)),
        };

        if let Some(state) = state {
            state.add_namespace(&self.driver, nsid, namespace).await;
        }

        Ok(())
    }

    pub async fn remove_namespace(&mut self, state: Option<&mut AdminState>, nsid: u32) -> bool {
        if self.namespaces.remove(&nsid).is_none() {
            return false;
        }

        if let Some(state) = state {
            state.remove_namespace(nsid).await;
        }

        true
    }

    async fn next_event(&mut self, state: &mut AdminState) -> Result<Event, QueueError> {
        let event = loop {
            // Wait for there to be room for a completion for the next
            // command or the completed sq deletion.
            poll_fn(|cx| state.admin_cq.poll_ready(cx)).await?;

            if !state.changed_namespaces.is_empty() && !state.notified_changed_namespaces {
                if let Some(cid) = state.asynchronous_event_requests.pop() {
                    state.admin_cq.write(
                        spec::Completion {
                            dw0: spec::AsynchronousEventRequestDw0::new()
                                .with_event_type(spec::AsynchronousEventType::NOTICE.0)
                                .with_log_page_identifier(spec::LogPageIdentifier::CHANGED_NAMESPACE_LIST.0)
                                .with_information(spec::AsynchronousEventInformationNotice::NAMESPACE_ATTRIBUTE_CHANGED.0)
                                .into(),
                            dw1: 0,
                            sqhd: state.admin_sq.sqhd(),
                            sqid: 0,
                            cid,
                            status: spec::CompletionStatus::new(),
                        },
                    )?;

                    state.notified_changed_namespaces = true;

                    // Note: Since the tests require AEN before invoking restore on the VM, this approach will
                    // fail if the completion queue is full during save. In that case, the test will hang. Unless
                    // the test is specifically stress testing the completion queue, this approach should be fine
                    // since it is unexpected to fill the queue with admin commands.
                    if let Some(aen_confirmation) = state.confirm_namespace_change_fault.take() {
                        aen_confirmation.complete(());
                    }
                    continue;
                }
            }

            let next_command = poll_fn(|cx| state.admin_sq.poll_next(cx)).map(Event::Command);
            let sq_delete_complete = async {
                let Some(sqid) = state.sq_delete_response.next().await else {
                    pending().await
                };
                Event::SqDeleteComplete(sqid)
            };
            let changed_namespace = async {
                let Some(nsid) = state.recv_changed_namespace.next().await else {
                    pending().await
                };
                Event::NamespaceChange(nsid)
            };
            let changed_namespace_fault = async {
                // NOTE: Don't check 'fault_active' as it may get toggled after
                // this future is created. This channel is only used by the test
                // so it is fine to always listen on it.
                let Some(ns_change) = self
                    .config
                    .fault_configuration
                    .namespace_fault
                    .recv_changed_namespace
                    .next()
                    .await
                else {
                    pending().await
                };

                let nsid = match ns_change {
                    NamespaceChange::ChangeNotification(rpc) => {
                        let (nsid, notify) = rpc.split();
                        state.confirm_namespace_change_fault = Some(notify);
                        nsid
                    }
                };

                // Don't notify the test quite yet because nothing has been written to the completion queue.
                Event::NamespaceChange(nsid)
            };

            break (
                next_command,
                sq_delete_complete,
                changed_namespace,
                changed_namespace_fault,
            )
                .race()
                .await;
        };
        Ok(event)
    }

    async fn process_event(
        &mut self,
        state: &mut AdminState,
        event: Result<Event, QueueError>,
    ) -> Result<(), QueueError> {
        let (command_processed, cid, result) = match event? {
            Event::Command(command) => {
                let mut command = command?;
                let opcode = spec::AdminOpcode(command.cdw0.opcode());

                if self.config.fault_configuration.fault_active.get()
                    && let Some(fault) = Self::get_configured_fault_behavior_mut::<nvme_spec::Command>(
                        &mut self
                            .config
                            .fault_configuration
                            .admin_fault
                            .admin_submission_queue_faults,
                        &command,
                    )
                {
                    match fault {
                        AdminQueueFaultBehavior::Update(command_updated) => {
                            tracing::info!(
                                "configured fault: admin command updated in sq. original: {:?},\n new: {:?}",
                                &command,
                                command_updated
                            );
                            command = *command_updated;
                        }
                        AdminQueueFaultBehavior::Drop => {
                            tracing::info!(
                                "configured fault: admin command dropped from sq {:?}",
                                &command
                            );
                            return Ok(());
                        }
                        AdminQueueFaultBehavior::Delay(duration) => {
                            self.timer.sleep(*duration).await;
                        }
                        AdminQueueFaultBehavior::Panic(message) => {
                            panic!(
                                "configured fault: admin command panic with command: {:?} and message: {}",
                                &command, &message
                            );
                        }
                        AdminQueueFaultBehavior::CustomPayload(_) => {
                            panic!(
                                "bad fault configuration: custom payloads are not applicable to admin submission commands. command: {:?}",
                                &command
                            );
                        }
                        AdminQueueFaultBehavior::Verify(send) => {
                            // Verify that the command was received.
                            if let Some(send) = send.take() {
                                send.send(());
                            }
                        }
                    }
                }

                let result = match opcode {
                    spec::AdminOpcode::IDENTIFY => self
                        .handle_identify(state, &command)
                        .map(|()| Some(Default::default())),
                    spec::AdminOpcode::GET_FEATURES => {
                        self.handle_get_features(state, &command).await.map(Some)
                    }
                    spec::AdminOpcode::SET_FEATURES => {
                        self.handle_set_features(state, &command).map(Some)
                    }
                    spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE => self
                        .handle_create_io_completion_queue(state, &command)
                        .map(|()| Some(Default::default())),
                    spec::AdminOpcode::CREATE_IO_SUBMISSION_QUEUE => self
                        .handle_create_io_submission_queue(state, &command)
                        .map(|()| Some(Default::default())),
                    spec::AdminOpcode::DELETE_IO_COMPLETION_QUEUE => self
                        .handle_delete_io_completion_queue(state, &command)
                        .map(|()| Some(Default::default())),
                    spec::AdminOpcode::DELETE_IO_SUBMISSION_QUEUE => {
                        self.handle_delete_io_submission_queue(state, &command)
                    }
                    spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST => {
                        self.handle_asynchronous_event_request(state, &command)
                    }
                    spec::AdminOpcode::ABORT => self.handle_abort(),
                    spec::AdminOpcode::GET_LOG_PAGE => self
                        .handle_get_log_page(state, &command)
                        .map(|()| Some(Default::default())),
                    spec::AdminOpcode::DOORBELL_BUFFER_CONFIG
                        if self.supports_shadow_doorbells(state) =>
                    {
                        self.handle_doorbell_buffer_config(state, &command)
                            .await
                            .map(|()| Some(Default::default()))
                    }
                    opcode => {
                        tracelimit::warn_ratelimited!(?opcode, "unsupported opcode");
                        Err(spec::Status::INVALID_COMMAND_OPCODE.into())
                    }
                };

                let result = match result {
                    Ok(Some(cr)) => cr,
                    Ok(None) => return Ok(()),
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            cid = command.cdw0.cid(),
                            ?opcode,
                            "command error"
                        );
                        err.into()
                    }
                };

                (Some(command), command.cdw0.cid(), result)
            }
            Event::SqDeleteComplete(sqid) => {
                let sq = &mut state.io_sqs[sqid as usize - 1];
                let cid = sq.pending_delete_cid.take().unwrap();
                let cqid = sq.cqid.take().unwrap();
                sq.task.stop().await;
                sq.task.remove();
                assert_eq!(
                    state.io_cqs[cqid as usize - 1]
                        .as_mut()
                        .unwrap()
                        .sqid
                        .take(),
                    Some(sqid)
                );
                (None, cid, Default::default())
            }
            Event::NamespaceChange(nsid) => {
                state.add_changed_namespace(nsid);
                return Ok(());
            }
        };

        let status = spec::CompletionStatus::new().with_status(result.status.0);

        let mut completion = spec::Completion {
            dw0: result.dw[0],
            dw1: result.dw[1],
            sqid: 0,
            sqhd: state.admin_sq.sqhd(),
            status,
            cid,
        };

        // Apply a completion queue fault only to synchronously processed admin commands
        // (Ignore namespace change and sq delete complete events for now).
        if let Some(command) = command_processed
            && self.config.fault_configuration.fault_active.get()
            && let Some(fault) = Self::get_configured_fault_behavior_mut::<nvme_spec::Completion>(
                &mut self
                    .config
                    .fault_configuration
                    .admin_fault
                    .admin_completion_queue_faults,
                &command,
            )
        {
            match fault {
                AdminQueueFaultBehavior::Update(completion_updated) => {
                    tracing::info!(
                        "configured fault: admin completion updated in cq. command: {:?},original: {:?},\n new: {:?}",
                        &command,
                        &completion,
                        completion_updated
                    );
                    completion = completion_updated.clone();
                }
                AdminQueueFaultBehavior::Drop => {
                    tracing::info!(
                        "configured fault: admin completion dropped from cq. command: {:?}, completion: {:?}",
                        &command,
                        &completion
                    );
                    return Ok(());
                }
                AdminQueueFaultBehavior::Delay(duration) => {
                    self.timer.sleep(*duration).await;
                }
                AdminQueueFaultBehavior::Panic(message) => {
                    panic!(
                        "configured fault: admin completion panic with command: {:?}, completion: {:?} and message: {}",
                        &command, &completion, &message
                    );
                }
                AdminQueueFaultBehavior::CustomPayload(payload) => {
                    tracing::info!(
                        "configured fault: admin completion custom payload write. completion: {:?}, payload size: {}",
                        &completion,
                        payload.len()
                    );

                    // Panic to avoid silent test failures.
                    PrpRange::parse(&self.config.mem, payload.len(), command.dptr)
                        .expect("configured fault failure: failed to parse PRP for custom payload write.")
                        .write(&self.config.mem, payload)
                        .expect("configured fault failure: failed to write custom payload");
                }
                AdminQueueFaultBehavior::Verify(send) => {
                    // Verify that the command was received.
                    if let Some(send) = send.take() {
                        send.send(());
                    }
                }
            }
        }

        state.admin_cq.write(completion)?;
        Ok(())
    }

    fn handle_identify(
        &mut self,
        state: &AdminState,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10: spec::Cdw10Identify = command.cdw10.into();
        // All identify results are 4096 bytes.
        let mut buf = [0u64; 512];
        let buf = buf.as_mut_bytes();
        match spec::Cns(cdw10.cns()) {
            spec::Cns::CONTROLLER => {
                let id = spec::IdentifyController::mut_from_prefix(buf).unwrap().0; // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                *id = self.identify_controller(state);

                write!(
                    Cursor::new(&mut id.subnqn[..]),
                    "nqn.2014-08.org.nvmexpress:uuid:{}",
                    self.config.subsystem_id
                )
                .unwrap();
            }
            spec::Cns::ACTIVE_NAMESPACES => {
                if command.nsid >= 0xfffffffe {
                    return Err(spec::Status::INVALID_NAMESPACE_OR_FORMAT.into());
                }
                let nsids = <[u32]>::mut_from_bytes(buf).unwrap();
                for (ns, nsid) in self
                    .namespaces
                    .keys()
                    .filter(|&ns| *ns > command.nsid)
                    .zip(nsids)
                {
                    *nsid = *ns;
                }
            }
            spec::Cns::NAMESPACE => {
                if let Some(ns) = self.namespaces.get(&command.nsid) {
                    ns.identify(buf);
                } else {
                    tracelimit::warn_ratelimited!(nsid = command.nsid, "unknown namespace id");
                }
            }
            spec::Cns::DESCRIPTOR_NAMESPACE => {
                if let Some(ns) = self.namespaces.get(&command.nsid) {
                    ns.namespace_id_descriptor(buf);
                } else {
                    tracelimit::warn_ratelimited!(nsid = command.nsid, "unknown namespace id");
                }
            }
            cns => {
                tracelimit::warn_ratelimited!(?cns, "unsupported cns");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        };
        PrpRange::parse(&self.config.mem, buf.len(), command.dptr)?.write(&self.config.mem, buf)?;
        Ok(())
    }

    fn identify_controller(&self, state: &AdminState) -> spec::IdentifyController {
        spec::IdentifyController {
            vid: VENDOR_ID,
            ssvid: VENDOR_ID,
            mdts: (MAX_DATA_TRANSFER_SIZE / PAGE_SIZE).trailing_zeros() as u8,
            ver: NVME_VERSION,
            rtd3r: 400000,
            rtd3e: 400000,
            sqes: spec::QueueEntrySize::new()
                .with_min(IOSQES)
                .with_max(IOSQES),
            cqes: spec::QueueEntrySize::new()
                .with_min(IOCQES)
                .with_max(IOCQES),
            frmw: spec::FirmwareUpdates::new().with_ffsro(true).with_nofs(1),
            nn: self.namespaces.keys().copied().max().unwrap_or(0),
            ieee: [0x74, 0xe2, 0x8c], // Microsoft
            fr: (*b"v1.00000").into(),
            mn: (*b"MSFT NVMe Accelerator v1.0              ").into(),
            sn: (*b"SN: 000001          ").into(),
            aerl: MAX_ASYNC_EVENT_REQUESTS - 1,
            elpe: ERROR_LOG_PAGE_ENTRIES - 1,
            oaes: spec::Oaes::new().with_namespace_attribute(true),
            oncs: spec::Oncs::new()
                .with_dataset_management(true)
                // Namespaces still have to opt in individually via `rescap`.
                .with_reservations(true),
            vwc: spec::VolatileWriteCache::new()
                .with_present(true)
                .with_broadcast_flush_behavior(spec::BroadcastFlushBehavior::NOT_SUPPORTED.0),
            cntrltype: spec::ControllerType::IO_CONTROLLER,
            oacs: spec::OptionalAdminCommandSupport::new()
                .with_doorbell_buffer_config(self.supports_shadow_doorbells(state)),
            ..FromZeros::new_zeroed()
        }
    }

    fn handle_set_features(
        &mut self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<CommandResult, NvmeError> {
        let cdw10: spec::Cdw10SetFeatures = command.cdw10.into();
        let mut dw = [0; 2];
        // Note that we don't support non-zero cdw10.save, since ONCS.save == 0.
        match spec::Feature(cdw10.fid()) {
            spec::Feature::NUMBER_OF_QUEUES => {
                if state.io_sqs.iter().any(|sq| sq.task.has_state())
                    || state.io_cqs.iter().any(|cq| cq.is_some())
                {
                    return Err(spec::Status::COMMAND_SEQUENCE_ERROR.into());
                }
                let cdw11: spec::Cdw11FeatureNumberOfQueues = command.cdw11.into();
                if cdw11.ncq_z() == u16::MAX || cdw11.nsq_z() == u16::MAX {
                    return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
                }
                let num_sqs = (cdw11.nsq_z() + 1).min(self.config.max_sqs);
                let num_cqs = (cdw11.ncq_z() + 1).min(self.config.max_cqs);
                state.set_max_queues(self, num_sqs, num_cqs);

                dw[0] = spec::Cdw11FeatureNumberOfQueues::new()
                    .with_ncq_z(num_cqs - 1)
                    .with_nsq_z(num_sqs - 1)
                    .into();
            }
            spec::Feature::VOLATILE_WRITE_CACHE => {
                let cdw11 = spec::Cdw11FeatureVolatileWriteCache::from(command.cdw11);
                if !cdw11.wce() {
                    tracelimit::warn_ratelimited!(
                        "ignoring unsupported attempt to disable write cache"
                    );
                }
            }
            feature => {
                tracelimit::warn_ratelimited!(?feature, "unsupported feature");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        }
        Ok(CommandResult::new(spec::Status::SUCCESS, dw))
    }

    async fn handle_get_features(
        &mut self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<CommandResult, NvmeError> {
        let cdw10: spec::Cdw10GetFeatures = command.cdw10.into();
        let mut dw = [0; 2];

        // Note that we don't support non-zero cdw10.sel, since ONCS.save == 0.
        match spec::Feature(cdw10.fid()) {
            spec::Feature::NUMBER_OF_QUEUES => {
                let num_cqs = state.io_cqs.len();
                let num_sqs = state.io_sqs.len();
                dw[0] = spec::Cdw11FeatureNumberOfQueues::new()
                    .with_ncq_z((num_cqs - 1) as u16)
                    .with_nsq_z((num_sqs - 1) as u16)
                    .into();
            }
            spec::Feature::VOLATILE_WRITE_CACHE => {
                // Write cache is always enabled.
                dw[0] = spec::Cdw11FeatureVolatileWriteCache::new()
                    .with_wce(true)
                    .into();
            }
            spec::Feature::NVM_RESERVATION_PERSISTENCE => {
                let namespace = self
                    .namespaces
                    .get(&command.nsid)
                    .ok_or(spec::Status::INVALID_NAMESPACE_OR_FORMAT)?;

                return namespace.get_feature(command).await;
            }
            feature => {
                tracelimit::warn_ratelimited!(?feature, "unsupported feature");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        }
        Ok(CommandResult::new(spec::Status::SUCCESS, dw))
    }

    fn handle_create_io_completion_queue(
        &mut self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10: spec::Cdw10CreateIoQueue = command.cdw10.into();
        let cdw11: spec::Cdw11CreateIoCompletionQueue = command.cdw11.into();
        if !cdw11.pc() {
            return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
        }
        let cqid = cdw10.qid();
        let io_queue = state
            .io_cqs
            .get_mut((cqid as usize).wrapping_sub(1))
            .ok_or(InvalidQueueIdentifier {
                qid: cqid,
                reason: InvalidQueueIdentifierReason::Oob,
            })?;

        if io_queue.is_some() {
            return Err(InvalidQueueIdentifier {
                qid: cqid,
                reason: InvalidQueueIdentifierReason::InUse,
            }
            .into());
        }

        let interrupt = if cdw11.ien() {
            let iv = cdw11.iv();
            if iv as usize >= self.config.interrupts.len() {
                return Err(spec::Status::INVALID_INTERRUPT_VECTOR.into());
            };
            Some(iv)
        } else {
            None
        };
        let gpa = command.dptr[0] & PAGE_MASK;
        let len0 = cdw10.qsize_z();
        if len0 == 0 || len0 >= MAX_QES || self.config.qe_sizes.lock().cqe_bits != IOCQES {
            return Err(spec::Status::INVALID_QUEUE_SIZE.into());
        }

        *io_queue = Some(IoCq {
            gpa,
            len: len0 + 1,
            interrupt,
            sqid: None,
        });
        Ok(())
    }

    fn handle_create_io_submission_queue(
        &mut self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10: spec::Cdw10CreateIoQueue = command.cdw10.into();
        let cdw11: spec::Cdw11CreateIoSubmissionQueue = command.cdw11.into();
        if !cdw11.pc() {
            return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
        }
        let sqid = cdw10.qid();
        let sq = state
            .io_sqs
            .get_mut((sqid as usize).wrapping_sub(1))
            .ok_or(InvalidQueueIdentifier {
                qid: sqid,
                reason: InvalidQueueIdentifierReason::Oob,
            })?;

        if sq.task.has_state() {
            return Err(InvalidQueueIdentifier {
                qid: sqid,
                reason: InvalidQueueIdentifierReason::InUse,
            }
            .into());
        }

        let cqid = cdw11.cqid();
        let cq = state
            .io_cqs
            .get_mut((cqid as usize).wrapping_sub(1))
            .and_then(|x| x.as_mut())
            .ok_or(spec::Status::COMPLETION_QUEUE_INVALID)?;

        // Don't allow sharing completion queues. This isn't spec compliant
        // but it simplifies the device significantly and OSes don't seem to
        // mind. This could be fixed by having a slower path when completion
        // queues are shared.
        if cq.sqid.is_some() {
            return Err(spec::Status::COMPLETION_QUEUE_INVALID.into());
        }

        let sq_gpa = command.dptr[0] & PAGE_MASK;
        let len0 = cdw10.qsize_z();
        if len0 == 0 || len0 >= MAX_QES || self.config.qe_sizes.lock().sqe_bits != IOSQES {
            return Err(spec::Status::INVALID_QUEUE_SIZE.into());
        }

        cq.sqid = Some(sqid);
        sq.cqid = Some(cqid);
        let interrupt = cq
            .interrupt
            .map(|iv| self.config.interrupts[iv as usize].clone());
        let namespaces = self.namespaces.clone();
        let sq_len = len0 + 1;
        let cq_gpa = cq.gpa;
        let cq_len = cq.len;
        let state = IoState::new(
            &self.config.mem,
            self.config.doorbells.clone(),
            sq_gpa,
            sq_len,
            sqid,
            cq_gpa,
            cq_len,
            cqid,
            interrupt,
            namespaces,
            self.config.fault_configuration.io_fault.clone(),
        );
        sq.task.insert(&sq.driver, "nvme-io", state);
        sq.task.start();
        Ok(())
    }

    fn handle_delete_io_submission_queue(
        &self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<Option<CommandResult>, NvmeError> {
        let cdw10: spec::Cdw10DeleteIoQueue = command.cdw10.into();
        let sqid = cdw10.qid();
        let sq = state
            .io_sqs
            .get_mut((sqid as usize).wrapping_sub(1))
            .ok_or(InvalidQueueIdentifier {
                qid: sqid,
                reason: InvalidQueueIdentifierReason::Oob,
            })?;

        if !sq.task.has_state() || sq.pending_delete_cid.is_some() {
            return Err(InvalidQueueIdentifier {
                qid: sqid,
                reason: InvalidQueueIdentifierReason::NotInUse,
            }
            .into());
        }

        sq.task
            .update_with(|sq, sq_state| sq.delete(sq_state.unwrap()));
        sq.pending_delete_cid = Some(command.cdw0.cid());
        Ok(None)
    }

    fn handle_delete_io_completion_queue(
        &self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10: spec::Cdw10DeleteIoQueue = command.cdw10.into();
        let cqid = cdw10.qid();
        let cq = state
            .io_cqs
            .get_mut((cqid as usize).wrapping_sub(1))
            .ok_or(InvalidQueueIdentifier {
                qid: cqid,
                reason: InvalidQueueIdentifierReason::Oob,
            })?;

        let active_cq = cq.as_ref().ok_or(InvalidQueueIdentifier {
            qid: cqid,
            reason: InvalidQueueIdentifierReason::NotInUse,
        })?;
        if active_cq.sqid.is_some() {
            return Err(spec::Status::INVALID_QUEUE_DELETION.into());
        }

        *cq = None;
        Ok(())
    }

    fn handle_asynchronous_event_request(
        &self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<Option<CommandResult>, NvmeError> {
        if state.asynchronous_event_requests.len() >= MAX_ASYNC_EVENT_REQUESTS as usize {
            return Err(spec::Status::ASYNCHRONOUS_EVENT_REQUEST_LIMIT_EXCEEDED.into());
        }
        state.asynchronous_event_requests.push(command.cdw0.cid());
        Ok(None)
    }

    /// Abort is a required command, but a legal implementation is to just
    /// complete it with a status that means "I'm sorry, that command couldn't
    /// be aborted."
    fn handle_abort(&self) -> Result<Option<CommandResult>, NvmeError> {
        Ok(Some(CommandResult {
            status: spec::Status::SUCCESS,
            dw: [1, 0],
        }))
    }

    fn handle_get_log_page(
        &self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10 = spec::Cdw10GetLogPage::from(command.cdw10);
        let cdw11 = spec::Cdw11GetLogPage::from(command.cdw11);
        let numd =
            ((cdw10.numdl_z() as u32) | ((cdw11.numdu() as u32) << 16)).saturating_add(1) as usize;
        let len = numd * 4;
        let prp = PrpRange::parse(&self.config.mem, len, command.dptr)?;

        match spec::LogPageIdentifier(cdw10.lid()) {
            spec::LogPageIdentifier::ERROR_INFORMATION => {
                // Write empty log entries.
                prp.zero(
                    &self.config.mem,
                    len.min(ERROR_LOG_PAGE_ENTRIES as usize * 64),
                )?;
            }
            spec::LogPageIdentifier::HEALTH_INFORMATION => {
                if command.nsid != !0 {
                    return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
                }
                // Write an empty page.
                prp.zero(&self.config.mem, len.min(512))?;
            }
            spec::LogPageIdentifier::FIRMWARE_SLOT_INFORMATION => {
                // Write an empty page.
                prp.zero(&self.config.mem, len.min(512))?;
            }
            spec::LogPageIdentifier::CHANGED_NAMESPACE_LIST => {
                // Zero the whole list.
                prp.zero(&self.config.mem, len.min(4096))?;
                // Now write in the changed namespaces.
                if state.changed_namespaces.len() > 1024 {
                    // Too many to fit, write !0 so the driver scans everything.
                    prp.write(&self.config.mem, (!0u32).as_bytes())?;
                } else {
                    let count = state.changed_namespaces.len().min(numd);
                    prp.write(
                        &self.config.mem,
                        state.changed_namespaces[..count].as_bytes(),
                    )?;
                }
                state.changed_namespaces.clear();
                if !cdw10.rae() {
                    state.notified_changed_namespaces = false;
                }
            }
            lid => {
                tracelimit::warn_ratelimited!(?lid, "unsupported log page");
                return Err(spec::Status::INVALID_LOG_PAGE.into());
            }
        }

        Ok(())
    }

    fn supports_shadow_doorbells(&self, state: &AdminState) -> bool {
        let num_queues = state.io_sqs.len().max(state.io_cqs.len()) + 1;
        let len = num_queues * (2 << DOORBELL_STRIDE_BITS);
        // The spec only allows a single shadow doorbell page.
        len <= PAGE_SIZE
    }

    async fn handle_doorbell_buffer_config(
        &self,
        state: &mut AdminState,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        // Validated by caller.
        assert!(self.supports_shadow_doorbells(state));

        let shadow_db_gpa = command.dptr[0];
        let event_idx_gpa = command.dptr[1];
        if (shadow_db_gpa | event_idx_gpa) & !PAGE_MASK != 0 {
            return Err(NvmeError::from(spec::Status::INVALID_FIELD_IN_COMMAND));
        }

        self.config
            .doorbells
            .write()
            .replace_mem(self.config.mem.clone(), shadow_db_gpa, Some(event_idx_gpa))
            .map_err(|err| NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err))?;

        Ok(())
    }

    /// Returns a mutable reference to the fault behavior for a given command if a fault is configured.
    fn get_configured_fault_behavior_mut<'a, T: Clone>(
        fault_configs: &'a mut [(CommandMatch, AdminQueueFaultBehavior<T>)],
        command: &'a nvme_spec::Command,
    ) -> Option<&'a mut AdminQueueFaultBehavior<T>> {
        fault_configs
            .iter_mut()
            .find(|(pattern, _)| match_command_pattern(pattern, command))
            .map(|(_, behavior)| behavior)
    }
}

impl AsyncRun<AdminState> for AdminHandler {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminState,
    ) -> Result<(), Cancelled> {
        loop {
            let event = stop.until_stopped(self.next_event(state)).await?;
            if let Err(err) = self.process_event(state, event).await {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "admin queue failure"
                );
                break;
            }
        }
        Ok(())
    }
}

impl InspectTask<AdminState> for AdminHandler {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&AdminState>) {
        req.respond().merge(self).merge(state);
    }
}
