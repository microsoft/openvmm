// Copyright (C) Microsoft Corporation. All rights reserved.

//! Implementation of the device driver core.

use super::spec;
use crate::queue_pair::admin_cmd;
use crate::queue_pair::Issuer;
use crate::queue_pair::QueuePair;
use crate::registers::Bar0;
use crate::registers::DeviceRegisters;
use crate::Namespace;
use crate::NamespaceError;
use crate::RequestError;
use crate::NVME_PAGE_SHIFT;
use crate::queue_pair::QueuePairSavedState;
use anyhow::Context as _;
use futures::future::join_all;
use futures::StreamExt;
use inspect::Inspect;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::sync::Arc;
use std::sync::OnceLock;
use task_control::AsyncRun;
use task_control::InspectTask;
use task_control::TaskControl;
use tracing::info_span;
use tracing::Instrument;
use user_driver::backoff::Backoff;
use user_driver::DeviceBacking;
use user_driver::save_restore::VfioDeviceSavedState;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

/// An NVMe driver.
///
/// Note that if this is dropped, the process will abort. Call
/// [`NvmeDriver::shutdown`] to drop this.
#[derive(Inspect)]
pub struct NvmeDriver<T: DeviceBacking> {
    #[inspect(flatten)]
    task: Option<TaskControl<DriverWorkerTask<T>, WorkerState>>,
    device_id: String,
    /// Namespace ID associated with this driver.
    //nsid: u32,
    identify: Option<Arc<spec::IdentifyController>>,
    #[inspect(skip)]
    driver: VmTaskDriver,
    #[inspect(skip)]
    admin: Option<Arc<Issuer>>,
    #[inspect(skip)]
    io_issuers: Arc<IoIssuers>,
    #[inspect(skip)]
    rescan_event: Arc<event_listener::Event>,
    /// NVMe namespace associated with this driver (1:1).
    #[inspect(skip)]
    namespace: Option<Arc<Namespace>>,
    bar0_va: Option<u64>,
    /// Keeps the controller connected (CSTS.RDY==1) while servicing.
    nvme_keepalive: bool,
}

#[derive(Inspect)]
struct DriverWorkerTask<T: DeviceBacking> {
    device: T,
    #[inspect(skip)]
    driver: VmTaskDriver,
    registers: Arc<DeviceRegisters<T>>,
    admin: Option<QueuePair>,
    #[inspect(iter_by_index)]
    io: Vec<IoQueue>,
    io_issuers: Arc<IoIssuers>,
    #[inspect(skip)]
    recv: mesh::Receiver<NvmeWorkerRequest>,
}

#[derive(Inspect)]
struct WorkerState {
    max_io_queues: u16,
    qsize: u16,
    #[inspect(skip)]
    async_event_task: Task<()>,
}

#[derive(Inspect)]
struct IoQueue {
    queue: QueuePair,
    iv: u16,
    cpu: u32,
}

impl IoQueue {
    pub async fn save(&self) -> anyhow::Result<QueuePairSavedState> {
        let mut saved_state = self.queue.save().await?;
        saved_state.cpu = self.cpu;
        saved_state.msix = self.iv as u32;
        Ok(saved_state)
    }
}

#[derive(Debug, Inspect)]
#[inspect(transparent)]
pub(crate) struct IoIssuers {
    #[inspect(iter_by_index)]
    per_cpu: Vec<OnceLock<IoIssuer>>,
    #[inspect(skip)]
    send: mesh::Sender<NvmeWorkerRequest>,
}

#[derive(Debug, Clone, Inspect)]
struct IoIssuer {
    #[inspect(flatten)]
    issuer: Arc<Issuer>,
    cpu: u32,
}

#[derive(Debug)]
enum NvmeWorkerRequest {
    CreateIssuer(Rpc<u32, ()>),
    /// Save worker state.
    Save(Rpc<(), NvmeDriverSavedState>),
}

