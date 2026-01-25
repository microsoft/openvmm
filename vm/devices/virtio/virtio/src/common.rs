// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::queue::QueueCore;
use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::VirtioQueuePayload;
use crate::spec::VirtioDeviceFeatures;
use async_trait::async_trait;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use guestmem::MappedMemoryRegion;
use pal_async::DefaultPool;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use std::io::Error;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use task_control::AsyncRun;
use task_control::StopTask;
use task_control::TaskControl;
use thiserror::Error;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

#[async_trait]
pub trait VirtioQueueWorkerContext {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool;
}

#[derive(Debug)]
pub struct VirtioQueueUsedHandler {
    core: QueueCore,
    last_used_index: u16,
    outstanding_desc_count: Arc<Mutex<(u16, event_listener::Event)>>,
    notify_guest: Interrupt,
}

impl VirtioQueueUsedHandler {
    fn new(core: QueueCore, notify_guest: Interrupt) -> Self {
        Self {
            core,
            last_used_index: 0,
            outstanding_desc_count: Arc::new(Mutex::new((0, event_listener::Event::new()))),
            notify_guest,
        }
    }

    pub fn add_outstanding_descriptor(&self) {
        let (count, _) = &mut *self.outstanding_desc_count.lock();
        *count += 1;
    }

    pub fn await_outstanding_descriptors(&self) -> event_listener::EventListener {
        let (count, event) = &*self.outstanding_desc_count.lock();
        let listener = event.listen();
        if *count == 0 {
            event.notify(usize::MAX);
        }
        listener
    }

    pub fn complete_descriptor(&mut self, descriptor_index: u16, bytes_written: u32) {
        match self.core.complete_descriptor(
            &mut self.last_used_index,
            descriptor_index,
            bytes_written,
        ) {
            Ok(true) => {
                self.notify_guest.deliver();
            }
            Ok(false) => {}
            Err(err) => {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to complete descriptor"
                );
            }
        }
        {
            let (count, event) = &mut *self.outstanding_desc_count.lock();
            *count -= 1;
            if *count == 0 {
                event.notify(usize::MAX);
            }
        }
    }
}

pub struct VirtioQueueCallbackWork {
    pub payload: Vec<VirtioQueuePayload>,
    used_queue_handler: Arc<Mutex<VirtioQueueUsedHandler>>,
    descriptor_index: u16,
    completed: bool,
}

impl VirtioQueueCallbackWork {
    pub fn new(
        payload: Vec<VirtioQueuePayload>,
        used_queue_handler: &Arc<Mutex<VirtioQueueUsedHandler>>,
        descriptor_index: u16,
    ) -> Self {
        let used_queue_handler = used_queue_handler.clone();
        used_queue_handler.lock().add_outstanding_descriptor();
        Self {
            payload,
            used_queue_handler,
            descriptor_index,
            completed: false,
        }
    }

    pub fn complete(&mut self, bytes_written: u32) {
        assert!(!self.completed);
        self.used_queue_handler
            .lock()
            .complete_descriptor(self.descriptor_index, bytes_written);
        self.completed = true;
    }

    pub fn descriptor_index(&self) -> u16 {
        self.descriptor_index
    }

    // Determine the total size of all readable or all writeable payload buffers.
    pub fn get_payload_length(&self, writeable: bool) -> u64 {
        self.payload
            .iter()
            .filter(|x| x.writeable == writeable)
            .fold(0, |acc, x| acc + x.length as u64)
    }

    // Read all payload into a buffer.
    pub fn read(&self, mem: &GuestMemory, target: &mut [u8]) -> Result<usize, GuestMemoryError> {
        let mut remaining = target;
        let mut read_bytes: usize = 0;
        for payload in &self.payload {
            if payload.writeable {
                continue;
            }

            let size = std::cmp::min(payload.length as usize, remaining.len());
            let (current, next) = remaining.split_at_mut(size);
            mem.read_at(payload.address, current)?;
            read_bytes += size;
            if next.is_empty() {
                break;
            }

            remaining = next;
        }

        Ok(read_bytes)
    }

