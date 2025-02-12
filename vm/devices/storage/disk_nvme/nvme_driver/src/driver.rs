// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the device driver core.

use super::spec;
use crate::driver::save_restore::IoQueueSavedState;
use crate::queue_pair::admin_cmd;
use crate::queue_pair::Issuer;
use crate::queue_pair::QueuePair;
use crate::registers::Bar0;
use crate::registers::DeviceRegisters;
use crate::Namespace;
use crate::NamespaceError;
use crate::NvmeDriverSavedState;
use crate::RequestError;
use crate::NVME_PAGE_SHIFT;
use anyhow::Context as _;
use futures::future::join_all;
use futures::StreamExt;
use inspect::Inspect;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use save_restore::NvmeDriverWorkerSavedState;
use std::sync::Arc;
use std::sync::OnceLock;
use task_control::AsyncRun;
use task_control::InspectTask;
use task_control::TaskControl;
use thiserror::Error;
use tracing::info_span;
use tracing::Instrument;
use user_driver::backoff::Backoff;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::DeviceBacking;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// An NVMe driver.
///
/// Note that if this is dropped, the process will abort. Call
/// [`NvmeDriver::shutdown`] to drop this.
///
/// Further, note that this is an internal interface to be used
/// only by `NvmeDisk`! Remove any sanitization in `fuzz_nvm_driver.rs`
/// if this struct is used anywhere else.
#[derive(Inspect)]
pub struct NvmeDriver<T: DeviceBacking> {
    #[inspect(flatten)]
    task: Option<TaskControl<DriverWorkerTask<T>, WorkerState>>,
    device_id: String,
    identify: Option<Arc<spec::IdentifyController>>,
    #[inspect(skip)]
    driver: VmTaskDriver,
    #[inspect(skip)]
    admin: Option<Arc<Issuer>>,
    #[inspect(skip)]
    io_issuers: Arc<IoIssuers>,
    #[inspect(skip)]
    rescan_event: Arc<event_listener::Event>,
    /// NVMe namespaces associated with this driver.
    #[inspect(skip)]
    namespaces: Vec<Arc<Namespace>>,
    /// Keeps the controller connected (CC.EN==1) while servicing.
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

/// An error restoring from saved state.
#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("invalid data")]
    InvalidData,
}

#[derive(Inspect)]
struct IoQueue {
    queue: QueuePair,
    iv: u16,
    cpu: u32,
}

impl IoQueue {
    pub async fn save(&self) -> anyhow::Result<IoQueueSavedState> {
        Ok(IoQueueSavedState {
            cpu: self.cpu,
            iv: self.iv as u32,
            queue_data: self.queue.save().await?,
        })
    }

    pub fn restore(
        spawner: VmTaskDriver,
        interrupt: DeviceInterrupt,
        registers: Arc<DeviceRegisters<impl DeviceBacking>>,
        mem_block: MemoryBlock,
        saved_state: &IoQueueSavedState,
    ) -> anyhow::Result<Self> {
        let IoQueueSavedState {
            cpu,
            iv,
            queue_data,
        } = saved_state;
        let queue =
            QueuePair::restore(spawner, interrupt, registers.clone(), mem_block, queue_data)?;

        Ok(Self {
            queue,
            iv: *iv as u16,
            cpu: *cpu,
        })
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
    Save(Rpc<(), anyhow::Result<NvmeDriverWorkerSavedState>>),
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

    /// Initializes but does not enable the device. DMA memory
    /// is preallocated from backing device.
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
            namespaces: vec![],
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
        let admin_len = std::cmp::min(QueuePair::MAX_SQ_ENTRIES, QueuePair::MAX_CQ_ENTRIES);
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
                Arc::get_mut(identify).unwrap().as_mut_bytes(),
            )
            .await
            .context("failed to identify controller")?;

        // Configure the number of IO queues.
        //
        // Note that interrupt zero is shared between IO queue 1 and the admin queue.
        let max_interrupt_count = worker.device.max_interrupt_count();
        if max_interrupt_count == 0 {
            anyhow::bail!("bad device behavior: max_interrupt_count == 0");
        }

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
            if worker.registers.cap.mqes_z() < 1 {
                anyhow::bail!("bad device behavior. mqes cannot be 0");
            }

