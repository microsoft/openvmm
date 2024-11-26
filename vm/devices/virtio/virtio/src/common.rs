// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::queue::QueueCore;
use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::VirtioQueuePayload;
use async_trait::async_trait;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use guestmem::MappedMemoryRegion;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::wait::PolledWait;
use pal_async::DefaultPool;
use pal_event::Event;
use parking_lot::Mutex;
use std::io::Error;
use std::pin::Pin;
use std::sync::Arc;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
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
        features: u64,
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

        Poll::Ready(Some(
            r.map_err(|err| Error::new(std::io::ErrorKind::Other, err)),
        ))
    }
}

enum VirtioQueueStateInner {
    Initializing {
        mem: GuestMemory,
        features: u64,
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
        features: u64,
        queue_resources: QueueResources,
        exit_event: event_listener::EventListener,
    ) -> TaskControl<VirtioQueueWorker, VirtioQueueState> {
        let pool = DefaultPool::new();
        let driver = pool.driver();
        let name: String = name.into();
        std::thread::Builder::new()
            .name(name.clone())
            .spawn(|| {
                pool.run();
            })
            .unwrap();

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
    pub features: u64,
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

#[derive(Copy, Clone, Debug, Default)]
pub struct DeviceTraits {
    pub device_id: u16,
    pub device_features: u64,
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
    pub features: u64,
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
            features: resources.features,
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
                    resources.features,
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
                futures::future::join_all(workers.iter_mut().map(|worker| async {
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

// UNSAFETY: test code implements a custom `GuestMemory` backing, which requires
// unsafe.
#[expect(unsafe_code)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::pci::*;
    use crate::spec::queue::*;
    use crate::spec::*;
    use crate::transport::VirtioMmioDevice;
    use crate::transport::VirtioPciDevice;
    use crate::PciInterruptModel;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pci::PciConfigSpace;
    use futures::StreamExt;
    use guestmem::GuestMemoryAccess;
    use guestmem::GuestMemoryBackingError;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use pal_async::DefaultDriver;
    use pci_core::msi::MsiInterruptSet;
    use pci_core::spec::caps::CapabilityId;
    use pci_core::spec::cfg_space;
    use pci_core::test_helpers::TestPciInterruptController;
    use std::collections::BTreeMap;
    use std::future::poll_fn;
    use std::io;
    use std::ptr::NonNull;
    use std::time::Duration;
    use test_with_tracing::test;
    use vmcore::line_interrupt::test_helpers::TestLineInterruptTarget;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    async fn must_recv_in_timeout<T: 'static + Send>(
        recv: &mut mesh::MpscReceiver<T>,
        timeout: Duration,
    ) -> T {
        mesh::CancelContext::new()
            .with_timeout(timeout)
            .until_cancelled(recv.next())
            .await
            .unwrap()
            .unwrap()
    }

    #[derive(Default)]
    struct VirtioTestMemoryAccess {
        memory_map: Mutex<MemoryMap>,
    }

    #[derive(Default)]
    struct MemoryMap {
        map: BTreeMap<u64, (bool, Vec<u8>)>,
    }

    impl MemoryMap {
        fn get(&mut self, address: u64, len: usize) -> Option<(bool, &mut [u8])> {
            let (&base, &mut (writable, ref mut data)) = self.map.range_mut(..=address).last()?;
            let data = data
                .get_mut(usize::try_from(address - base).ok()?..)?
                .get_mut(..len)?;

            Some((writable, data))
        }

        fn insert(&mut self, address: u64, data: &[u8], writable: bool) {
            if let Some((is_writable, v)) = self.get(address, data.len()) {
                assert_eq!(writable, is_writable);
                v.copy_from_slice(data);
                return;
            }

            let end = address + data.len() as u64;
            let mut data = data.to_vec();
            if let Some((&next, &(next_writable, ref next_data))) = self.map.range(address..).next()
            {
                if end > next {
                    let next_end = next + next_data.len() as u64;
                    panic!("overlapping memory map: {address:#x}..{end:#x} > {next:#x}..={next_end:#x}");
                }
                if end == next && next_writable == writable {
                    data.extend(next_data.as_slice());
                    self.map.remove(&next).unwrap();
                }
            }

            if let Some((&prev, &mut (prev_writable, ref mut prev_data))) =
                self.map.range_mut(..address).last()
            {
                let prev_end = prev + prev_data.len() as u64;
                if prev_end > address {
                    panic!("overlapping memory map: {prev:#x}..{prev_end:#x} > {address:#x}..={end:#x}");
                }
                if prev_end == address && prev_writable == writable {
                    prev_data.extend_from_slice(&data);
                    return;
                }
            }

            self.map.insert(address, (writable, data));
        }
    }

    impl VirtioTestMemoryAccess {
        fn new() -> Arc<Self> {
            Default::default()
        }

        fn modify_memory_map(&self, address: u64, data: &[u8], writeable: bool) {
            self.memory_map.lock().insert(address, data, writeable);
        }

        fn memory_map_get_u16(&self, address: u64) -> u16 {
            let mut map = self.memory_map.lock();
            let (_, data) = map.get(address, 2).unwrap();
            u16::from_le_bytes(data.try_into().unwrap())
        }

        fn memory_map_get_u32(&self, address: u64) -> u32 {
            let mut map = self.memory_map.lock();
            let (_, data) = map.get(address, 4).unwrap();
            u32::from_le_bytes(data.try_into().unwrap())
        }
    }

    // SAFETY: test code
    unsafe impl GuestMemoryAccess for VirtioTestMemoryAccess {
        fn mapping(&self) -> Option<NonNull<u8>> {
            None
        }

        fn max_address(&self) -> u64 {
            // No real bound, so use the max physical address width on
            // AMD64/ARM64.
            1 << 52
        }

        unsafe fn read_fallback(
            &self,
            address: u64,
            dest: *mut u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            match self.memory_map.lock().get(address, len) {
                Some((_, value)) => {
                    // SAFETY: guaranteed by caller
                    unsafe {
                        std::ptr::copy(value.as_ptr(), dest, len);
                    }
                }
                None => panic!("Unexpected read request at address {:x}", address),
            }
            Ok(())
        }

        unsafe fn write_fallback(
            &self,
            address: u64,
            src: *const u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            match self.memory_map.lock().get(address, len) {
                Some((true, value)) => {
                    // SAFETY: guaranteed by caller
                    unsafe {
                        std::ptr::copy(src, value.as_mut_ptr(), len);
                    }
                }
                _ => panic!("Unexpected write request at address {:x}", address),
            }
            Ok(())
        }

        fn fill_fallback(
            &self,
            address: u64,
            val: u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            match self.memory_map.lock().get(address, len) {
                Some((true, value)) => value.fill(val),
                _ => panic!("Unexpected write request at address {:x}", address),
            };
            Ok(())
        }
    }

    struct DoorbellEntry;
    impl Drop for DoorbellEntry {
        fn drop(&mut self) {}
    }

    impl DoorbellRegistration for VirtioTestMemoryAccess {
        fn register_doorbell(
            &self,
            _: u64,
            _: Option<u64>,
            _: Option<u32>,
            _: &Event,
        ) -> io::Result<Box<dyn Send + Sync>> {
            Ok(Box::new(DoorbellEntry))
        }
    }

    type VirtioTestWorkCallback =
        Box<dyn Fn(anyhow::Result<VirtioQueueCallbackWork>) -> bool + Sync + Send>;
    struct CreateDirectQueueParams {
        process_work: VirtioTestWorkCallback,
        notify: Interrupt,
        event: Event,
    }

    struct VirtioTestGuest {
        test_mem: Arc<VirtioTestMemoryAccess>,
        driver: DefaultDriver,
        num_queues: u16,
        queue_size: u16,
        use_ring_event_index: bool,
        last_avail_index: Vec<u16>,
        last_used_index: Vec<u16>,
        avail_descriptors: Vec<Vec<bool>>,
        exit_event: event_listener::Event,
    }

    impl VirtioTestGuest {
        fn new(
            driver: &DefaultDriver,
            test_mem: &Arc<VirtioTestMemoryAccess>,
            num_queues: u16,
            queue_size: u16,
            use_ring_event_index: bool,
        ) -> Self {
            let last_avail_index: Vec<u16> = vec![0; num_queues as usize];
            let last_used_index: Vec<u16> = vec![0; num_queues as usize];
            let avail_descriptors: Vec<Vec<bool>> =
                vec![vec![true; queue_size as usize]; num_queues as usize];
            let test_guest = Self {
                test_mem: test_mem.clone(),
                driver: driver.clone(),
                num_queues,
                queue_size,
                use_ring_event_index,
                last_avail_index,
                last_used_index,
                avail_descriptors,
                exit_event: event_listener::Event::new(),
            };
            for i in 0..num_queues {
                test_guest.add_queue_memory(i);
            }
            test_guest
        }

        fn mem(&self) -> GuestMemory {
            GuestMemory::new("test", self.test_mem.clone())
        }

        fn create_direct_queues<F>(
            &self,
            f: F,
        ) -> Vec<TaskControl<VirtioQueueWorker, VirtioQueueState>>
        where
            F: Fn(u16) -> CreateDirectQueueParams,
        {
            (0..self.num_queues)
                .map(|i| {
                    let params = f(i);
                    let worker = VirtioQueueWorker::new(
                        self.driver.clone(),
                        Box::new(VirtioTestWork {
                            callback: params.process_work,
                        }),
                    );
                    worker.into_running_task(
                        "virtio-test-queue".to_string(),
                        self.mem(),
                        self.queue_features(),
                        QueueResources {
                            params: self.queue_params(i),
                            notify: params.notify,
                            event: params.event,
                        },
                        self.exit_event.listen(),
                    )
                })
                .collect::<Vec<_>>()
        }

        fn queue_features(&self) -> u64 {
            if self.use_ring_event_index {
                VIRTIO_F_RING_EVENT_IDX as u64
            } else {
                0
            }
        }

        fn queue_params(&self, i: u16) -> QueueParams {
            QueueParams {
                size: self.queue_size,
                enable: true,
                desc_addr: self.get_queue_descriptor_base_address(i),
                avail_addr: self.get_queue_available_base_address(i),
                used_addr: self.get_queue_used_base_address(i),
            }
        }

        fn get_queue_base_address(&self, index: u16) -> u64 {
            0x10000000 * index as u64
        }

        fn get_queue_descriptor_base_address(&self, index: u16) -> u64 {
            self.get_queue_base_address(index) + 0x1000
        }

        fn get_queue_available_base_address(&self, index: u16) -> u64 {
            self.get_queue_base_address(index) + 0x2000
        }

        fn get_queue_used_base_address(&self, index: u16) -> u64 {
            self.get_queue_base_address(index) + 0x3000
        }

        fn get_queue_descriptor_backing_memory_address(&self, index: u16) -> u64 {
            self.get_queue_base_address(index) + 0x4000
        }

        fn setup_chipset_device(&self, dev: &mut VirtioMmioDevice, driver_features: u64) {
            dev.write_u32(112, VIRTIO_ACKNOWLEDGE);
            dev.write_u32(112, VIRTIO_DRIVER);
            dev.write_u32(36, 0);
            dev.write_u32(32, driver_features as u32);
            dev.write_u32(36, 1);
            dev.write_u32(32, (driver_features >> 32) as u32);
            dev.write_u32(112, VIRTIO_FEATURES_OK);
            for i in 0..self.num_queues {
                let queue_index = i;
                dev.write_u32(48, i as u32);
                dev.write_u32(56, self.queue_size as u32);
                let desc_addr = self.get_queue_descriptor_base_address(queue_index);
                dev.write_u32(128, desc_addr as u32);
                dev.write_u32(132, (desc_addr >> 32) as u32);
                let avail_addr = self.get_queue_available_base_address(queue_index);
                dev.write_u32(144, avail_addr as u32);
                dev.write_u32(148, (avail_addr >> 32) as u32);
                let used_addr = self.get_queue_used_base_address(queue_index);
                dev.write_u32(160, used_addr as u32);
                dev.write_u32(164, (used_addr >> 32) as u32);
                // enable the queue
                dev.write_u32(68, 1);
            }
            dev.write_u32(112, VIRTIO_DRIVER_OK);
            assert_eq!(dev.read_u32(0xfc), 2);
        }

        fn setup_pci_device(&self, dev: &mut VirtioPciTestDevice, driver_features: u64) {
            let bar_address1: u64 = 0x10000000000;
            dev.pci_device
                .pci_cfg_write(0x14, (bar_address1 >> 32) as u32)
                .unwrap();
            dev.pci_device
                .pci_cfg_write(0x10, bar_address1 as u32)
                .unwrap();

            let bar_address2: u64 = 0x20000000000;
            dev.pci_device
                .pci_cfg_write(0x1c, (bar_address2 >> 32) as u32)
                .unwrap();
            dev.pci_device
                .pci_cfg_write(0x18, bar_address2 as u32)
                .unwrap();

            dev.pci_device
                .pci_cfg_write(0x4, cfg_space::Command::MMIO_ENABLED.bits() as u32)
                .unwrap();

            let mut device_status = VIRTIO_ACKNOWLEDGE as u8;
            dev.pci_device
                .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
                .unwrap();
            device_status = VIRTIO_DRIVER as u8;
            dev.pci_device
                .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
                .unwrap();
            dev.write_u32(bar_address1 + 8, 0);
            dev.write_u32(bar_address1 + 12, driver_features as u32);
            dev.write_u32(bar_address1 + 8, 1);
            dev.write_u32(bar_address1 + 12, (driver_features >> 32) as u32);
            device_status = VIRTIO_FEATURES_OK as u8;
            dev.pci_device
                .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
                .unwrap();
            // setup config interrupt
            dev.pci_device
                .mmio_write(bar_address2, &0_u64.to_le_bytes())
                .unwrap(); // vector
            dev.pci_device
                .mmio_write(bar_address2 + 8, &0_u32.to_le_bytes())
                .unwrap(); // data
            dev.pci_device
                .mmio_write(bar_address2 + 12, &0_u32.to_le_bytes())
                .unwrap();
            for i in 0..self.num_queues {
                let queue_index = i;
                dev.pci_device
                    .mmio_write(bar_address1 + 22, &queue_index.to_le_bytes())
                    .unwrap();
                dev.pci_device
                    .mmio_write(bar_address1 + 24, &self.queue_size.to_le_bytes())
                    .unwrap();
                // setup MSI information for the queue
                let msix_vector = queue_index + 1;
                let address = bar_address2 + 0x10 * msix_vector as u64;
                dev.pci_device
                    .mmio_write(address, &(msix_vector as u64).to_le_bytes())
                    .unwrap();
                let address = bar_address2 + 0x10 * msix_vector as u64 + 8;
                dev.pci_device
                    .mmio_write(address, &0_u32.to_le_bytes())
                    .unwrap();
                let address = bar_address2 + 0x10 * msix_vector as u64 + 12;
                dev.pci_device
                    .mmio_write(address, &0_u32.to_le_bytes())
                    .unwrap();
                dev.pci_device
                    .mmio_write(bar_address1 + 26, &msix_vector.to_le_bytes())
                    .unwrap();
                // setup queue addresses
                let desc_addr = self.get_queue_descriptor_base_address(queue_index);
                dev.write_u32(bar_address1 + 32, desc_addr as u32);
                dev.write_u32(bar_address1 + 36, (desc_addr >> 32) as u32);
                let avail_addr = self.get_queue_available_base_address(queue_index);
                dev.write_u32(bar_address1 + 40, avail_addr as u32);
                dev.write_u32(bar_address1 + 44, (avail_addr >> 32) as u32);
                let used_addr = self.get_queue_used_base_address(queue_index);
                dev.write_u32(bar_address1 + 48, used_addr as u32);
                dev.write_u32(bar_address1 + 52, (used_addr >> 32) as u32);
                // enable the queue
                let enabled: u16 = 1;
                dev.pci_device
                    .mmio_write(bar_address1 + 28, &enabled.to_le_bytes())
                    .unwrap();
            }
            // enable all device MSI interrupts
            dev.pci_device.pci_cfg_write(0x40, 0x80000000).unwrap();
            // run device
            device_status = VIRTIO_DRIVER_OK as u8;
            dev.pci_device
                .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
                .unwrap();
            let mut config_generation: [u8; 1] = [0];
            dev.pci_device
                .mmio_read(bar_address1 + 21, &mut config_generation)
                .unwrap();
            assert_eq!(config_generation[0], 2);
        }

        fn get_queue_descriptor(&self, queue_index: u16, descriptor_index: u16) -> u64 {
            self.get_queue_descriptor_base_address(queue_index) + 0x10 * descriptor_index as u64
        }

        fn add_queue_memory(&self, queue_index: u16) {
            // descriptors
            for i in 0..self.queue_size {
                let base = self.get_queue_descriptor(queue_index, i);
                // physical address
                self.test_mem.modify_memory_map(
                    base,
                    &(self.get_queue_descriptor_backing_memory_address(queue_index)
                        + 0x1000 * i as u64)
                        .to_le_bytes(),
                    false,
                );
                // length
                self.test_mem
                    .modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), false);
                // flags
                self.test_mem
                    .modify_memory_map(base + 12, &0u16.to_le_bytes(), false);
                // next index
                self.test_mem
                    .modify_memory_map(base + 14, &0u16.to_le_bytes(), false);
            }

            // available queue (flags, index)
            let base = self.get_queue_available_base_address(queue_index);
            self.test_mem
                .modify_memory_map(base, &0u16.to_le_bytes(), false);
            self.test_mem
                .modify_memory_map(base + 2, &0u16.to_le_bytes(), false);
            // available queue ring buffer
            for i in 0..self.queue_size {
                let base = base + 4 + 2 * i as u64;
                self.test_mem
                    .modify_memory_map(base, &0u16.to_le_bytes(), false);
            }
            // used event
            if self.use_ring_event_index {
                self.test_mem.modify_memory_map(
                    base + 4 + 2 * self.queue_size as u64,
                    &0u16.to_le_bytes(),
                    false,
                );
            }

            // used queue (flags, index)
            let base = self.get_queue_used_base_address(queue_index);
            self.test_mem
                .modify_memory_map(base, &0u16.to_le_bytes(), true);
            self.test_mem
                .modify_memory_map(base + 2, &0u16.to_le_bytes(), true);
            for i in 0..self.queue_size {
                let base = base + 4 + 8 * i as u64;
                // index
                self.test_mem
                    .modify_memory_map(base, &0u32.to_le_bytes(), true);
                // length
                self.test_mem
                    .modify_memory_map(base + 4, &0u32.to_le_bytes(), true);
            }
            // available event
            if self.use_ring_event_index {
                self.test_mem.modify_memory_map(
                    base + 4 + 8 * self.queue_size as u64,
                    &0u16.to_le_bytes(),
                    true,
                );
            }
        }

        fn reserve_descriptor(&mut self, queue_index: u16) -> u16 {
            let avail_descriptors = &mut self.avail_descriptors[queue_index as usize];
            for (i, desc) in avail_descriptors.iter_mut().enumerate() {
                if *desc {
                    *desc = false;
                    return i as u16;
                }
            }

            panic!("No descriptors are available!");
        }

        fn free_descriptor(&mut self, queue_index: u16, desc_index: u16) {
            assert!(desc_index < self.queue_size);
            let desc_addr = self.get_queue_descriptor(queue_index, desc_index);
            let flags: DescriptorFlags = self.test_mem.memory_map_get_u16(desc_addr + 12).into();
            if flags.next() {
                let next = self.test_mem.memory_map_get_u16(desc_addr + 14);
                self.free_descriptor(queue_index, next);
            }
            let avail_descriptors = &mut self.avail_descriptors[queue_index as usize];
            assert_eq!(avail_descriptors[desc_index as usize], false);
            avail_descriptors[desc_index as usize] = true;
        }

        fn queue_available_desc(&mut self, queue_index: u16, desc_index: u16) {
            let avail_base_addr = self.get_queue_available_base_address(queue_index);
            let last_avail_index = &mut self.last_avail_index[queue_index as usize];
            let next_index = *last_avail_index % self.queue_size;
            *last_avail_index = last_avail_index.wrapping_add(1);
            self.test_mem.modify_memory_map(
                avail_base_addr + 4 + 2 * next_index as u64,
                &desc_index.to_le_bytes(),
                false,
            );
            self.test_mem.modify_memory_map(
                avail_base_addr + 2,
                &last_avail_index.to_le_bytes(),
                false,
            );
        }

        fn add_to_avail_queue(&mut self, queue_index: u16) {
            let next_descriptor = self.reserve_descriptor(queue_index);
            // flags
            self.test_mem.modify_memory_map(
                self.get_queue_descriptor(queue_index, next_descriptor) + 12,
                &0u16.to_le_bytes(),
                false,
            );
            self.queue_available_desc(queue_index, next_descriptor);
        }

        fn add_indirect_to_avail_queue(&mut self, queue_index: u16) {
            let next_descriptor = self.reserve_descriptor(queue_index);
            // flags
            self.test_mem.modify_memory_map(
                self.get_queue_descriptor(queue_index, next_descriptor) + 12,
                &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
                false,
            );
            // create another (indirect) descriptor in the buffer
            let buffer_addr = self.get_queue_descriptor_backing_memory_address(queue_index);
            // physical address
            self.test_mem.modify_memory_map(
                buffer_addr,
                &0xffffffff00000000u64.to_le_bytes(),
                false,
            );
            // length
            self.test_mem
                .modify_memory_map(buffer_addr + 8, &0x1000u32.to_le_bytes(), false);
            // flags
            self.test_mem
                .modify_memory_map(buffer_addr + 12, &0u16.to_le_bytes(), false);
            // next index
            self.test_mem
                .modify_memory_map(buffer_addr + 14, &0u16.to_le_bytes(), false);
            self.queue_available_desc(queue_index, next_descriptor);
        }

        fn add_linked_to_avail_queue(&mut self, queue_index: u16, desc_count: u16) {
            let mut descriptors = Vec::with_capacity(desc_count as usize);
            for _ in 0..desc_count {
                descriptors.push(self.reserve_descriptor(queue_index));
            }

            for i in 0..descriptors.len() {
                let base = self.get_queue_descriptor(queue_index, descriptors[i]);
                let flags = if i < descriptors.len() - 1 {
                    u16::from(DescriptorFlags::new().with_next(true))
                } else {
                    0
                };
                self.test_mem
                    .modify_memory_map(base + 12, &flags.to_le_bytes(), false);
                let next = if i < descriptors.len() - 1 {
                    descriptors[i + 1]
                } else {
                    0
                };
                self.test_mem
                    .modify_memory_map(base + 14, &next.to_le_bytes(), false);
            }
            self.queue_available_desc(queue_index, descriptors[0]);
        }

        fn add_indirect_linked_to_avail_queue(&mut self, queue_index: u16, desc_count: u16) {
            let next_descriptor = self.reserve_descriptor(queue_index);
            // flags
            self.test_mem.modify_memory_map(
                self.get_queue_descriptor(queue_index, next_descriptor) + 12,
                &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
                false,
            );
            // create indirect descriptors in the buffer
            let buffer_addr = self.get_queue_descriptor_backing_memory_address(queue_index);
            for i in 0..desc_count {
                let base = buffer_addr + 0x10 * i as u64;
                let indirect_buffer_addr = 0xffffffff00000000u64 + 0x1000 * i as u64;
                // physical address
                self.test_mem
                    .modify_memory_map(base, &indirect_buffer_addr.to_le_bytes(), false);
                // length
                self.test_mem
                    .modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), false);
                // flags
                let flags = if i < desc_count - 1 {
                    u16::from(DescriptorFlags::new().with_next(true))
                } else {
                    0
                };
                self.test_mem
                    .modify_memory_map(base + 12, &flags.to_le_bytes(), false);
                // next index
                let next = if i < desc_count - 1 { i + 1 } else { 0 };
                self.test_mem
                    .modify_memory_map(base + 14, &next.to_le_bytes(), false);
            }
            self.queue_available_desc(queue_index, next_descriptor);
        }

        fn get_next_completed(&mut self, queue_index: u16) -> Option<(u16, u32)> {
            let avail_base_addr = self.get_queue_available_base_address(queue_index);
            let used_base_addr = self.get_queue_used_base_address(queue_index);
            let cur_used_index = self.test_mem.memory_map_get_u16(used_base_addr + 2);
            let last_used_index = &mut self.last_used_index[queue_index as usize];
            if *last_used_index == cur_used_index {
                return None;
            }

            if self.use_ring_event_index {
                self.test_mem.modify_memory_map(
                    avail_base_addr + 4 + 2 * self.queue_size as u64,
                    &cur_used_index.to_le_bytes(),
                    false,
                );
            }

            let next_index = *last_used_index % self.queue_size;
            *last_used_index = last_used_index.wrapping_add(1);
            let desc_index = self
                .test_mem
                .memory_map_get_u32(used_base_addr + 4 + 8 * next_index as u64);
            let desc_index = desc_index as u16;
            let bytes_written = self
                .test_mem
                .memory_map_get_u32(used_base_addr + 8 + 8 * next_index as u64);
            self.free_descriptor(queue_index, desc_index);
            Some((desc_index, bytes_written))
        }
    }