    // Write the specified buffer to the payload buffers.
    pub fn write_at_offset(
        &self,
        offset: u64,
        mem: &GuestMemory,
        source: &[u8],
    ) -> Result<(), VirtioWriteError> {
        let mut skip_bytes = offset;
        let mut remaining = source;
        for payload in &self.payload {
            if !payload.writeable {
                continue;
            }

            let payload_length = payload.length as u64;
            if skip_bytes >= payload_length {
                skip_bytes -= payload_length;
                continue;
            }

            let size = std::cmp::min(
                payload_length as usize - skip_bytes as usize,
                remaining.len(),
            );
            let (current, next) = remaining.split_at(size);
            mem.write_at(payload.address + skip_bytes, current)?;
            remaining = next;
            if remaining.is_empty() {
                break;
            }
            skip_bytes = 0;
        }

        if !remaining.is_empty() {
            return Err(VirtioWriteError::NotAllWritten(source.len()));
        }

        Ok(())
    }

    pub fn write(&self, mem: &GuestMemory, source: &[u8]) -> Result<(), VirtioWriteError> {
        self.write_at_offset(0, mem, source)
    }
}

#[derive(Debug, Error)]
pub enum VirtioWriteError {
    #[error(transparent)]
    Memory(#[from] GuestMemoryError),
    #[error("{0:#x} bytes not written")]
    NotAllWritten(usize),
}

impl Drop for VirtioQueueCallbackWork {
    fn drop(&mut self) {
        if !self.completed {
            self.complete(0);
        }
    }
}

#[derive(Debug)]
pub struct VirtioQueue {
    core: QueueCore,
    last_avail_index: u16,
    used_handler: Arc<Mutex<VirtioQueueUsedHandler>>,
    queue_event: PolledWait<Event>,
}

impl VirtioQueue {
    pub fn new(
        features: VirtioDeviceFeatures,
        params: QueueParams,
        mem: GuestMemory,
        notify: Interrupt,
        queue_event: PolledWait<Event>,
    ) -> Result<Self, QueueError> {
        let core = QueueCore::new(features, mem, params)?;
        let used_handler = Arc::new(Mutex::new(VirtioQueueUsedHandler::new(
            core.clone(),
            notify,
        )));
        Ok(Self {
            core,
            last_avail_index: 0,
            used_handler,
            queue_event,
        })
    }

    async fn wait_for_outstanding_descriptors(&self) {
        let wait_for_descriptors = self.used_handler.lock().await_outstanding_descriptors();
        wait_for_descriptors.await;
    }

    fn poll_next_buffer(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<VirtioQueueCallbackWork>, QueueError>> {
        let descriptor_index = loop {
            if let Some(descriptor_index) = self.core.descriptor_index(self.last_avail_index)? {
                break descriptor_index;
            };
            ready!(self.queue_event.wait().poll_unpin(cx)).expect("waits on Event cannot fail");
        };
        let payload = self
            .core
            .reader(descriptor_index)
            .collect::<Result<Vec<_>, _>>()?;

        self.last_avail_index = self.last_avail_index.wrapping_add(1);
        Poll::Ready(Ok(Some(VirtioQueueCallbackWork::new(
            payload,
            &self.used_handler,
            descriptor_index,
        ))))
    }
}

impl Drop for VirtioQueue {
    fn drop(&mut self) {
        if Arc::get_mut(&mut self.used_handler).is_none() {
            tracing::error!("Virtio queue dropped with outstanding work pending")
        }
    }
}

impl Stream for VirtioQueue {
    type Item = Result<VirtioQueueCallbackWork, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(r) = ready!(self.get_mut().poll_next_buffer(cx)).transpose() else {
            return Poll::Ready(None);
        };

