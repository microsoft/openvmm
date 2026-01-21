// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the device driver core.

use super::spec;
use crate::NVME_PAGE_SHIFT;
use crate::NamespaceError;
use crate::NamespaceHandle;
use crate::RequestError;
use crate::driver::save_restore::IoQueueSavedState;
use crate::namespace::Namespace;
use crate::queue_pair::AdminAerHandler;
use crate::queue_pair::Issuer;
use crate::queue_pair::MAX_CQ_ENTRIES;
use crate::queue_pair::MAX_SQ_ENTRIES;
use crate::queue_pair::NoOpAerHandler;
use crate::queue_pair::QueuePair;
use crate::queue_pair::admin_cmd;
use crate::registers::Bar0;
use crate::registers::DeviceRegisters;
use crate::save_restore::NvmeDriverSavedState;
use anyhow::Context as _;
use futures::StreamExt;
use futures::future::join_all;
use inspect::Inspect;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::RwLock;
use save_restore::NvmeDriverWorkerSavedState;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use task_control::AsyncRun;
use task_control::InspectTask;
use task_control::TaskControl;
use thiserror::Error;
use tracing::Instrument;
use tracing::Span;
use tracing::info_span;
use user_driver::DeviceBacking;
use user_driver::backoff::Backoff;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
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
pub struct NvmeDriver<D: DeviceBacking> {
    #[inspect(flatten)]
    task: Option<TaskControl<DriverWorkerTask<D>, WorkerState>>,
    device_id: String,
    identify: Option<Arc<spec::IdentifyController>>,
    #[inspect(skip)]
    driver: VmTaskDriver,
    #[inspect(skip)]
    admin: Option<Arc<Issuer>>,
    #[inspect(skip)]
    io_issuers: Arc<IoIssuers>,
    #[inspect(skip)]
    rescan_notifiers: Arc<RwLock<HashMap<u32, mesh::Sender<()>>>>,
    /// NVMe namespaces associated with this driver. Mapping nsid to NamespaceHandle.
    #[inspect(skip)]
    namespaces: HashMap<u32, WeakOrStrong<Namespace>>,
    /// Keeps the controller connected (CC.EN==1) while servicing.
    nvme_keepalive: bool,
    bounce_buffer: bool,
}

/// This type allows the driver to store either a weak or a strong reference to
/// a type T (in this case, Namespace) in the same map. The idea here is that
/// during normal operation the driver should ONLY hold weak refernces to
/// Namespaces. However, during save/restore, we need to hold strong references
/// after restoring namespace from saved state until they are requested for by
/// the StorageController. When a restored Namespace is requested, we can downgrade
/// the strong reference and then proceed as normal.
enum WeakOrStrong<T> {
    Weak(Weak<T>),
    Strong(Arc<T>),
}

impl<T> WeakOrStrong<T> {
    /// Returns a strong reference to the underlying value.
    /// If this is a strong pointer, returns a clone of the Arc.
    /// If this is a weak pointer, returns the result of upgrade() (None if gone).
    pub fn get_arc(&mut self) -> Option<Arc<T>> {
        match self {
            WeakOrStrong::Strong(arc) => {
                let strong = arc.clone();
                *self = WeakOrStrong::Weak(Arc::downgrade(arc));
                Some(strong)
            }
            WeakOrStrong::Weak(weak) => weak.upgrade(),
        }
    }

    pub fn is_weak(&self) -> bool {
        matches!(self, WeakOrStrong::Weak(_))
    }
}