impl<T: DeviceBacking> NvmeDriver<T> {
    /// Initializes the driver.
    pub async fn new(
        driver_source: &VmTaskDriverSource,
        cpu_count: u32,
        device: T,
    ) -> anyhow::Result<Self> {
        let pci_id = device.id().to_owned();
        let mut this = Self::new_disabled(driver_source, cpu_count, device)
            .instrument(tracing::info_span!("nvme_new_disabled", pci_id))
            .await?;
        match this
            .enable(cpu_count as u16)
            .instrument(tracing::info_span!("nvme_enable", pci_id))
            .await
        {
            Ok(()) => Ok(this),
            Err(err) => {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "device initialization failed, shutting down"
                );
                this.shutdown().await;
                Err(err)
            }
        }
    }

    /// Initializes but does not enable the device. No memory is aliased by the
    /// device by this call.
    async fn new_disabled(
        driver_source: &VmTaskDriverSource,
        cpu_count: u32,
        mut device: T,
    ) -> anyhow::Result<Self> {
        let driver = driver_source.simple();
        let bar0 = Bar0(
            device
                .map_bar(0)
                .context("failed to map device registers")?,
        );
        let bar0_va = bar0.base_va();

        let cc = bar0.cc();
        if cc.en() || bar0.csts().rdy() {
            if !bar0
                .reset(&driver)
                .instrument(tracing::info_span!(
                    "nvme_already_enabled",
                    pci_id = device.id().to_owned()
                ))
                .await
            {
                anyhow::bail!("device is gone");
            }
        }

        let registers = Arc::new(DeviceRegisters::new(bar0));
        let cap = registers.cap;

        if cap.mpsmin() != 0 {
            anyhow::bail!(
                "unsupported minimum page size: {}",
                cap.mpsmin() + NVME_PAGE_SHIFT
            );
        }

        let (send, recv) = mesh::channel();
        let io_issuers = Arc::new(IoIssuers {
            per_cpu: (0..cpu_count).map(|_| OnceLock::new()).collect(),
            send,
        });

        Ok(Self {
            device_id: device.id().to_owned(),
            task: Some(TaskControl::new(DriverWorkerTask {
                device,
                driver: driver.clone(),
                registers,
                admin: None,
                io: Vec::new(),
                io_issuers: io_issuers.clone(),
                recv,
            })),
            admin: None,
            identify: None,
            driver,
            io_issuers,
            rescan_event: Default::default(),
            namespace: None,
            bar0_va: Some(bar0_va),
            nvme_keepalive: false,
        })
    }

    /// Enables the device, aliasing the admin queue memory and adding IO queues.
    async fn enable(&mut self, requested_io_queue_count: u16) -> anyhow::Result<()> {
        const ADMIN_QID: u16 = 0;

        let task = &mut self.task.as_mut().unwrap();
        let worker = task.task_mut();

        // Request the admin queue pair be the same size to avoid potential
        // device bugs where differing sizes might be a less common scenario
        //
        // Namely: using differing sizes revealed a bug in the initial NvmeDirectV2 implementation
        let admin_len = std::cmp::min(QueuePair::MAX_SQSIZE, QueuePair::MAX_CQSIZE);
        let admin_sqes = admin_len;
        let admin_cqes = admin_len;

        let interrupt0 = worker
            .device
            .map_interrupt(0, 0)
            .context("failed to map interrupt 0")?;

        // Start the admin queue pair.
        let admin = QueuePair::new(
            self.driver.clone(),
            &worker.device,
            ADMIN_QID,
            admin_sqes,
            admin_cqes,
            interrupt0,
            worker.registers.clone(),
        )
        .context("failed to create admin queue pair")?;

        let admin = worker.admin.insert(admin);

        // Register the admin queue with the controller.
        worker.registers.bar0.set_aqa(
            spec::Aqa::new()
                .with_acqs_z(admin_cqes - 1)
                .with_asqs_z(admin_sqes - 1),
        );
        worker.registers.bar0.set_asq(admin.sq_addr());
        worker.registers.bar0.set_acq(admin.cq_addr());

        // Enable the controller.
        let span = tracing::info_span!("nvme_ctrl_enable", pci_id = worker.device.id().to_owned());
        let ctrl_enable_span = span.enter();
        worker.registers.bar0.set_cc(
            spec::Cc::new()
                .with_iocqes(4)
                .with_iosqes(6)
                .with_en(true)
                .with_mps(0),
        );

        // Wait for the controller to be ready.
        let mut backoff = Backoff::new(&self.driver);
        loop {
            let csts = worker.registers.bar0.csts();
            if u32::from(csts) == !0 {
                anyhow::bail!("device is gone");
            }
            if csts.cfs() {
                worker.registers.bar0.reset(&self.driver).await;
                anyhow::bail!("device had fatal error");
            }
            if csts.rdy() {
                break;
            }
            backoff.back_off().await;
        }
        drop(ctrl_enable_span);

        // Get the controller identify structure.
        let identify = self
            .identify
            .insert(Arc::new(spec::IdentifyController::new_zeroed()));

        admin
            .issuer()
            .issue_out(
                spec::Command {
                    cdw10: spec::Cdw10Identify::new()
                        .with_cns(spec::Cns::CONTROLLER.0)
                        .into(),
                    ..admin_cmd(spec::AdminOpcode::IDENTIFY)
                },
                Arc::get_mut(identify).unwrap().as_bytes_mut(),
            )
            .await
            .context("failed to identify controller")?;

        // Configure the number of IO queues.
        //
        // Note that interrupt zero is shared between IO queue 1 and the admin queue.
        let max_interrupt_count = worker.device.max_interrupt_count();
        let requested_io_queue_count = if max_interrupt_count < requested_io_queue_count as u32 {
            tracing::warn!(
                max_interrupt_count,
                requested_io_queue_count,
                "queue count constrained by msi count"
            );
            max_interrupt_count as u16
        } else {
            requested_io_queue_count
        };

        let completion = admin
            .issuer()
            .issue_neither(spec::Command {
                cdw10: spec::Cdw10SetFeatures::new()
                    .with_fid(spec::Feature::NUMBER_OF_QUEUES.0)
                    .into(),
                cdw11: spec::Cdw11FeatureNumberOfQueues::new()
                    .with_ncq_z(requested_io_queue_count - 1)
                    .with_nsq_z(requested_io_queue_count - 1)
                    .into(),
                ..admin_cmd(spec::AdminOpcode::SET_FEATURES)
            })
            .await
            .context("failed to set number of queues")?;

        // See how many queues are actually available.
        let dw0 = spec::Cdw11FeatureNumberOfQueues::from(completion.dw0);
        let sq_count = dw0.nsq_z() + 1;
        let cq_count = dw0.ncq_z() + 1;
        let allocated_io_queue_count = sq_count.min(cq_count);
        if allocated_io_queue_count < requested_io_queue_count {
            tracing::warn!(
                sq_count,
                cq_count,
                requested_io_queue_count,
                "queue count constrained by hardware queue count"
            );
        }

        let max_io_queues = allocated_io_queue_count.min(requested_io_queue_count);

        let qsize = {
            let io_cqsize = QueuePair::MAX_CQSIZE.min(worker.registers.cap.mqes_z() + 1);
            let io_sqsize = QueuePair::MAX_SQSIZE.min(worker.registers.cap.mqes_z() + 1);

            // Some hardware (such as ASAP) require that the sq and cq have the same size.
            io_cqsize.min(io_sqsize)
        };

        // Spawn a task to handle asynchronous events.
        let async_event_task = self.driver.spawn("nvme_async_event", {
            let admin = admin.issuer().clone();
            let rescan_event = self.rescan_event.clone();
            async move {
                if let Err(err) = handle_asynchronous_events(&admin, &rescan_event).await {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "asynchronous event failure, not processing any more"
                    );
                }
            }
        });

        let mut state = WorkerState {
            qsize,
            async_event_task,
            max_io_queues,
        };

        self.admin = Some(admin.issuer().clone());

        // Pre-create the IO queue 1 for CPU 0. The other queues will be created
        // lazily.
        let issuer = worker
            .create_io_queue(&mut state, 0)
            .await
            .context("failed to create io queue 1")?;

        self.io_issuers.per_cpu[0].set(issuer).unwrap();
        task.insert(&self.driver, "nvme_worker", state);
        task.start();
        Ok(())
    }

    /// Shuts the device down.
    pub async fn shutdown(mut self) {
        // If nvme_keepalive was requested, return early.
        // The memory is still aliased as we don't flush pending IOs.
        if self.nvme_keepalive {
            return;
        }
        self.reset().await;
        drop(self);
    }

    fn reset(&mut self) -> impl 'static + Send + std::future::Future<Output = ()> {
        let driver = self.driver.clone();
        let mut task = std::mem::take(&mut self.task).unwrap();
        async move {
            task.stop().await;
            let (worker, state) = task.into_inner();
            if let Some(state) = state {
                state.async_event_task.cancel().await;
            }
            // Hold onto responses until the reset completes so that waiting IOs do
            // not think the memory is unaliased by the device.
            let _io_responses = join_all(worker.io.into_iter().map(|io| io.queue.shutdown())).await;
            let _admin_responses;
            if let Some(admin) = worker.admin {
                _admin_responses = admin.shutdown().await;
            }
            worker.registers.bar0.reset(&driver).await;
        }
    }

    /// Gets the namespace with namespace ID `nsid`.
    pub async fn namespace(&mut self, nsid: u32) -> Result<Arc<Namespace>, NamespaceError> {
        let ns = Arc::new(Namespace::new(
            &self.driver,
            self.admin.as_ref().unwrap().clone(),
            self.rescan_event.clone(),
            self.identify.clone().unwrap(),
            &self.io_issuers,
            &self.device_id,
            nsid,
        )
        .await?
        );
        self.namespace = Some(ns.clone());
        Ok(ns)
    }

    /// Returns the number of CPUs that are in fallback mode (that are using a
    /// remote CPU's queue due to a failure or resource limitation).
    pub fn fallback_cpu_count(&self) -> usize {
        self.io_issuers
            .per_cpu
            .iter()
            .enumerate()
            .filter(|&(cpu, c)| c.get().map_or(false, |c| c.cpu != cpu as u32))
            .count()
    }

    /// Saves the NVMe driver state during servicing.
    pub async fn save(&mut self) -> anyhow::Result<NvmeDriverSavedState> {
        self.nvme_keepalive = true;
        let mut save_state = self
            .io_issuers
            .send
            .call(NvmeWorkerRequest::Save, ())
            .await?;

        // Update other fields not accessible by worker task.
        self.identify.as_ref().unwrap().write_to(save_state.identify_ctrl.as_mut());
        save_state.device_id = self.device_id.clone();
        save_state.nsid = self.namespace.as_ref().map_or(0, |ns| ns.nsid());
        // Either 1 or 0 namespaces per driver.
        save_state.namespace = match &self.namespace {
            Some(ns) => {
                Some(ns.save()?)
            },
            None => None,
        };
        save_state.bar0_va = self.bar0_va;

        Ok(save_state)
    }

    /// Change device's behavior when servicing.
    pub fn update_servicing_flags(&mut self, nvme_keepalive: bool) {
        self.nvme_keepalive = nvme_keepalive;
    }
}