        Poll::Ready(Some(r.map_err(Error::other)))
    }
}

enum VirtioQueueStateInner {
    Initializing {
        mem: GuestMemory,
        features: VirtioDeviceFeatures,
        params: QueueParams,
        event: Event,
        notify: Interrupt,
        exit_event: event_listener::EventListener,
    },
    InitializationInProgress,
    Running {
        queue: VirtioQueue,
        exit_event: event_listener::EventListener,
    },
}

pub struct VirtioQueueState {
    inner: VirtioQueueStateInner,
}

pub struct VirtioQueueWorker {
    driver: Box<dyn Driver>,
    context: Box<dyn VirtioQueueWorkerContext + Send>,
}

impl VirtioQueueWorker {
    pub fn new(driver: impl Driver, context: Box<dyn VirtioQueueWorkerContext + Send>) -> Self {
        Self {
            driver: Box::new(driver),
            context,
        }
    }

    pub fn into_running_task(
        self,
        name: impl Into<String>,
        mem: GuestMemory,
        features: VirtioDeviceFeatures,
        queue_resources: QueueResources,
        exit_event: event_listener::EventListener,
    ) -> TaskControl<VirtioQueueWorker, VirtioQueueState> {
        let name = name.into();
        let (_, driver) = DefaultPool::spawn_on_thread(&name);

        let mut task = TaskControl::new(self);
        task.insert(
            driver,
            name,
            VirtioQueueState {
                inner: VirtioQueueStateInner::Initializing {
                    mem,
                    features,
                    params: queue_resources.params,
                    event: queue_resources.event,
                    notify: queue_resources.notify,
                    exit_event,
                },
            },
        );
        task.start();
        task
    }

    async fn run_queue(&mut self, state: &mut VirtioQueueState) -> bool {
        match &mut state.inner {
            VirtioQueueStateInner::InitializationInProgress => unreachable!(),
            VirtioQueueStateInner::Initializing { .. } => {
                let VirtioQueueStateInner::Initializing {
                    mem,
                    features,
                    params,
                    event,
                    notify,
                    exit_event,
                } = std::mem::replace(
                    &mut state.inner,
                    VirtioQueueStateInner::InitializationInProgress,
                )
                else {
                    unreachable!()
                };
                let queue_event = PolledWait::new(&self.driver, event).unwrap();
                let queue = VirtioQueue::new(features, params, mem, notify, queue_event);
                if let Err(err) = queue {
                    tracing::error!(
                        err = &err as &dyn std::error::Error,
                        "Failed to start queue"
                    );
                    false
                } else {
                    state.inner = VirtioQueueStateInner::Running {
                        queue: queue.unwrap(),
                        exit_event,
                    };
                    true
                }
            }
            VirtioQueueStateInner::Running { queue, exit_event } => {
                let mut exit = exit_event.fuse();
                let mut queue_ready = queue.next().fuse();
                let work = futures::select_biased! {
                    _ = exit => return false,
                    work = queue_ready => work.expect("queue will never complete").map_err(anyhow::Error::from),
                };
                self.context.process_work(work).await
            }
        }
    }
}

impl AsyncRun<VirtioQueueState> for VirtioQueueWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut VirtioQueueState,
    ) -> Result<(), task_control::Cancelled> {
        while stop.until_stopped(self.run_queue(state)).await? {}
        Ok(())
    }
}

pub struct VirtioRunningState {
    pub features: VirtioDeviceFeatures,
    pub enabled_queues: Vec<bool>,
}

pub enum VirtioState {
    Unknown,
    Running(VirtioRunningState),
    Stopped,
}

pub(crate) struct VirtioDoorbells {
    registration: Option<Arc<dyn DoorbellRegistration>>,
    doorbells: Vec<Box<dyn Send + Sync>>,
}

impl VirtioDoorbells {
    pub fn new(registration: Option<Arc<dyn DoorbellRegistration>>) -> Self {
        Self {
            registration,
            doorbells: Vec::new(),
        }
    }

    pub fn add(&mut self, address: u64, value: Option<u64>, length: Option<u32>, event: &Event) {
        if let Some(registration) = &mut self.registration {
            let doorbell = registration.register_doorbell(address, value, length, event);
            if let Ok(doorbell) = doorbell {
                self.doorbells.push(doorbell);
            }
        }
    }

