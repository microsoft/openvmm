// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::queue::QueueCoreCompleteWork;
use crate::queue::QueueCoreGetWork;
use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::QueueWork;
use crate::queue::VirtioQueuePayload;
use crate::queue::new_queue;
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

#[async_trait]
pub trait VirtioQueueWorkerContext {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool;
}

#[derive(Debug)]
pub struct VirtioQueueUsedHandler {
    core: QueueCoreCompleteWork,
    outstanding_desc_count: Arc<Mutex<(u16, event_listener::Event)>>,
    notify_guest: Interrupt,
}

impl VirtioQueueUsedHandler {
    fn new(core: QueueCoreCompleteWork, notify_guest: Interrupt) -> Self {
        Self {
            core,
            outstanding_desc_count: Arc::new(Mutex::new((0, event_listener::Event::new()))),
            notify_guest,
        }
    }

    pub fn add_outstanding_descriptor(&self) {
        let (count, _) = &mut *self.outstanding_desc_count.lock();
        *count += 1;
    }

    pub fn complete_descriptor(&mut self, work: &QueueWork, bytes_written: u32) {
        match self.core.complete_descriptor(work, bytes_written) {
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
    used_queue_handler: Arc<Mutex<VirtioQueueUsedHandler>>,
    work: QueueWork,
    pub payload: Vec<VirtioQueuePayload>,
    completed: bool,
}

impl VirtioQueueCallbackWork {
    pub fn new(
        mut work: QueueWork,
        used_queue_handler: &Arc<Mutex<VirtioQueueUsedHandler>>,
    ) -> Self {
        let used_queue_handler = used_queue_handler.clone();
        let payload = std::mem::take(&mut work.payload);
        used_queue_handler.lock().add_outstanding_descriptor();
        Self {
            work,
            payload,
            used_queue_handler,
            completed: false,
        }
    }

    pub fn complete(&mut self, bytes_written: u32) {
        assert!(!self.completed);
        self.used_queue_handler
            .lock()
            .complete_descriptor(&self.work, bytes_written);
        self.completed = true;
    }

    pub fn descriptor_index(&self) -> u16 {
        self.work.descriptor_index()
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
    core: QueueCoreGetWork,
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
        let (get_work, complete_work) = new_queue(features, mem, params)?;
        let used_handler = Arc::new(Mutex::new(VirtioQueueUsedHandler::new(
            complete_work,
            notify,
        )));
        Ok(Self {
            core: get_work,
            used_handler,
            queue_event,
        })
    }

    fn poll_next_buffer(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<VirtioQueueCallbackWork>, QueueError>> {
        let work = loop {
            if let Some(work) = self.core.try_next_work()? {
                break work;
            };
            ready!(self.queue_event.wait().poll_unpin(cx)).expect("waits on Event cannot fail");
        };
        Poll::Ready(Ok(Some(VirtioQueueCallbackWork::new(
            work,
            &self.used_handler,
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

pub trait VirtioDevice: Send {
    fn traits(&self) -> DeviceTraits;
    fn read_registers_u32(&self, offset: u16) -> u32;
    fn write_registers_u32(&mut self, offset: u16, val: u32);
    fn enable(&mut self, resources: Resources);
    /// Poll the device to complete a disable/reset operation.
    ///
    /// This is called when the guest writes status=0 (device reset). The device
    /// should stop workers and drain any in-flight IO. Returns `Poll::Ready(())`
    /// when the disable is complete, or `Poll::Pending` if more work is needed.
    ///
    /// Devices that don't need async cleanup can return `Poll::Ready(())`
    /// immediately.
    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()>;
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