async fn handle_asynchronous_events(
    admin: &Issuer,
    rescan_event: &event_listener::Event,
) -> anyhow::Result<()> {
    loop {
        let completion = admin
            .issue_neither(admin_cmd(spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST))
            .await
            .context("asynchronous event request failed")?;

        let dw0 = spec::AsynchronousEventRequestDw0::from(completion.dw0);
        match spec::AsynchronousEventType(dw0.event_type()) {
            spec::AsynchronousEventType::NOTICE => {
                tracing::info!("namespace attribute change event");

                // Clear the namespace list.
                let mut list = [0u32; 1024];
                admin
                    .issue_out(
                        spec::Command {
                            cdw10: spec::Cdw10GetLogPage::new()
                                .with_lid(spec::LogPageIdentifier::CHANGED_NAMESPACE_LIST.0)
                                .with_numdl_z(1023)
                                .into(),
                            ..admin_cmd(spec::AdminOpcode::GET_LOG_PAGE)
                        },
                        list.as_bytes_mut(),
                    )
                    .await
                    .context("failed to query changed namespace list")?;

                if list[0] != 0 {
                    // For simplicity, tell all namespaces to rescan.
                    rescan_event.notify(usize::MAX);
                }
            }
            event_type => {
                tracing::info!(
                    ?event_type,
                    information = dw0.information(),
                    log_page_identifier = dw0.log_page_identifier(),
                    "unhandled asynchronous event"
                );
            }
        }
    }
}