    pub fn clear(&mut self) {
        self.doorbells.clear();
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct DeviceTraitsSharedMemory {
    pub id: u8,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceTraits {
    pub device_id: u16,
    pub device_features: VirtioDeviceFeatures,
    pub max_queues: u16,
    pub device_register_length: u32,
    pub shared_memory: DeviceTraitsSharedMemory,
}

pub trait LegacyVirtioDevice: Send {
    fn traits(&self) -> DeviceTraits;
    fn read_registers_u32(&self, offset: u16) -> u32;
    fn write_registers_u32(&mut self, offset: u16, val: u32);
    fn get_work_callback(&mut self, index: u16) -> Box<dyn VirtioQueueWorkerContext + Send>;
    fn state_change(&mut self, state: &VirtioState);
}

pub trait VirtioDevice: Send {
    fn traits(&self) -> DeviceTraits;
    fn read_registers_u32(&self, offset: u16) -> u32;
    fn write_registers_u32(&mut self, offset: u16, val: u32);
    fn enable(&mut self, resources: Resources);
    fn disable(&mut self);
}

pub struct QueueResources {
    pub params: QueueParams,
    pub notify: Interrupt,
    pub event: Event,
}

pub struct Resources {
    pub features: VirtioDeviceFeatures,
    pub queues: Vec<QueueResources>,
    pub shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    pub shared_memory_size: u64,
}

/// Wraps an object implementing [`LegacyVirtioDevice`] and implements [`VirtioDevice`].
pub struct LegacyWrapper<T: LegacyVirtioDevice> {
    device: T,
    driver: VmTaskDriver,
    mem: GuestMemory,
    workers: Vec<TaskControl<VirtioQueueWorker, VirtioQueueState>>,
    exit_event: event_listener::Event,
}

impl<T: LegacyVirtioDevice> LegacyWrapper<T> {
    pub fn new(driver_source: &VmTaskDriverSource, device: T, mem: &GuestMemory) -> Self {
        Self {
            device,
            driver: driver_source.simple(),
            mem: mem.clone(),
            workers: Vec::new(),
            exit_event: event_listener::Event::new(),
        }
    }
}

impl<T: LegacyVirtioDevice> VirtioDevice for LegacyWrapper<T> {
    fn traits(&self) -> DeviceTraits {
        self.device.traits()
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
        self.device.read_registers_u32(offset)
    }

    fn write_registers_u32(&mut self, offset: u16, val: u32) {
        self.device.write_registers_u32(offset, val)
    }

    fn enable(&mut self, resources: Resources) {
        let running_state = VirtioRunningState {
            features: resources.features.clone(),
            enabled_queues: resources
                .queues
                .iter()
                .map(|QueueResources { params, .. }| params.enable)
                .collect(),
        };

        self.device
            .state_change(&VirtioState::Running(running_state));
        self.workers = resources
            .queues
            .into_iter()
            .enumerate()
            .filter_map(|(i, queue_resources)| {
                if !queue_resources.params.enable {
                    return None;
                }
                let worker = VirtioQueueWorker::new(
                    self.driver.clone(),
                    self.device.get_work_callback(i as u16),
                );
                Some(worker.into_running_task(
                    "virtio-queue".to_string(),
                    self.mem.clone(),
                    resources.features.clone(),
                    queue_resources,
                    self.exit_event.listen(),
                ))
            })
            .collect();
    }

    fn disable(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        self.exit_event.notify(usize::MAX);
        self.device.state_change(&VirtioState::Stopped);
        let mut workers = self.workers.drain(..).collect::<Vec<_>>();
        self.driver
            .spawn("shutdown-legacy-virtio-queues".to_owned(), async move {
                futures::future::join_all(workers.iter_mut().map(async |worker| {
                    worker.stop().await;
                    if let Some(VirtioQueueStateInner::Running { queue, .. }) =
                        worker.state_mut().map(|s| &s.inner)
                    {
                        queue.wait_for_outstanding_descriptors().await;
                    }
                }))
                .await;
            })
            .detach();
    }
}

impl<T: LegacyVirtioDevice> Drop for LegacyWrapper<T> {
    fn drop(&mut self) {
        self.disable();
    }
}