            let io_cqsize = (QueuePair::MAX_CQ_ENTRIES - 1).min(worker.registers.cap.mqes_z()) + 1;
            let io_sqsize = (QueuePair::MAX_SQ_ENTRIES - 1).min(worker.registers.cap.mqes_z()) + 1;

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
        // lazily. Numbering for I/O queues starts with 1 (0 is Admin).
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

    fn reset(&mut self) -> impl Send + std::future::Future<Output = ()> + use<T> {
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
    pub async fn namespace(&self, nsid: u32) -> Result<Namespace, NamespaceError> {
        Namespace::new(
            &self.driver,
            self.admin.as_ref().unwrap().clone(),
            self.rescan_event.clone(),
            self.identify.clone().unwrap(),
            &self.io_issuers,
            nsid,
        )
        .await
    }

    /// Returns the number of CPUs that are in fallback mode (that are using a
    /// remote CPU's queue due to a failure or resource limitation).
    pub fn fallback_cpu_count(&self) -> usize {
        self.io_issuers
            .per_cpu
            .iter()
            .enumerate()
            .filter(|&(cpu, c)| c.get().is_some_and(|c| c.cpu != cpu as u32))
            .count()
    }

    /// Saves the NVMe driver state during servicing.
    pub async fn save(&mut self) -> anyhow::Result<NvmeDriverSavedState> {
        // Nothing to save if Identify Controller was never queried.
        if self.identify.is_none() {
            return Err(save_restore::Error::InvalidState.into());
        }
        self.nvme_keepalive = true;
        match self
            .io_issuers
            .send
            .call(NvmeWorkerRequest::Save, ())
            .await?
        {
            Ok(s) => {
                // TODO: The decision is to re-query namespace data after the restore.
                // Leaving the code in place so it can be restored in future.
                // The reason is uncertainty about namespace change during servicing.
                // ------
                // for ns in &self.namespaces {
                //     s.namespaces.push(ns.save()?);
                // }
                Ok(NvmeDriverSavedState {
                    identify_ctrl: spec::IdentifyController::read_from_bytes(
                        self.identify.as_ref().unwrap().as_bytes(),
                    )
                    .unwrap(),
                    device_id: self.device_id.clone(),
                    // TODO: See the description above, save the vector once resolved.
                    namespaces: vec![],
                    worker_data: s,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Restores NVMe driver state after servicing.
    pub async fn restore(
        driver_source: &VmTaskDriverSource,
        cpu_count: u32,
        mut device: T,
        saved_state: &NvmeDriverSavedState,
    ) -> anyhow::Result<Self> {
        let driver = driver_source.simple();
        let bar0_mapping = device
            .map_bar(0)
            .context("failed to map device registers")?;
        let bar0 = Bar0(bar0_mapping);

        // It is expected the device to be alive when restoring.
        if !bar0.csts().rdy() {
            anyhow::bail!("device is gone");
        }

        let registers = Arc::new(DeviceRegisters::new(bar0));

        let (send, recv) = mesh::channel();
        let io_issuers = Arc::new(IoIssuers {
            per_cpu: (0..cpu_count).map(|_| OnceLock::new()).collect(),
            send,
        });

        let mut this = Self {
            device_id: device.id().to_owned(),
            task: Some(TaskControl::new(DriverWorkerTask {
                device,
                driver: driver.clone(),
                registers: registers.clone(),
                admin: None, // Updated below.
                io: Vec::new(),
                io_issuers: io_issuers.clone(),
                recv,
            })),
            admin: None, // Updated below.
            identify: Some(Arc::new(
                spec::IdentifyController::read_from_bytes(saved_state.identify_ctrl.as_bytes())
                    .map_err(|_| RestoreError::InvalidData)?, // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            )),
            driver: driver.clone(),
            io_issuers,
            rescan_event: Default::default(),
            namespaces: vec![],
            nvme_keepalive: true,
        };

        let task = &mut this.task.as_mut().unwrap();
        let worker = task.task_mut();

        // Interrupt 0 is shared between admin queue and I/O queue 1.
        let interrupt0 = worker
            .device
            .map_interrupt(0, 0)
            .context("failed to map interrupt 0")?;

        let dma_client = worker.device.dma_client();

        // Restore the admin queue pair.
        let admin = saved_state
            .worker_data
            .admin
            .as_ref()
            .map(|a| {
                // Restore memory block for admin queue pair.
                let mem_block = dma_client
                    .attach_dma_buffer(a.mem_len, a.base_pfn)
                    .expect("unable to restore mem block");
                QueuePair::restore(driver.clone(), interrupt0, registers.clone(), mem_block, a)
                    .unwrap()
            })
            .unwrap();

        let admin = worker.admin.insert(admin);

        // Spawn a task to handle asynchronous events.
        let async_event_task = this.driver.spawn("nvme_async_event", {
            let admin = admin.issuer().clone();
            let rescan_event = this.rescan_event.clone();
            async move {
                if let Err(err) = handle_asynchronous_events(&admin, &rescan_event).await {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "asynchronous event failure, not processing any more"
                    );
                }
            }
        });

        let state = WorkerState {
            qsize: saved_state.worker_data.qsize,
            async_event_task,
            max_io_queues: saved_state.worker_data.max_io_queues,
        };

        this.admin = Some(admin.issuer().clone());

        // Restore I/O queues.
        // Interrupt vector 0 is shared between Admin queue and I/O queue #1.
        worker.io = saved_state
            .worker_data
            .io
            .iter()
            .flat_map(|q| -> Result<IoQueue, anyhow::Error> {
                let interrupt = worker
                    .device
                    .map_interrupt(q.iv, q.cpu)
                    .context("failed to map interrupt")?;
                let mem_block =
                    dma_client.attach_dma_buffer(q.queue_data.mem_len, q.queue_data.base_pfn)?;
                let q =
                    IoQueue::restore(driver.clone(), interrupt, registers.clone(), mem_block, q)?;
                let issuer = IoIssuer {
                    issuer: q.queue.issuer().clone(),
                    cpu: q.cpu,
                };
                this.io_issuers.per_cpu[q.cpu as usize].set(issuer).unwrap();
                Ok(q)
            })
            .collect();

        // Restore namespace(s).
        for ns in &saved_state.namespaces {
            // TODO: Current approach is to re-query namespace data after servicing
            // and this array will be empty. Once we confirm that we can process
            // namespace change notification AEN, the restore code will be re-added.
            this.namespaces.push(Arc::new(Namespace::restore(
                &driver,
                admin.issuer().clone(),
                this.rescan_event.clone(),
                this.identify.clone().unwrap(),
                &this.io_issuers,
                ns,
            )?));
        }

        task.insert(&this.driver, "nvme_worker", state);
        task.start();

        Ok(this)
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
                        list.as_mut_bytes(),
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
                    }
                    Some(NvmeWorkerRequest::Save(rpc)) => rpc.handle(|_| self.save(state)).await,
                    None => break,
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

    /// Save NVMe driver state for servicing.
    pub async fn save(
        &mut self,
        worker_state: &mut WorkerState,
    ) -> anyhow::Result<NvmeDriverWorkerSavedState> {
        let admin = match self.admin.as_ref() {
            Some(a) => Some(a.save().await?),
            None => None,
        };

        let io = join_all(self.io.drain(..).map(|q| async move { q.save().await }))
            .await
            .into_iter()
            .flatten()
            .collect();

        Ok(NvmeDriverWorkerSavedState {
            admin,
            io,
            qsize: worker_state.qsize,
            max_io_queues: worker_state.max_io_queues,
        })
    }
}

impl<T: DeviceBacking> InspectTask<WorkerState> for DriverWorkerTask<T> {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&WorkerState>) {
        req.respond().merge(self).merge(state);
    }
}

pub mod save_restore {
    use super::*;

    /// Save and Restore errors for this module.
    #[derive(Debug, Error)]
    pub enum Error {
        /// No data to save.
        #[error("invalid object state")]
        InvalidState,
    }

    /// Save/restore state for NVMe driver.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct NvmeDriverSavedState {
        /// Copy of the controller's IDENTIFY structure.
        /// It is defined as Option<> in original structure.
        #[mesh(1, encoding = "mesh::payload::encoding::ZeroCopyEncoding")]
        pub identify_ctrl: spec::IdentifyController,
        /// Device ID string.
        #[mesh(2)]
        pub device_id: String,
        /// Namespace data.
        #[mesh(3)]
        pub namespaces: Vec<SavedNamespaceData>,
        /// NVMe driver worker task data.
        #[mesh(4)]
        pub worker_data: NvmeDriverWorkerSavedState,
    }

    /// Save/restore state for NVMe driver worker task.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct NvmeDriverWorkerSavedState {
        /// Admin queue state.
        #[mesh(1)]
        pub admin: Option<QueuePairSavedState>,
        /// IO queue states.
        #[mesh(2)]
        pub io: Vec<IoQueueSavedState>,
        /// Queue size as determined by CAP.MQES.
        #[mesh(3)]
        pub qsize: u16,
        /// Max number of IO queue pairs.
        #[mesh(4)]
        pub max_io_queues: u16,
    }

    /// Save/restore state for QueuePair.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct QueuePairSavedState {
        /// Allocated memory size in bytes.
        #[mesh(1)]
        pub mem_len: usize,
        /// First PFN of the physically contiguous block.
        #[mesh(2)]
        pub base_pfn: u64,
        /// Queue ID used when creating the pair
        /// (SQ and CQ IDs are using same number).
        #[mesh(3)]
        pub qid: u16,
        /// Submission queue entries.
        #[mesh(4)]
        pub sq_entries: u16,
        /// Completion queue entries.
        #[mesh(5)]
        pub cq_entries: u16,
        /// QueueHandler task data.
        #[mesh(6)]
        pub handler_data: QueueHandlerSavedState,
    }

    /// Save/restore state for IoQueue.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct IoQueueSavedState {
        #[mesh(1)]
        /// Which CPU handles requests.
        pub cpu: u32,
        #[mesh(2)]
        /// Interrupt vector (MSI-X)
        pub iv: u32,
        #[mesh(3)]
        pub queue_data: QueuePairSavedState,
    }

    /// Save/restore state for QueueHandler task.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct QueueHandlerSavedState {
        #[mesh(1)]
        pub sq_state: SubmissionQueueSavedState,
        #[mesh(2)]
        pub cq_state: CompletionQueueSavedState,
        #[mesh(3)]
        pub pending_cmds: PendingCommandsSavedState,
    }

    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct SubmissionQueueSavedState {
        #[mesh(1)]
        pub sqid: u16,
        #[mesh(2)]
        pub head: u32,
        #[mesh(3)]
        pub tail: u32,
        #[mesh(4)]
        pub committed_tail: u32,
        #[mesh(5)]
        pub len: u32,
    }

    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct CompletionQueueSavedState {
        #[mesh(1)]
        pub cqid: u16,
        #[mesh(2)]
        pub head: u32,
        #[mesh(3)]
        pub committed_head: u32,
        #[mesh(4)]
        pub len: u32,
        #[mesh(5)]
        /// NVMe completion tag.
        pub phase: bool,
    }

    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct PendingCommandSavedState {
        #[mesh(1, encoding = "mesh::payload::encoding::ZeroCopyEncoding")]
        pub command: spec::Command,
    }

    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct PendingCommandsSavedState {
        #[mesh(1)]
        pub commands: Vec<PendingCommandSavedState>,
        #[mesh(2)]
        pub next_cid_high_bits: u16,
        #[mesh(3)]
        pub cid_key_bits: u32,
    }

    /// NVMe namespace data.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct SavedNamespaceData {
        #[mesh(1)]
        pub nsid: u32,
        #[mesh(2, encoding = "mesh::payload::encoding::ZeroCopyEncoding")]
        pub identify_ns: nvme_spec::nvm::IdentifyNamespace,
    }
}