impl<T: DeviceBacking> Drop for NvmeDriver<T> {
    fn drop(&mut self) {
        if self.task.is_some() {
            // Do not reset NVMe device when nvme_keepalive is requested.
            if !self.nvme_keepalive {
                // Reset the device asynchronously so that pending IOs are not
                // dropped while their memory is aliased.
                let reset = self.reset();
                self.driver.spawn("nvme_drop", reset).detach();
            }
        }
    }
}

impl IoIssuers {
    pub async fn get(&self, cpu: u32) -> Result<&Issuer, RequestError> {
        if let Some(v) = self.per_cpu[cpu as usize].get() {
            return Ok(&v.issuer);
        }

        self.send
            .call(NvmeWorkerRequest::CreateIssuer, cpu)
            .await
            .map_err(RequestError::Gone)?;

        Ok(self.per_cpu[cpu as usize]
            .get()
            .expect("issuer was set by rpc")
            .issuer
            .as_ref())
    }
}

impl<T: DeviceBacking> AsyncRun<WorkerState> for DriverWorkerTask<T> {
    async fn run(
        &mut self,
        stop: &mut task_control::StopTask<'_>,
        state: &mut WorkerState,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            loop {
                match self.recv.next().await {
                    Some(NvmeWorkerRequest::CreateIssuer(rpc)) => {
                        rpc.handle(|cpu| self.create_io_issuer(state, cpu)).await
                    },
                    Some(NvmeWorkerRequest::Save(rpc)) => {
                        rpc.handle(|_| {
                                let save_state = self.save_wrapper(state);
                                save_state
                            }
                        ).await
                    },
                    None => {
                        break
                    }
                }
            }
        })
        .await
    }
}