    struct VirtioTestWork {
        callback: VirtioTestWorkCallback,
    }

    #[async_trait]
    impl VirtioQueueWorkerContext for VirtioTestWork {
        async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
            (self.callback)(work)
        }
    }
    struct VirtioPciTestDevice {
        pci_device: VirtioPciDevice,
        test_intc: Arc<TestPciInterruptController>,
    }

    type TestDeviceQueueWorkFn = Arc<dyn Fn(u16, VirtioQueueCallbackWork) + Send + Sync>;

    struct TestDevice {
        traits: DeviceTraits,
        queue_work: Option<TestDeviceQueueWorkFn>,
    }

    impl TestDevice {
        fn new(traits: DeviceTraits, queue_work: Option<TestDeviceQueueWorkFn>) -> Self {
            Self { traits, queue_work }
        }
    }

    impl LegacyVirtioDevice for TestDevice {
        fn traits(&self) -> DeviceTraits {
            self.traits
        }

        fn read_registers_u32(&self, _offset: u16) -> u32 {
            0
        }

        fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

        fn get_work_callback(&mut self, index: u16) -> Box<dyn VirtioQueueWorkerContext + Send> {
            Box::new(TestDeviceWorker {
                index,
                queue_work: self.queue_work.clone(),
            })
        }

        fn state_change(&mut self, _state: &VirtioState) {}
    }

    struct TestDeviceWorker {
        index: u16,
        queue_work: Option<TestDeviceQueueWorkFn>,
    }

    #[async_trait]
    impl VirtioQueueWorkerContext for TestDeviceWorker {
        async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
            if let Err(err) = work {
                panic!(
                    "Invalid virtio queue state index {} error {}",
                    self.index,
                    err.as_ref() as &dyn std::error::Error
                );
            }
            if let Some(ref func) = self.queue_work {
                (func)(self.index, work.unwrap());
            }
            true
        }
    }

    impl VirtioPciTestDevice {
        fn new(
            driver: &DefaultDriver,
            num_queues: u16,
            test_mem: &Arc<VirtioTestMemoryAccess>,
            queue_work: Option<TestDeviceQueueWorkFn>,
        ) -> Self {
            let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
            let mem = GuestMemory::new("test", test_mem.clone());
            let mut msi_set = MsiInterruptSet::new();

            let dev = VirtioPciDevice::new(
                Box::new(LegacyWrapper::new(
                    &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
                    TestDevice::new(
                        DeviceTraits {
                            device_id: 3,
                            device_features: 2,
                            max_queues: num_queues,
                            device_register_length: 12,
                            ..Default::default()
                        },
                        queue_work,
                    ),
                    &mem,
                )),
                PciInterruptModel::Msix(&mut msi_set),
                Some(doorbell_registration),
                &mut ExternallyManagedMmioIntercepts,
                None,
            )
            .unwrap();

            let test_intc = Arc::new(TestPciInterruptController::new());
            msi_set.connect(test_intc.as_ref());

            Self {
                pci_device: dev,
                test_intc,
            }
        }

        fn read_u32(&mut self, address: u64) -> u32 {
            let mut value = [0; 4];
            self.pci_device.mmio_read(address, &mut value).unwrap();
            u32::from_ne_bytes(value)
        }

        fn write_u32(&mut self, address: u64, value: u32) {
            self.pci_device
                .mmio_write(address, &value.to_ne_bytes())
                .unwrap();
        }
    }

    #[async_test]
    async fn verify_chipset_config(driver: DefaultDriver) {
        let mem = VirtioTestMemoryAccess::new();
        let doorbell_registration: Arc<dyn DoorbellRegistration> = mem.clone();
        let mem = GuestMemory::new("test", mem);
        let interrupt = LineInterrupt::detached();

        let mut dev = VirtioMmioDevice::new(
            Box::new(LegacyWrapper::new(
                &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
                TestDevice::new(
                    DeviceTraits {
                        device_id: 3,
                        device_features: 2,
                        max_queues: 1,
                        device_register_length: 0,
                        ..Default::default()
                    },
                    None,
                ),
                &mem,
            )),
            interrupt,
            Some(doorbell_registration),
            0,
            1,
        );
        // magic value
        assert_eq!(dev.read_u32(0), u32::from_le_bytes(*b"virt"));
        // version
        assert_eq!(dev.read_u32(4), 2);
        // device ID
        assert_eq!(dev.read_u32(8), 3);
        // vendor ID
        assert_eq!(dev.read_u32(12), 0x1af4);
        // device feature (bank 0)
        assert_eq!(
            dev.read_u32(16),
            VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
        );
        // device feature bank index
        assert_eq!(dev.read_u32(20), 0);
        // device feature (bank 1)
        dev.write_u32(20, 1);
        assert_eq!(dev.read_u32(20), 1);
        assert_eq!(dev.read_u32(16), VIRTIO_F_VERSION_1);
        // device feature (bank 2)
        dev.write_u32(20, 2);
        assert_eq!(dev.read_u32(16), 0);
        // driver feature (bank 0)
        assert_eq!(dev.read_u32(32), 0);
        dev.write_u32(32, 2);
        assert_eq!(dev.read_u32(32), 2);
        dev.write_u32(32, 0xffffffff);
        assert_eq!(
            dev.read_u32(32),
            VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
        );
        // driver feature bank index
        assert_eq!(dev.read_u32(36), 0);
        dev.write_u32(36, 1);
        assert_eq!(dev.read_u32(36), 1);
        // driver feature (bank 1)
        assert_eq!(dev.read_u32(32), 0);
        dev.write_u32(32, 0xffffffff);
        assert_eq!(dev.read_u32(32), VIRTIO_F_VERSION_1);
        // driver feature (bank 2)
        dev.write_u32(36, 2);
        assert_eq!(dev.read_u32(32), 0);
        dev.write_u32(32, 0xffffffff);
        assert_eq!(dev.read_u32(32), 0);
        // host notify
        assert_eq!(dev.read_u32(80), 0);
        // interrupt status
        assert_eq!(dev.read_u32(96), 0);
        // interrupt ACK (queue 0)
        assert_eq!(dev.read_u32(100), 0);
        // device status
        assert_eq!(dev.read_u32(112), 0);
        // config generation
        assert_eq!(dev.read_u32(0xfc), 0);

        // queue index
        assert_eq!(dev.read_u32(48), 0);
        // queue max size (queue 0)
        assert_eq!(dev.read_u32(52), 0x40);
        // queue size (queue 0)
        assert_eq!(dev.read_u32(56), 0x40);
        dev.write_u32(56, 0x20);
        assert_eq!(dev.read_u32(56), 0x20);
        // queue enable (queue 0)
        assert_eq!(dev.read_u32(68), 0);
        dev.write_u32(68, 1);
        assert_eq!(dev.read_u32(68), 1);
        dev.write_u32(68, 0xffffffff);
        assert_eq!(dev.read_u32(68), 1);
        dev.write_u32(68, 0);
        assert_eq!(dev.read_u32(68), 0);
        // queue descriptor address low (queue 0)
        assert_eq!(dev.read_u32(128), 0);
        dev.write_u32(128, 0xffff);
        assert_eq!(dev.read_u32(128), 0xffff);
        // queue descriptor address high (queue 0)
        assert_eq!(dev.read_u32(132), 0);
        dev.write_u32(132, 1);
        assert_eq!(dev.read_u32(132), 1);
        // queue available address low (queue 0)
        assert_eq!(dev.read_u32(144), 0);
        dev.write_u32(144, 0xeeee);
        assert_eq!(dev.read_u32(144), 0xeeee);
        // queue available address high (queue 0)
        assert_eq!(dev.read_u32(148), 0);
        dev.write_u32(148, 2);
        assert_eq!(dev.read_u32(148), 2);
        // queue used address low (queue 0)
        assert_eq!(dev.read_u32(160), 0);
        dev.write_u32(160, 0xdddd);
        assert_eq!(dev.read_u32(160), 0xdddd);
        // queue used address high (queue 0)
        assert_eq!(dev.read_u32(164), 0);
        dev.write_u32(164, 3);
        assert_eq!(dev.read_u32(164), 3);

        // switch to queue #1
        dev.write_u32(48, 1);
        assert_eq!(dev.read_u32(48), 1);
        // queue max size (queue 1)
        assert_eq!(dev.read_u32(52), 0);
        // queue size (queue 1)
        assert_eq!(dev.read_u32(56), 0);
        dev.write_u32(56, 2);
        assert_eq!(dev.read_u32(56), 0);
        // queue enable (queue 1)
        assert_eq!(dev.read_u32(68), 0);
        dev.write_u32(68, 1);
        assert_eq!(dev.read_u32(68), 0);
        // queue descriptor address low (queue 1)
        assert_eq!(dev.read_u32(128), 0);
        dev.write_u32(128, 1);
        assert_eq!(dev.read_u32(128), 0);
        // queue descriptor address high (queue 1)
        assert_eq!(dev.read_u32(132), 0);
        dev.write_u32(132, 1);
        assert_eq!(dev.read_u32(132), 0);
        // queue available address low (queue 1)
        assert_eq!(dev.read_u32(144), 0);
        dev.write_u32(144, 1);
        assert_eq!(dev.read_u32(144), 0);
        // queue available address high (queue 1)
        assert_eq!(dev.read_u32(148), 0);
        dev.write_u32(148, 1);
        assert_eq!(dev.read_u32(148), 0);
        // queue used address low (queue 1)
        assert_eq!(dev.read_u32(160), 0);
        dev.write_u32(160, 1);
        assert_eq!(dev.read_u32(160), 0);
        // queue used address high (queue 1)
        assert_eq!(dev.read_u32(164), 0);
        dev.write_u32(164, 1);
        assert_eq!(dev.read_u32(164), 0);
    }

    #[async_test]
    async fn verify_pci_config(driver: DefaultDriver) {
        let mut pci_test_device =
            VirtioPciTestDevice::new(&driver, 1, &VirtioTestMemoryAccess::new(), None);
        let mut capabilities = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(4, &mut capabilities)
            .unwrap();
        assert_eq!(
            capabilities,
            (cfg_space::Status::CAPABILITIES_LIST.bits() as u32) << 16
        );
        let mut next_cap_offset = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(0x34, &mut next_cap_offset)
            .unwrap();
        assert_ne!(next_cap_offset, 0);

        let mut header = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16, &mut header)
            .unwrap();
        let header = header.to_le_bytes();
        assert_eq!(header[0], CapabilityId::MSIX.0);
        next_cap_offset = header[1] as u32;
        assert_ne!(next_cap_offset, 0);

        let mut header = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16, &mut header)
            .unwrap();
        let header = header.to_le_bytes();
        assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
        assert_eq!(header[3], VIRTIO_PCI_CAP_COMMON_CFG);
        assert_eq!(header[2], 16);
        let mut buf = 0;

        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
            .unwrap();
        assert_eq!(buf, 0);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
            .unwrap();
        assert_eq!(buf, 0);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
            .unwrap();
        assert_eq!(buf, 0x38);
        next_cap_offset = header[1] as u32;
        assert_ne!(next_cap_offset, 0);

        let mut header = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16, &mut header)
            .unwrap();
        let header = header.to_le_bytes();
        assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
        assert_eq!(header[3], VIRTIO_PCI_CAP_NOTIFY_CFG);
        assert_eq!(header[2], 20);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
            .unwrap();
        assert_eq!(buf, 0);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
            .unwrap();
        assert_eq!(buf, 0x38);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
            .unwrap();
        assert_eq!(buf, 4);
        next_cap_offset = header[1] as u32;
        assert_ne!(next_cap_offset, 0);

        let mut header = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16, &mut header)
            .unwrap();
        let header = header.to_le_bytes();
        assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
        assert_eq!(header[3], VIRTIO_PCI_CAP_ISR_CFG);
        assert_eq!(header[2], 16);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
            .unwrap();
        assert_eq!(buf, 0);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
            .unwrap();
        assert_eq!(buf, 0x3c);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
            .unwrap();
        assert_eq!(buf, 4);
        next_cap_offset = header[1] as u32;
        assert_ne!(next_cap_offset, 0);

        let mut header = 0;
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16, &mut header)
            .unwrap();
        let header = header.to_le_bytes();
        assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
        assert_eq!(header[3], VIRTIO_PCI_CAP_DEVICE_CFG);
        assert_eq!(header[2], 16);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
            .unwrap();
        assert_eq!(buf, 0);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
            .unwrap();
        assert_eq!(buf, 0x40);
        pci_test_device
            .pci_device
            .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
            .unwrap();
        assert_eq!(buf, 12);
        next_cap_offset = header[1] as u32;
        assert_eq!(next_cap_offset, 0);
    }

    #[async_test]
    async fn verify_pci_registers(driver: DefaultDriver) {
        let mut pci_test_device =
            VirtioPciTestDevice::new(&driver, 1, &VirtioTestMemoryAccess::new(), None);
        let bar_address1: u64 = 0x2000000000;
        pci_test_device
            .pci_device
            .pci_cfg_write(0x14, (bar_address1 >> 32) as u32)
            .unwrap();
        pci_test_device
            .pci_device
            .pci_cfg_write(0x10, bar_address1 as u32)
            .unwrap();

        let bar_address2: u64 = 0x4000;
        pci_test_device
            .pci_device
            .pci_cfg_write(0x1c, (bar_address2 >> 32) as u32)
            .unwrap();
        pci_test_device
            .pci_device
            .pci_cfg_write(0x18, bar_address2 as u32)
            .unwrap();

        pci_test_device
            .pci_device
            .pci_cfg_write(0x4, cfg_space::Command::MMIO_ENABLED.bits() as u32)
            .unwrap();

        // device feature bank index
        assert_eq!(pci_test_device.read_u32(bar_address1), 0);
        // device feature (bank 0)
        assert_eq!(
            pci_test_device.read_u32(bar_address1 + 4),
            VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
        );
        // device feature (bank 1)
        pci_test_device.write_u32(bar_address1, 1);
        assert_eq!(pci_test_device.read_u32(bar_address1), 1);
        assert_eq!(
            pci_test_device.read_u32(bar_address1 + 4),
            VIRTIO_F_VERSION_1
        );
        // device feature (bank 2)
        pci_test_device.write_u32(bar_address1, 2);
        assert_eq!(pci_test_device.read_u32(bar_address1), 2);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 4), 0);
        // driver feature bank index
        assert_eq!(pci_test_device.read_u32(bar_address1 + 8), 0);
        // driver feature (bank 0)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
        pci_test_device.write_u32(bar_address1 + 12, 2);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 2);
        pci_test_device.write_u32(bar_address1 + 12, 0xffffffff);
        assert_eq!(
            pci_test_device.read_u32(bar_address1 + 12),
            VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
        );
        // driver feature (bank 1)
        pci_test_device.write_u32(bar_address1 + 8, 1);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 8), 1);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
        pci_test_device.write_u32(bar_address1 + 12, 0xffffffff);
        assert_eq!(
            pci_test_device.read_u32(bar_address1 + 12),
            VIRTIO_F_VERSION_1
        );
        // driver feature (bank 2)
        pci_test_device.write_u32(bar_address1 + 8, 2);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 8), 2);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
        pci_test_device.write_u32(bar_address1 + 12, 0xffffffff);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
        // max queues and the msix vector for config changes
        assert_eq!(pci_test_device.read_u32(bar_address1 + 16), 1 << 16);
        // queue index, config generation and device status
        assert_eq!(pci_test_device.read_u32(bar_address1 + 20), 0);
        // current queue size and msix vector
        assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0x40);
        pci_test_device.write_u32(bar_address1 + 24, 0x20);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0x20);
        // current queue enabled and notify offset
        assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
        pci_test_device.write_u32(bar_address1 + 28, 1);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 1);
        pci_test_device.write_u32(bar_address1 + 28, 0xffff);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 1);
        pci_test_device.write_u32(bar_address1 + 28, 0);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
        // current queue descriptor table address (low)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0);
        pci_test_device.write_u32(bar_address1 + 32, 0xffff);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0xffff);
        // current queue descriptor table address (high)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 0);
        pci_test_device.write_u32(bar_address1 + 36, 1);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 1);
        // current queue available ring address (low)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0);
        pci_test_device.write_u32(bar_address1 + 40, 0xeeee);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0xeeee);
        // current queue available ring address (high)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 0);
        pci_test_device.write_u32(bar_address1 + 44, 2);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 2);
        // current queue used ring address (low)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0);
        pci_test_device.write_u32(bar_address1 + 48, 0xdddd);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0xdddd);
        // current queue used ring address (high)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 0);
        pci_test_device.write_u32(bar_address1 + 52, 3);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 3);
        // VIRTIO_PCI_CAP_NOTIFY_CFG notification register
        assert_eq!(pci_test_device.read_u32(bar_address1 + 56), 0);
        // VIRTIO_PCI_CAP_ISR_CFG register
        assert_eq!(pci_test_device.read_u32(bar_address1 + 60), 0);

        // switch to queue #1 (disabled, only one queue on this device)
        let queue_index: u16 = 1;
        pci_test_device
            .pci_device
            .mmio_write(bar_address1 + 22, &queue_index.to_le_bytes())
            .unwrap();
        assert_eq!(pci_test_device.read_u32(bar_address1 + 20), 1 << 24);
        // current queue size and msix vector
        assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0);
        pci_test_device.write_u32(bar_address1 + 24, 2);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0);
        // current queue enabled and notify offset
        assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
        pci_test_device.write_u32(bar_address1 + 28, 1);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
        // current queue descriptor table address (low)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0);
        pci_test_device.write_u32(bar_address1 + 32, 0x10);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0);
        // current queue descriptor table address (high)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 0);
        pci_test_device.write_u32(bar_address1 + 36, 0x10);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 0);
        // current queue available ring address (low)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0);
        pci_test_device.write_u32(bar_address1 + 40, 0x10);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0);
        // current queue available ring address (high)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 0);
        pci_test_device.write_u32(bar_address1 + 44, 0x10);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 0);
        // current queue used ring address (low)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0);
        pci_test_device.write_u32(bar_address1 + 48, 0x10);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0);
        // current queue used ring address (high)
        assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 0);
        pci_test_device.write_u32(bar_address1 + 52, 0x10);
        assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 0);
    }

    #[async_test]
    async fn verify_queue_simple(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
        let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
        let (tx, mut rx) = mesh::mpsc_channel();
        let event = Event::new();
        let mut queues = guest.create_direct_queues(|i| {
            let tx = tx.clone();
            CreateDirectQueueParams {
                process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                    let mut work = work.expect("Queue failure");
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].address, base_addr);
                    assert_eq!(work.payload[0].length, 0x1000);
                    work.complete(123);
                    true
                }),
                notify: Interrupt::from_fn(move || {
                    tx.send(i as usize);
                }),
                event: event.clone(),
            }
        });

        guest.add_to_avail_queue(0);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        let (desc, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
        queues[0].stop().await;
    }

    #[async_test]
    async fn verify_queue_indirect(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
        let (tx, mut rx) = mesh::mpsc_channel();
        let event = Event::new();
        let mut queues = guest.create_direct_queues(|i| {
            let tx = tx.clone();
            CreateDirectQueueParams {
                process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                    let mut work = work.expect("Queue failure");
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].address, 0xffffffff00000000u64);
                    assert_eq!(work.payload[0].length, 0x1000);
                    work.complete(123);
                    true
                }),
                notify: Interrupt::from_fn(move || {
                    tx.send(i as usize);
                }),
                event: event.clone(),
            }
        });

        guest.add_indirect_to_avail_queue(0);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        let (desc, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
        queues[0].stop().await;
    }

    #[async_test]
    async fn verify_queue_linked(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 5, true);
        let (tx, mut rx) = mesh::mpsc_channel();
        let base_address = guest.get_queue_descriptor_backing_memory_address(0);
        let event = Event::new();
        let mut queues = guest.create_direct_queues(|i| {
            let tx = tx.clone();
            CreateDirectQueueParams {
                process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                    let mut work = work.expect("Queue failure");
                    assert_eq!(work.payload.len(), 3);
                    for i in 0..work.payload.len() {
                        assert_eq!(work.payload[i].address, base_address + 0x1000 * i as u64);
                        assert_eq!(work.payload[i].length, 0x1000);
                    }
                    work.complete(123 * 3);
                    true
                }),
                notify: Interrupt::from_fn(move || {
                    tx.send(i as usize);
                }),
                event: event.clone(),
            }
        });

        guest.add_linked_to_avail_queue(0, 3);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        let (desc, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123 * 3);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
        queues[0].stop().await;
    }

    #[async_test]
    async fn verify_queue_indirect_linked(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 5, true);
        let (tx, mut rx) = mesh::mpsc_channel();
        let event = Event::new();
        let mut queues = guest.create_direct_queues(|i| {
            let tx = tx.clone();
            CreateDirectQueueParams {
                process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                    let mut work = work.expect("Queue failure");
                    assert_eq!(work.payload.len(), 3);
                    for i in 0..work.payload.len() {
                        assert_eq!(
                            work.payload[i].address,
                            0xffffffff00000000u64 + 0x1000 * i as u64
                        );
                        assert_eq!(work.payload[i].length, 0x1000);
                    }
                    work.complete(123 * 3);
                    true
                }),
                notify: Interrupt::from_fn(move || {
                    tx.send(i as usize);
                }),
                event: event.clone(),
            }
        });

        guest.add_indirect_linked_to_avail_queue(0, 3);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        let (desc, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123 * 3);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
        queues[0].stop().await;
    }

    #[async_test]
    async fn verify_queue_avail_rollover(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
        let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
        let (tx, mut rx) = mesh::mpsc_channel();
        let event = Event::new();
        let mut queues = guest.create_direct_queues(|i| {
            let tx = tx.clone();
            CreateDirectQueueParams {
                process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                    let mut work = work.expect("Queue failure");
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].address, base_addr);
                    assert_eq!(work.payload[0].length, 0x1000);
                    work.complete(123);
                    true
                }),
                notify: Interrupt::from_fn(move || {
                    tx.send(i as usize);
                }),
                event: event.clone(),
            }
        });

        for _ in 0..3 {
            guest.add_to_avail_queue(0);
            event.signal();
            must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
            let (desc, len) = guest.get_next_completed(0).unwrap();
            assert_eq!(desc, 0u16);
            assert_eq!(len, 123);
            assert_eq!(guest.get_next_completed(0).is_none(), true);
        }

        queues[0].stop().await;
    }

    #[async_test]
    async fn verify_multi_queue(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 5, 2, true);
        let (tx, mut rx) = mesh::mpsc_channel();
        let events = (0..guest.num_queues)
            .map(|_| Event::new())
            .collect::<Vec<_>>();
        let mut queues = guest.create_direct_queues(|queue_index| {
            let tx = tx.clone();
            let base_addr = guest.get_queue_descriptor_backing_memory_address(queue_index);
            CreateDirectQueueParams {
                process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                    let mut work = work.expect("Queue failure");
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].address, base_addr);
                    assert_eq!(work.payload[0].length, 0x1000);
                    work.complete(123 * queue_index as u32);
                    true
                }),
                notify: Interrupt::from_fn(move || {
                    tx.send(queue_index as usize);
                }),
                event: events[queue_index as usize].clone(),
            }
        });

        for (i, event) in events.iter().enumerate() {
            let queue_index = i as u16;
            guest.add_to_avail_queue(queue_index);
            event.signal();
        }
        // wait for all queue processing to finish
        for _ in 0..guest.num_queues {
            must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        }
        // check results
        for queue_index in 0..guest.num_queues {
            let (desc, len) = guest.get_next_completed(queue_index).unwrap();
            assert_eq!(desc, 0u16);
            assert_eq!(len, 123 * queue_index as u32);
        }
        // verify no extraneous completions
        for (i, queue) in queues.iter_mut().enumerate() {
            let queue_index = i as u16;
            assert_eq!(guest.get_next_completed(queue_index).is_none(), true);
            queue.stop().await;
        }
    }

    fn take_mmio_interrupt_status(dev: &mut VirtioMmioDevice, mask: u32) -> u32 {
        let mut v = [0; 4];
        dev.mmio_read(96, &mut v).unwrap();
        dev.mmio_write(100, &mask.to_ne_bytes()).unwrap();
        u32::from_ne_bytes(v)
    }

    async fn expect_mmio_interrupt(
        dev: &mut VirtioMmioDevice,
        target: &TestLineInterruptTarget,
        mask: u32,
        multiple_expected: bool,
    ) {
        poll_fn(|cx| target.poll_high(cx, 0)).await;
        let v = take_mmio_interrupt_status(dev, mask);
        assert_eq!(v & mask, mask);
        assert!(multiple_expected || !target.is_high(0));
    }

    #[async_test]
    async fn verify_device_queue_simple(driver: DefaultDriver) {
        let test_mem = VirtioTestMemoryAccess::new();
        let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
        let mem = guest.mem();
        let features = ((VIRTIO_F_VERSION_1 as u64) << 32) | VIRTIO_F_RING_EVENT_IDX as u64 | 2;
        let target = TestLineInterruptTarget::new_arc();
        let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
        let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
        let queue_work = Arc::new(move |_: u16, mut work: VirtioQueueCallbackWork| {
            assert_eq!(work.payload.len(), 1);
            assert_eq!(work.payload[0].address, base_addr);
            assert_eq!(work.payload[0].length, 0x1000);
            work.complete(123);
        });
        let mut dev = VirtioMmioDevice::new(
            Box::new(LegacyWrapper::new(
                &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
                TestDevice::new(
                    DeviceTraits {
                        device_id: 3,
                        device_features: features,
                        max_queues: 1,
                        device_register_length: 0,
                        ..Default::default()
                    },
                    Some(queue_work),
                ),
                &mem,
            )),
            interrupt,
            Some(doorbell_registration),
            0,
            1,
        );

        guest.setup_chipset_device(&mut dev, features);
        expect_mmio_interrupt(
            &mut dev,
            &target,
            VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE,
            false,
        )
        .await;
        guest.add_to_avail_queue(0);
        // notify device
        dev.write_u32(80, 0);
        expect_mmio_interrupt(
            &mut dev,
            &target,
            VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
            false,
        )
        .await;
        let (desc, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
        // reset the device
        dev.write_u32(112, 0);
        drop(dev);
    }

    #[async_test]
    async fn verify_device_multi_queue(driver: DefaultDriver) {
        let num_queues = 5;
        let test_mem = VirtioTestMemoryAccess::new();
        let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, num_queues, 2, true);
        let mem = guest.mem();
        let features = ((VIRTIO_F_VERSION_1 as u64) << 32) | VIRTIO_F_RING_EVENT_IDX as u64 | 2;
        let target = TestLineInterruptTarget::new_arc();
        let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
        let base_addr: Vec<_> = (0..num_queues)
            .map(|i| guest.get_queue_descriptor_backing_memory_address(i))
            .collect();
        let queue_work = Arc::new(move |i: u16, mut work: VirtioQueueCallbackWork| {
            assert_eq!(work.payload.len(), 1);
            assert_eq!(work.payload[0].address, base_addr[i as usize]);
            assert_eq!(work.payload[0].length, 0x1000);
            work.complete(123 * i as u32);
        });
        let mut dev = VirtioMmioDevice::new(
            Box::new(LegacyWrapper::new(
                &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
                TestDevice::new(
                    DeviceTraits {
                        device_id: 3,
                        device_features: features,
                        max_queues: num_queues + 1,
                        device_register_length: 0,
                        ..Default::default()
                    },
                    Some(queue_work),
                ),
                &mem,
            )),
            interrupt,
            Some(doorbell_registration),
            0,
            1,
        );
        guest.setup_chipset_device(&mut dev, features);
        expect_mmio_interrupt(
            &mut dev,
            &target,
            VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE,
            false,
        )
        .await;
        for i in 0..num_queues {
            guest.add_to_avail_queue(i);
            // notify device
            dev.write_u32(80, i as u32);
        }
        // check results
        for i in 0..num_queues {
            let (desc, len) = loop {
                if let Some(x) = guest.get_next_completed(i) {
                    break x;
                }
                expect_mmio_interrupt(
                    &mut dev,
                    &target,
                    VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
                    i < (num_queues - 1),
                )
                .await;
            };
            assert_eq!(desc, 0u16);
            assert_eq!(len, 123 * i as u32);
        }
        // verify no extraneous completions
        for i in 0..num_queues {
            assert_eq!(guest.get_next_completed(i).is_none(), true);
        }
        // reset the device
        dev.write_u32(112, 0);
        drop(dev);
    }

    #[async_test]
    async fn verify_device_multi_queue_pci(driver: DefaultDriver) {
        let num_queues = 5;
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new(&driver, &test_mem, num_queues, 2, true);
        let features = ((VIRTIO_F_VERSION_1 as u64) << 32) | VIRTIO_F_RING_EVENT_IDX as u64 | 2;
        let base_addr: Vec<_> = (0..num_queues)
            .map(|i| guest.get_queue_descriptor_backing_memory_address(i))
            .collect();
        let mut dev = VirtioPciTestDevice::new(
            &driver,
            num_queues + 1,
            &test_mem,
            Some(Arc::new(move |i, mut work| {
                assert_eq!(work.payload.len(), 1);
                assert_eq!(work.payload[0].address, base_addr[i as usize]);
                assert_eq!(work.payload[0].length, 0x1000);
                work.complete(123 * i as u32);
            })),
        );

        guest.setup_pci_device(&mut dev, features);

        let mut timer = PolledTimer::new(&driver);

        // expect a config generation interrupt
        timer.sleep(Duration::from_millis(100)).await;
        let delivered = dev.test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.0, 0);
        assert!(dev.test_intc.get_next_interrupt().is_none());

        for i in 0..num_queues {
            guest.add_to_avail_queue(i);
            // notify device
            dev.write_u32(0x10000000000 + 0x38, i as u32);
        }
        // verify all queue processing finished
        timer.sleep(Duration::from_millis(100)).await;
        for _ in 0..num_queues {
            let delivered = dev.test_intc.get_next_interrupt();
            assert!(delivered.is_some());
        }
        // check results
        for i in 0..num_queues {
            let (desc, len) = guest.get_next_completed(i).unwrap();
            assert_eq!(desc, 0u16);
            assert_eq!(len, 123 * i as u32);
        }
        // verify no extraneous completions
        for i in 0..num_queues {
            assert_eq!(guest.get_next_completed(i).is_none(), true);
        }
        // reset the device
        let device_status: u8 = 0;
        dev.pci_device
            .mmio_write(0x10000000000 + 20, &device_status.to_le_bytes())
            .unwrap();
        drop(dev);
    }
}