#[derive(Inspect)]
struct DriverWorkerTask<D: DeviceBacking> {
    /// The VFIO device backing this driver. For KeepAlive cases, the VFIO handle
    /// is never dropped, otherwise there is a chance that VFIO will reset the
    /// device. We don't want that.
    ///
    /// Dropped in `NvmeDriver::reset`.
    device: ManuallyDrop<D>,
    #[inspect(skip)]
    driver: VmTaskDriver,
    registers: Arc<DeviceRegisters<D>>,
    admin: Option<QueuePair<AdminAerHandler, D>>,
    #[inspect(iter_by_index)]
    io: Vec<IoQueue<D>>,
    /// Prototype IO queues for restoring from saved state. These are queues
    /// that were created on the device at some point, but had no pending
    /// IOs at save/restore time. These will be promoted to full IO queues
    /// on demand.
    ///
    /// cpu => queue info
    #[inspect(skip)]
    proto_io: HashMap<u32, ProtoIoQueue>,
    /// The next qid to use when creating an IO queue for a new issuer.
    next_ioq_id: u16,
    io_issuers: Arc<IoIssuers>,
    #[inspect(skip)]
    recv: mesh::Receiver<NvmeWorkerRequest>,
    bounce_buffer: bool,
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

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("no more io queues available, reached maximum {0}")]
    NoMoreIoQueues(u16),
    #[error("failed to map interrupt")]
    InterruptMapFailure(#[source] anyhow::Error),
    #[error("failed to create io queue pair {1}")]
    IoQueuePairCreationFailure(#[source] anyhow::Error, u16),
    #[error("failed to create io completion queue {1}")]
    IoCompletionQueueFailure(#[source] anyhow::Error, u16),
    #[error("failed to create io submission queue {1}")]
    IoSubmissionQueueFailure(#[source] anyhow::Error, u16),
    // Other device related errors
    #[error(transparent)]
    Other(anyhow::Error),
}

#[derive(Debug, Clone)]
struct ProtoIoQueue {
    save_state: IoQueueSavedState,
    mem: MemoryBlock,
}

#[derive(Inspect)]
struct IoQueue<D: DeviceBacking> {
    queue: QueuePair<NoOpAerHandler, D>,
    iv: u16,
    cpu: u32,
}

impl<D: DeviceBacking> IoQueue<D> {
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
        registers: Arc<DeviceRegisters<D>>,
        mem_block: MemoryBlock,
        saved_state: &IoQueueSavedState,
        bounce_buffer: bool,
    ) -> anyhow::Result<Self> {
        let IoQueueSavedState {
            cpu,
            iv,
            queue_data,
        } = saved_state;
        let queue = QueuePair::restore(
            spawner,
            interrupt,
            registers.clone(),
            mem_block,
            queue_data,
            bounce_buffer,
            NoOpAerHandler,
        )?;

        Ok(Self {
            queue,
            iv: *iv as u16,
            cpu: *cpu,
        })
    }
}

#[derive(Debug, Inspect)]
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
    Save(Rpc<Span, anyhow::Result<NvmeDriverWorkerSavedState>>),
}

impl<D: DeviceBacking> NvmeDriver<D> {
    /// Initializes the driver.
    pub async fn new(
        driver_source: &VmTaskDriverSource,
        cpu_count: u32,
        device: D,
        bounce_buffer: bool,
    ) -> anyhow::Result<Self> {
        let pci_id = device.id().to_owned();
        let mut this = Self::new_disabled(driver_source, cpu_count, device, bounce_buffer)
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
        mut device: D,
        bounce_buffer: bool,
    ) -> anyhow::Result<Self> {
        let driver = driver_source.simple();
        let bar0 = Bar0(
            device
                .map_bar(0)
                .context("failed to map device registers")?,
        );

        let cc = bar0.cc();
        if cc.en() || bar0.csts().rdy() {
            if let Err(e) = bar0
                .reset(&driver)
                .instrument(tracing::info_span!(
                    "nvme_already_enabled",
                    pci_id = device.id().to_owned()
                ))
                .await
            {
                anyhow::bail!("device is gone, csts: {:#x}", e);
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
                device: ManuallyDrop::new(device),
                driver: driver.clone(),
                registers,
                admin: None,
                io: Vec::new(),
                proto_io: HashMap::new(),
                next_ioq_id: 1,
                io_issuers: io_issuers.clone(),
                recv,
                bounce_buffer,
            })),
            admin: None,
            identify: None,
            driver,
            io_issuers,
            rescan_notifiers: Default::default(),
            namespaces: Default::default(),
            nvme_keepalive: false,
            bounce_buffer,
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
        let admin_len = std::cmp::min(MAX_SQ_ENTRIES, MAX_CQ_ENTRIES);
        let admin_sqes = admin_len;
        let admin_cqes = admin_len;

        let interrupt0 = worker
            .device
            .map_interrupt(0, 0)
            .context("failed to map interrupt 0")?;

        // Start the admin queue pair.
        let admin = QueuePair::new(
            self.driver.clone(),
            worker.device.deref(),
            ADMIN_QID,
            admin_sqes,
            admin_cqes,
            interrupt0,
            worker.registers.clone(),
            self.bounce_buffer,
            AdminAerHandler::new(),
        )
        .context("failed to create admin queue pair")?;

        let admin_sqes = admin.sq_entries();
        let admin_cqes = admin.cq_entries();

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
            let csts_val: u32 = csts.into();
            if csts_val == !0 {
                anyhow::bail!("device is gone, csts: {:#x}", csts_val);
            }
            if csts.cfs() {
                // Attempt to leave the device in reset state CC.EN 1 -> 0.
                let after_reset = if let Err(e) = worker.registers.bar0.reset(&self.driver).await {
                    e
                } else {
                    0
                };
                anyhow::bail!(
                    "device had fatal error, csts: {:#x}, after reset: {:#}",
                    csts_val,
                    after_reset
                );
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
                pci_id = ?worker.device.id(),
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
                pci_id = ?worker.device.id(),
                "queue count constrained by hardware queue count"
            );
        }

        let max_io_queues = allocated_io_queue_count.min(requested_io_queue_count);

        let qsize = {
            if worker.registers.cap.mqes_z() < 1 {
                anyhow::bail!("bad device behavior. mqes cannot be 0");
            }

            let io_cqsize = (MAX_CQ_ENTRIES - 1).min(worker.registers.cap.mqes_z()) + 1;
            let io_sqsize = (MAX_SQ_ENTRIES - 1).min(worker.registers.cap.mqes_z()) + 1;

            tracing::debug!(
                io_cqsize,
                io_sqsize,
                hw_size = worker.registers.cap.mqes_z(),
                pci_id = ?worker.device.id(),
                "io queue sizes"
            );

            // Some hardware (such as ASAP) require that the sq and cq have the same size.
            io_cqsize.min(io_sqsize)
        };

        // Spawn a task to handle asynchronous events.
        let async_event_task = self.driver.spawn("nvme_async_event", {
            let admin = admin.issuer().clone();
            let rescan_notifiers = self.rescan_notifiers.clone();
            async move {
                if let Err(err) = handle_asynchronous_events(&admin, rescan_notifiers).await {
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
        tracing::debug!(pci_id = ?self.device_id, "shutting down nvme driver");

        // If nvme_keepalive was requested, return early.
        // The memory is still aliased as we don't flush pending IOs.
        if self.nvme_keepalive {
            return;
        }
        self.reset().await;
        drop(self);
    }

    fn reset(&mut self) -> impl Send + Future<Output = ()> + use<D> {
        let driver = self.driver.clone();
        let id = self.device_id.clone();
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
            if let Err(e) = worker.registers.bar0.reset(&driver).await {
                tracing::info!(csts = e, "device reset failed");
            }

            let _vfio = ManuallyDrop::into_inner(worker.device);
            tracing::debug!(pci_id = ?id, "dropping vfio handle to device");
        }
    }

    /// Gets the namespace with namespace ID `nsid`.
    pub async fn namespace(&mut self, nsid: u32) -> Result<NamespaceHandle, NamespaceError> {
        if let Some(namespace) = self.namespaces.get_mut(&nsid) {
            // After restore we will have a strong ref -> downgrade and return.
            // If we have a weak ref, make sure it is not upgradeable (that means we have a duplicate somewhere).
            let is_weak = namespace.is_weak(); // This value will change after invoking get_arc().
            let namespace = namespace.get_arc();
            if let Some(namespace) = namespace {
                if is_weak && namespace.check_active().is_ok() {
                    return Err(NamespaceError::Duplicate(nsid));
                }

                return Ok(NamespaceHandle::new(namespace));
            }
        }

        let (send, recv) = mesh::channel::<()>();
        let namespace = Arc::new(
            Namespace::new(
                &self.driver,
                self.admin.as_ref().unwrap().clone(),
                recv,
                self.identify.clone().unwrap(),
                &self.io_issuers,
                nsid,
            )
            .await?,
        );
        self.namespaces
            .insert(nsid, WeakOrStrong::Weak(Arc::downgrade(&namespace)));

        // Append the sender to the list of notifiers for this nsid.
        let mut notifiers = self.rescan_notifiers.write();
        notifiers.insert(nsid, send);
        Ok(NamespaceHandle::new(namespace))
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
        let span = tracing::info_span!("nvme_driver_save", pci_id = self.device_id);
        self.nvme_keepalive = true;
        match self
            .io_issuers
            .send
            .call(NvmeWorkerRequest::Save, span.clone())
            .instrument(span.clone())
            .await?
        {
            Ok(s) => {
                let _e = span.entered();
                tracing::info!(
                    namespaces = self
                        .namespaces
                        .keys()
                        .map(|nsid| nsid.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    "saving namespaces",
                );
                let mut saved_namespaces = vec![];
                for (nsid, namespace) in self.namespaces.iter_mut() {
                    let is_weak = namespace.is_weak(); // This value will change after invoking get_arc().
                    if let Some(ns) = namespace.get_arc()
                        && ns.check_active().is_ok()
                        && is_weak
                    {
                        saved_namespaces.push(ns.save().with_context(|| {
                            format!(
                                "failed to save namespace nsid {} device {}",
                                nsid, self.device_id
                            )
                        })?);
                    }
                }
                Ok(NvmeDriverSavedState {
                    identify_ctrl: spec::IdentifyController::read_from_bytes(
                        self.identify.as_ref().unwrap().as_bytes(),
                    )
                    .unwrap(),
                    device_id: self.device_id.clone(),
                    namespaces: saved_namespaces,
                    worker_data: s,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// This should only be called during restore if keepalive is no longer
    /// supported and the previously enabled device needs to be reset. It
    /// performs a controller reset by setting cc.en to 0. It will then also
    /// drop the given device instance.
    pub async fn clear_existing_state(
        driver_source: &VmTaskDriverSource,
        mut device: D,
    ) -> anyhow::Result<()> {
        let driver = driver_source.simple();
        let bar0_mapping = device
            .map_bar(0)
            .context("failed to map device registers to clear existing state")?;
        let bar0 = Bar0(bar0_mapping);
        bar0.reset(&driver)
            .await
            .map_err(|e| anyhow::anyhow!("failed to reset device during clear: {:#x}", e))?;
        Ok(())
    }

    /// Restores NVMe driver state after servicing.
    pub async fn restore(
        driver_source: &VmTaskDriverSource,
        cpu_count: u32,
        mut device: D,
        saved_state: &NvmeDriverSavedState,
        bounce_buffer: bool,
    ) -> anyhow::Result<Self> {
        let pci_id = device.id().to_owned();
        let driver = driver_source.simple();
        let bar0_mapping = device
            .map_bar(0)
            .context("failed to map device registers")?;
        let bar0 = Bar0(bar0_mapping);

        // It is expected for the device to be alive when restoring.
        let csts = bar0.csts();
        if !csts.rdy() {
            tracing::error!(
                csts = u32::from(csts),
                ?pci_id,
                "device is not ready during restore"
            );
            anyhow::bail!(
                "device is not ready during restore, csts: {:#x}",
                u32::from(csts)
            );
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
                device: ManuallyDrop::new(device),
                driver: driver.clone(),
                registers: registers.clone(),
                admin: None, // Updated below.
                io: Vec::new(),
                proto_io: HashMap::new(),
                next_ioq_id: 1,
                io_issuers: io_issuers.clone(),
                recv,
                bounce_buffer,
            })),
            admin: None, // Updated below.
            identify: Some(Arc::new(
                spec::IdentifyController::read_from_bytes(saved_state.identify_ctrl.as_bytes())
                    .map_err(|_| RestoreError::InvalidData)?,
            )),
            driver: driver.clone(),
            io_issuers,
            rescan_notifiers: Default::default(),
            namespaces: Default::default(),
            nvme_keepalive: true,
            bounce_buffer,
        };

        let task = &mut this.task.as_mut().unwrap();
        let worker = task.task_mut();

        // Interrupt 0 is shared between admin queue and I/O queue 1.
        let interrupt0 = worker
            .device
            .map_interrupt(0, 0)
            .with_context(|| format!("failed to map interrupt 0 for {}", pci_id))?;

        let dma_client = worker.device.dma_client();
        let restored_memory = dma_client
            .attach_pending_buffers()
            .with_context(|| format!("failed to restore allocations for {}", pci_id))?;

        // Restore the admin queue pair.
        let admin = saved_state
            .worker_data
            .admin
            .as_ref()
            .map(|a| {
                tracing::info!(
                    id = a.qid,
                    pending_commands_count = a.handler_data.pending_cmds.commands.len(),
                    ?pci_id,
                    "restoring admin queue",
                );
                // Restore memory block for admin queue pair.
                let mem_block = restored_memory
                    .iter()
                    .find(|mem| mem.len() == a.mem_len && a.base_pfn == mem.pfns()[0])
                    .expect("unable to find restored mem block")
                    .to_owned();
                QueuePair::restore(
                    driver.clone(),
                    interrupt0,
                    registers.clone(),
                    mem_block,
                    a,
                    bounce_buffer,
                    AdminAerHandler::new(),
                )
                .expect("failed to restore admin queue pair")
            })
            .expect("attempted to restore admin queue from empty state");

        let admin = worker.admin.insert(admin);

        // Spawn a task to handle asynchronous events.
        let async_event_task = this.driver.spawn("nvme_async_event", {
            let admin = admin.issuer().clone();
            let rescan_notifiers = this.rescan_notifiers.clone();
            async move {
                if let Err(err) = handle_asynchronous_events(&admin, rescan_notifiers)
                    .instrument(tracing::info_span!("async_event_handler"))
                    .await
                {
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

        tracing::info!(
            state = saved_state
                .worker_data
                .io
                .iter()
                .map(|io_state| format!(
                    "{{qid={}, pending_commands_count={}}}",
                    io_state.queue_data.qid,
                    io_state.queue_data.handler_data.pending_cmds.commands.len()
                ))
                .collect::<Vec<_>>()
                .join(", "),
            ?pci_id,
            "restoring io queues",
        );

        // Restore I/O queues.
        // (1) Restore qid1 and any queues that have pending commands.
        // Interrupt vector 0 is shared between Admin queue and I/O queue #1.
        let mut max_seen_qid = 1;
        worker.io = saved_state
            .worker_data
            .io
            .iter()
            .filter(|q| {
                q.queue_data.qid == 1 || !q.queue_data.handler_data.pending_cmds.commands.is_empty()
            })
            .flat_map(|q| -> Result<IoQueue<D>, anyhow::Error> {
                let qid = q.queue_data.qid;
                let cpu = q.cpu;
                tracing::info!(qid, cpu, ?pci_id, "restoring queue");
                max_seen_qid = max_seen_qid.max(qid);
                let interrupt = worker.device.map_interrupt(q.iv, q.cpu).with_context(|| {
                    format!(
                        "failed to map interrupt for {}, cpu {}, iv {}",
                        pci_id, q.cpu, q.iv
                    )
                })?;
                tracing::info!(qid, cpu, ?pci_id, "restoring queue: search for mem block");
                let mem_block = restored_memory
                    .iter()
                    .find(|mem| {
                        mem.len() == q.queue_data.mem_len && q.queue_data.base_pfn == mem.pfns()[0]
                    })
                    .expect("unable to find restored mem block")
                    .to_owned();
                tracing::info!(qid, cpu, ?pci_id, "restoring queue: restore IoQueue");
                let q = IoQueue::restore(
                    driver.clone(),
                    interrupt,
                    registers.clone(),
                    mem_block,
                    q,
                    bounce_buffer,
                )?;
                tracing::info!(qid, cpu, ?pci_id, "restoring queue: create issuer");
                let issuer = IoIssuer {
                    issuer: q.queue.issuer().clone(),
                    cpu: q.cpu,
                };
                this.io_issuers.per_cpu[q.cpu as usize].set(issuer).unwrap();
                Ok(q)
            })
            .collect();

        // (2) Create prototype entries for any queues that don't currently have outstanding commands.
        // They will be restored on demand later.
        worker.proto_io = saved_state
            .worker_data
            .io
            .iter()
            .filter(|q| {
                q.queue_data.qid != 1 && q.queue_data.handler_data.pending_cmds.commands.is_empty()
            })
            .map(|q| {
                // Create a prototype IO queue entry.
                tracing::info!(
                    qid = q.queue_data.qid,
                    cpu = q.cpu,
                    ?pci_id,
                    "creating prototype io queue entry",
                );
                max_seen_qid = max_seen_qid.max(q.queue_data.qid);
                let mem_block = restored_memory
                    .iter()
                    .find(|mem| {
                        mem.len() == q.queue_data.mem_len && q.queue_data.base_pfn == mem.pfns()[0]
                    })
                    .expect("unable to find restored mem block")
                    .to_owned();
                (
                    q.cpu,
                    ProtoIoQueue {
                        save_state: q.clone(),
                        mem: mem_block,
                    },
                )
            })
            .collect();

        // Update next_ioq_id to avoid reusing qids.
        worker.next_ioq_id = max_seen_qid + 1;

        tracing::info!(
            namespaces = saved_state
                .namespaces
                .iter()
                .map(|ns| format!("{{nsid={}, size={}}}", ns.nsid, ns.identify_ns.nsze))
                .collect::<Vec<_>>()
                .join(", "),
            ?pci_id,
            "restoring namespaces",
        );

        // Restore namespace(s).
        for ns in &saved_state.namespaces {
            let (send, recv) = mesh::channel::<()>();
            this.namespaces.insert(
                ns.nsid,
                WeakOrStrong::Strong(Arc::new(Namespace::restore(
                    &driver,
                    admin.issuer().clone(),
                    recv,
                    this.identify.clone().unwrap(),
                    &this.io_issuers,
                    ns,
                )?)),
            );
            this.rescan_notifiers.write().insert(ns.nsid, send);
        }

        task.insert(&this.driver, "nvme_worker", state);
        task.start();

        Ok(this)
    }

    /// Change device's behavior when servicing.
    pub fn update_servicing_flags(&mut self, nvme_keepalive: bool) {
        tracing::debug!(nvme_keepalive, "updating nvme servicing flags");
        self.nvme_keepalive = nvme_keepalive;
    }
}

async fn handle_asynchronous_events(
    admin: &Issuer,
    rescan_notifiers: Arc<RwLock<HashMap<u32, mesh::Sender<()>>>>,
) -> anyhow::Result<()> {
    tracing::info!("starting asynchronous event handler task");
    loop {
        let dw0 = admin
            .issue_get_aen()
            .await
            .context("asynchronous event request failed")?;

        match spec::AsynchronousEventType(dw0.event_type()) {
            spec::AsynchronousEventType::NOTICE => {
                tracing::info!("received an async notice event (aen) from the controller");

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

                // Notify only the namespaces that have changed.

                // NOTE: The nvme spec states - If more than 1,024 namespaces have
                // changed attributes since the last time the log page was read,
                // the first entry in the log page shall be set to
                // FFFFFFFFh and the remainder of the list shall be zero filled.
                let notifier_guard = rescan_notifiers.read();
                if list[0] == 0xFFFFFFFF && list[1] == 0 {
                    // More than 1024 namespaces changed - notify all registered namespaces
                    tracing::info!("more than 1024 namespaces changed, notifying all listeners");
                    for notifiers in notifier_guard.values() {
                        notifiers.send(());
                    }
                } else {
                    // Notify specific namespaces that have changed
                    for nsid in list.iter().filter(|&&nsid| nsid != 0) {
                        tracing::info!(nsid, "notifying listeners of changed namespace");
                        if let Some(notifier) = notifier_guard.get(nsid) {
                            notifier.send(());
                        }
                    }
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

impl<D: DeviceBacking> Drop for NvmeDriver<D> {
    fn drop(&mut self) {
        tracing::trace!(pci_id = ?self.device_id, ka = self.nvme_keepalive, task = self.task.is_some(), "dropping nvme driver");
        if self.task.is_some() {
            // Do not reset NVMe device when nvme_keepalive is requested.
            tracing::debug!(nvme_keepalive = self.nvme_keepalive, pci_id = ?self.device_id, "dropping nvme driver");
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

impl<D: DeviceBacking> AsyncRun<WorkerState> for DriverWorkerTask<D> {
    async fn run(
        &mut self,
        stop: &mut task_control::StopTask<'_>,
        state: &mut WorkerState,
    ) -> Result<(), task_control::Cancelled> {
        let r = stop
            .until_stopped(async {
                loop {
                    match self.recv.next().await {
                        Some(NvmeWorkerRequest::CreateIssuer(rpc)) => {
                            rpc.handle(async |cpu| self.create_io_issuer(state, cpu).await)
                                .await
                        }
                        Some(NvmeWorkerRequest::Save(rpc)) => {
                            rpc.handle(async |span| {
                                let child_span = tracing::info_span!(
                                    parent: &span,
                                    "nvme_worker_save",
                                    pci_id = %self.device.id()
                                );
                                self.save(state).instrument(child_span).await
                            })
                            .await
                        }
                        None => break,
                    }
                }
            })
            .await;
        tracing::info!(pci_id = %self.device.id(), "nvme worker task exiting");
        r
    }
}

impl<D: DeviceBacking> DriverWorkerTask<D> {
    fn restore_io_issuer(&mut self, proto: ProtoIoQueue) -> anyhow::Result<()> {
        let pci_id = self.device.id().to_owned();
        let qid = proto.save_state.queue_data.qid;
        let cpu = proto.save_state.cpu;

        tracing::info!(
            qid,
            cpu,
            ?pci_id,
            "restoring queue from prototype: mapping interrupt"
        );
        let interrupt = self
            .device
            .map_interrupt(proto.save_state.iv, proto.save_state.cpu)
            .with_context(|| {
                format!(
                    "failed to map interrupt for {}, cpu {}, iv {}",
                    pci_id, proto.save_state.cpu, proto.save_state.iv
                )
            })?;

        tracing::info!(
            qid,
            cpu,
            ?pci_id,
            "restoring queue from prototype: restore IoQueue"
        );
        let queue = IoQueue::restore(
            self.driver.clone(),
            interrupt,
            self.registers.clone(),
            proto.mem,
            &proto.save_state,
            self.bounce_buffer,
        )
        .with_context(|| format!("failed to restore io queue for {}, cpu {}", pci_id, cpu))?;

        tracing::info!(
            qid,
            cpu,
            ?pci_id,
            "restoring queue from prototype: restore complete"
        );

        let issuer = IoIssuer {
            issuer: queue.queue.issuer().clone(),
            cpu,
        };

        self.io_issuers.per_cpu[cpu as usize]
            .set(issuer)
            .expect("issuer already set for this cpu");
        self.io.push(queue);

        Ok(())
    }

    async fn create_io_issuer(&mut self, state: &mut WorkerState, cpu: u32) {
        tracing::debug!(cpu, pci_id = ?self.device.id(), "issuer request");
        if self.io_issuers.per_cpu[cpu as usize].get().is_some() {
            return;
        }

        if let Some(proto) = self.proto_io.remove(&cpu) {
            match self.restore_io_issuer(proto) {
                Ok(()) => return,
                Err(err) => {
                    // The memory block will be dropped as `proto` goes out of scope.
                    //
                    // TODO: in future work, consider trying to issue the NVMe command to delete
                    // the prior IO queue pair. Given that restore failed, and crucially, why
                    // restore failed, that may or may not be the right thing to do. It is probably
                    // the "right" protocol thing to do, though.

                    tracing::error!(
                        pci_id = ?self.device.id(),
                        cpu,
                        error = ?err,
                        "failed to restore io queue from prototype, creating new queue"
                    );
                }
            }
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
                    .expect("unable to find an io issuer for fallback");

                // Log the error as informational only when there is a lack of
                // hardware resources from the device.
                match err {
                    DeviceError::NoMoreIoQueues(_) => {
                        tracing::info!(
                            pci_id = ?self.device.id(),
                            cpu,
                            fallback_cpu,
                            error = &err as &dyn std::error::Error,
                            "failed to create io queue, falling back"
                        );
                    }
                    _ => {
                        tracing::error!(
                            pci_id = ?self.device.id(),
                            cpu,
                            fallback_cpu,
                            error = &err as &dyn std::error::Error,
                            "failed to create io queue, falling back"
                        );
                    }
                }

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
    ) -> Result<IoIssuer, DeviceError> {
        if self.io.len() >= state.max_io_queues as usize {
            return Err(DeviceError::NoMoreIoQueues(state.max_io_queues));
        }

        // qid is 1-based, iv is 0-based.
        // And, IO queue 1 shares interrupt vector 0 with the admin queue.
        let qid = self.next_ioq_id;
        let iv = qid - 1;
        self.next_ioq_id += 1;

        tracing::debug!(cpu, qid, iv, pci_id = ?self.device.id(), "creating io queue");

        let interrupt = self
            .device
            .map_interrupt(iv.into(), cpu)
            .map_err(DeviceError::InterruptMapFailure)?;

        let queue = QueuePair::new(
            self.driver.clone(),
            self.device.deref(),
            qid,
            state.qsize,
            state.qsize,
            interrupt,
            self.registers.clone(),
            self.bounce_buffer,
            NoOpAerHandler,
        )
        .map_err(|err| DeviceError::IoQueuePairCreationFailure(err, qid))?;

        assert_eq!(queue.sq_entries(), queue.cq_entries());
        state.qsize = queue.sq_entries();

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
                .map_err(|err| DeviceError::IoCompletionQueueFailure(err.into(), qid))?;

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
                .map_err(|err| DeviceError::IoSubmissionQueueFailure(err.into(), qid))?;

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
                        pci_id = ?self.device.id(),
                        error = &err as &dyn std::error::Error,
                        "failed to delete completion queue in teardown path"
                    );
                }
            }
            let io = self.io.pop().unwrap();
            io.queue.shutdown().await;
            return Err(DeviceError::Other(err));
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

        let io: Vec<IoQueueSavedState> = join_all(self.io.drain(..).map(async |q| q.save().await))
            .await
            .into_iter()
            .flatten()
            // Don't forget to include any queues that were saved from a _previous_ save, but were never restored
            // because they didn't see any IO.
            .chain(
                self.proto_io
                    .drain()
                    .map(|(_cpu, proto_queue)| proto_queue.save_state),
            )
            .collect();

        match admin {
            None => tracing::warn!(pci_id = ?self.device.id(), "no admin queue saved"),
            Some(ref admin_state) => tracing::info!(
                pci_id = ?self.device.id(),
                id = admin_state.qid,
                pending_commands_count = admin_state.handler_data.pending_cmds.commands.len(),
                "saved admin queue",
            ),
        }

        match io.is_empty() {
            true => tracing::warn!(pci_id = ?self.device.id(), "no io queues saved"),
            false => tracing::info!(
                pci_id = ?self.device.id(),
                state = io
                    .iter()
                    .map(|io_state| format!(
                        "{{qid={}, pending_commands_count={}}}",
                        io_state.queue_data.qid,
                        io_state.queue_data.handler_data.pending_cmds.commands.len()
                    ))
                    .collect::<Vec<_>>()
                    .join(", "),
                "saved io queues",
            ),
        }

        Ok(NvmeDriverWorkerSavedState {
            admin,
            io,
            qsize: worker_state.qsize,
            max_io_queues: worker_state.max_io_queues,
        })
    }
}

impl<D: DeviceBacking> InspectTask<WorkerState> for DriverWorkerTask<D> {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&WorkerState>) {
        req.respond().merge(self).merge(state);
    }
}

/// Save/restore data structures exposed by the NVMe driver.
#[expect(missing_docs)]
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
        #[mesh(4)]
        pub aer_handler: Option<AerHandlerSavedState>,
    }

    /// Snapshot of submission queue metadata captured during save.
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

    /// Snapshot of completion queue metadata captured during save.
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

    /// Pending command entry captured from a queue handler.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct PendingCommandSavedState {
        #[mesh(1, encoding = "mesh::payload::encoding::ZeroCopyEncoding")]
        pub command: spec::Command,
    }

    /// Collection of pending commands indexed by CID.
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

    /// Saved Async Event Request handler metadata.
    #[derive(Clone, Debug, Protobuf)]
    #[mesh(package = "nvme_driver")]
    pub struct AerHandlerSavedState {
        #[mesh(1)]
        pub last_aen: Option<u32>,
        #[mesh(2)]
        pub await_aen_cid: Option<u16>,
    }
}