impl<T: DeviceBacking> DriverWorkerTask<T> {
    async fn create_io_issuer(&mut self, state: &mut WorkerState, cpu: u32) {
        tracing::debug!(cpu, "issuer request");
        if self.io_issuers.per_cpu[cpu as usize].get().is_some() {
            return;
        }

        let issuer = match self
            .create_io_queue(state, cpu)
            .instrument(info_span!("create_nvme_io_queue", cpu))
            .await
        {
            Ok(issuer) => issuer,
            Err(err) => {
                // Find a fallback queue close in index to the failed queue.
                let (fallback_cpu, fallback) = self.io_issuers.per_cpu[..cpu as usize]
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(i, issuer)| issuer.get().map(|issuer| (i, issuer)))
                    .unwrap();

                tracing::error!(
                    cpu,
                    fallback_cpu,
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to create io queue, falling back"
                );
                fallback.clone()
            }
        };

        self.io_issuers.per_cpu[cpu as usize]
            .set(issuer)
            .ok()
            .unwrap();
    }

    async fn create_io_queue(
        &mut self,
        state: &mut WorkerState,
        cpu: u32,
    ) -> anyhow::Result<IoIssuer> {
        if self.io.len() >= state.max_io_queues as usize {
            anyhow::bail!("no more io queues available");
        }

        let qid = self.io.len() as u16 + 1;

        tracing::debug!(cpu, qid, "creating io queue");

        // Share IO queue 1's interrupt with the admin queue.
        let iv = self.io.len() as u16;
        let interrupt = self
            .device
            .map_interrupt(iv.into(), cpu)
            .context("failed to map interrupt")?;

        let queue = QueuePair::new(
            self.driver.clone(),
            &self.device,
            qid,
            state.qsize,
            state.qsize,
            interrupt,
            self.registers.clone(),
        )
        .with_context(|| format!("failed to create io queue pair {qid}"))?;

        let io_sq_addr = queue.sq_addr();
        let io_cq_addr = queue.cq_addr();

        // Add the queue pair before aliasing its memory with the device so
        // that it can be torn down correctly on failure.
        self.io.push(IoQueue { queue, iv, cpu });
        let io_queue = self.io.last_mut().unwrap();

        let admin = self.admin.as_ref().unwrap().issuer().as_ref();

        let mut created_completion_queue = false;
        let r = async {
            admin
                .issue_raw(spec::Command {
                    cdw10: spec::Cdw10CreateIoQueue::new()
                        .with_qid(qid)
                        .with_qsize_z(state.qsize - 1)
                        .into(),
                    cdw11: spec::Cdw11CreateIoCompletionQueue::new()
                        .with_ien(true)
                        .with_iv(iv)
                        .with_pc(true)
                        .into(),
                    dptr: [io_cq_addr, 0],
                    ..admin_cmd(spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE)
                })
                .await
                .with_context(|| format!("failed to create io completion queue {qid}"))?;

            created_completion_queue = true;

            admin
                .issue_raw(spec::Command {
                    cdw10: spec::Cdw10CreateIoQueue::new()
                        .with_qid(qid)
                        .with_qsize_z(state.qsize - 1)
                        .into(),
                    cdw11: spec::Cdw11CreateIoSubmissionQueue::new()
                        .with_cqid(qid)
                        .with_pc(true)
                        .into(),
                    dptr: [io_sq_addr, 0],
                    ..admin_cmd(spec::AdminOpcode::CREATE_IO_SUBMISSION_QUEUE)
                })
                .await
                .with_context(|| format!("failed to create io submission queue {qid}"))?;

            Ok(())
        };

        if let Err(err) = r.await {
            if created_completion_queue {
                if let Err(err) = admin
                    .issue_raw(spec::Command {
                        cdw10: spec::Cdw10DeleteIoQueue::new().with_qid(qid).into(),
                        ..admin_cmd(spec::AdminOpcode::DELETE_IO_COMPLETION_QUEUE)
                    })
                    .await
                {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to delete completion queue in teardown path"
                    );
                }
            }
            let io = self.io.pop().unwrap();
            io.queue.shutdown().await;
            return Err(err);
        }

        Ok(IoIssuer {
            issuer: io_queue.queue.issuer().clone(),
            cpu,
        })
    }

    /// Wrapper around save() to consume its response (for AsyncRun thread).
    async fn save_wrapper(&mut self, worker_state: &mut WorkerState) -> NvmeDriverSavedState {
        match self
            .save(worker_state)
            .await
        {
            Ok(state) => {
                state
            },
            Err(_) => {
                NvmeDriverSavedState {
                    admin: None,
                    io: vec![],
                    nsid: 0, // Invalid namespace ID per NVMe spec.
                    identify_ctrl: [0; 4096],
                    device_id: "".to_string(),
                    namespace: None,
                    bar0_va: None,
                    qsize: worker_state.qsize,
                    max_io_queues: worker_state.max_io_queues,
                    vfio_state: VfioDeviceSavedState {
                        pci_id: "".to_string(), // Empty string on error.
                        msix_info_count: 0,
                    }
                }
            }
        }
    }

    /// Save NVMe driver state for servicing.
    pub async fn save(&mut self, worker_state: &mut WorkerState) -> anyhow::Result<NvmeDriverSavedState> {
        let admin = self.admin.as_ref().unwrap().save().await?;
        let mut io: Vec<QueuePairSavedState> = Vec::new();
        for io_q in self.io.iter() {
            io.push(io_q.save().await?);
        }

        let save_state = NvmeDriverSavedState {
            admin: Some(admin),
            io,
            nsid: 0, // Will be updated by the caller.
            identify_ctrl: [0; 4096], // Will be updated by the caller.
            device_id: "".to_string(), // Will be updated by the caller.
            namespace: None, // Will be updated by the caller.
            bar0_va: None, // Will be updated by the caller.
            qsize: worker_state.qsize,
            max_io_queues: worker_state.max_io_queues,
            vfio_state: VfioDeviceSavedState {
                pci_id: self.device.id().to_owned(),
                msix_info_count: self.device.max_interrupt_count(),
            }
        };

        Ok(save_state)
    }
}

impl<T: DeviceBacking> InspectTask<WorkerState> for DriverWorkerTask<T> {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&WorkerState>) {
        req.respond().merge(self).merge(state);
    }
}

/// Save/restore state for NVMe driver.
#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "openhcl.nvme")]
pub struct NvmeDriverSavedState {
    /// Namespace ID.
    #[mesh(1)]
    pub nsid: u32,
    /// Admin queue state.
    #[mesh(2)]
    pub admin: Option<QueuePairSavedState>,
    /// IO queue states.
    #[mesh(3)]
    pub io: Vec<QueuePairSavedState>,
    /// Copy of the controller's IDENTIFY structure.
    #[mesh(4)]
    pub identify_ctrl: [u8; 4096],
    /// Device ID string.
    #[mesh(5)]
    pub device_id: String,
    /// Namespace data.
    #[mesh(6)]
    pub namespace: Option<crate::namespace::SavedNamespaceData>,
    /// BAR0 mapping.
    #[mesh(7)]
    pub bar0_va: Option<u64>,
    /// Queue size as determined by CAP.MQES.
    #[mesh(8)]
    pub qsize: u16,
    /// Max number of IO queue pairs.
    #[mesh(9)]
    pub max_io_queues: u16,
    /// State of the attached VFIO device.
    #[mesh(10)]
    pub vfio_state: VfioDeviceSavedState,
}
