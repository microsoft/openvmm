// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::virtio_util::VirtioPayloadReader;
use crate::virtio_util::VirtioPayloadWriter;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const VIRTIO_DEVICE_TYPE_FS: u16 = 26;

/// PCI configuration space values for virtio-fs devices.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
struct VirtioFsDeviceConfig {
    tag: [u8; 36],
    num_request_queues: u32,
}

/// A virtio-fs PCI device.
#[derive(InspectMut)]
pub struct VirtioFsDevice {
    task_name: Box<str>,
    driver: VmTaskDriver,
    #[inspect(skip)]
    config: VirtioFsDeviceConfig,
    mem: GuestMemory,
    #[inspect(skip)]
    fs: Arc<fuse::Session>,
    #[inspect(skip)]
    workers: Vec<TaskControl<VirtioFsWorker, VirtioFsQueue>>,
    shmem_size: u64,
    #[inspect(skip)]
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
}

impl VirtioFsDevice {
    /// Creates a new `VirtioFsDevice` with the specified mount tag.
    pub fn new<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        memory: GuestMemory,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        let mut config = VirtioFsDeviceConfig {
            tag: [0; 36],
            num_request_queues: 1,
        };

        let notify_corruption = if let Some(notify) = notify_corruption {
            notify
        } else {
            Arc::new(|| {})
        };

        // Copy the tag into the config space (truncate it for now if too long).
        let length = std::cmp::min(tag.len(), config.tag.len());
        config.tag[..length].copy_from_slice(&tag.as_bytes()[..length]);

        Self {
            task_name: format!("virtiofs-{}", tag).into(),
            driver: driver_source.simple(),
            config,
            mem: memory,
            fs: Arc::new(fuse::Session::new(fs)),
            workers: Vec::new(),
            shmem_size,
            notify_corruption,
        }
    }
}

impl VirtioDevice for VirtioFsDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VIRTIO_DEVICE_TYPE_FS,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: 2,
            device_register_length: self.config.as_bytes().len() as u32,
            shared_memory: DeviceTraitsSharedMemory {
                id: 0,
                size: self.shmem_size,
            },
        }
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
        let offset = offset as usize;
        let config = self.config.as_bytes();
        if offset < config.len() {
            u32::from_le_bytes(
                config[offset..offset + 4]
                    .try_into()
                    .expect("Incorrect length"),
            )
        } else {
            0
        }
    }

    fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracing::warn!(offset, val, "[virtiofs] Unknown write",);
    }

    fn enable(&mut self, resources: Resources) -> anyhow::Result<()> {
        self.workers = resources
            .queues
            .into_iter()
            .filter_map(|queue_resources| {
                if !queue_resources.params.enable {
                    return None;
                }

                let mut tc = TaskControl::new(VirtioFsWorker {
                    fs: self.fs.clone(),
                    mem: self.mem.clone(),
                    shared_memory_region: resources.shared_memory_region.clone(),
                    shared_memory_size: resources.shared_memory_size,
                    notify_corruption: self.notify_corruption.clone(),
                });

                let queue_event = PolledWait::new(&self.driver, queue_resources.event).unwrap();
                let queue = VirtioQueue::new(
                    resources.features.clone(),
                    queue_resources.params,
                    self.mem.clone(),
                    queue_resources.notify,
                    queue_event,
                )
                .expect("failed to create virtio queue");

                tc.insert(
                    self.driver.clone(),
                    &*self.task_name,
                    VirtioFsQueue { queue },
                );
                tc.start();
                Some(tc)
            })
            .collect();
        Ok(())
    }

    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        for worker in &mut self.workers {
            ready!(worker.poll_stop(cx));
        }
        self.workers.clear();
        Poll::Ready(())
    }
}

struct VirtioFsWorker {
    fs: Arc<fuse::Session>,
    mem: GuestMemory,
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    shared_memory_size: u64,
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
}

struct VirtioFsQueue {
    queue: VirtioQueue,
}

impl AsyncRun<VirtioFsQueue> for VirtioFsWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut VirtioFsQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    process_virtiofs_request(self, work);
                }
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "Failed processing queue"
                    );
                    break;
                }
            }
        }
        Ok(())
    }
}

fn process_virtiofs_request(worker: &VirtioFsWorker, mut work: VirtioQueueCallbackWork) {
    // Parse the request.
    let reader = VirtioPayloadReader::new(&worker.mem, &work);
    let request = match fuse::Request::new(reader) {
        Ok(request) => request,
        Err(e) => {
            tracing::error!(
                error = &e as &dyn std::error::Error,
                "[virtiofs] Invalid FUSE message, error"
            );
            // Often this will result in the guest failing the device as there is no response to a request.
            (worker.notify_corruption)();
            // This only happens if even the header couldn't be parsed, so there's no way
            // to send an error reply since the request's unique ID isn't known.
            work.complete(0);
            return;
        }
    };

    // Dispatch to the file system.
    let mut sender = VirtioReplySender {
        work,
        mem: &worker.mem,
    };
    let mapper = worker
        .shared_memory_region
        .as_ref()
        .map(|shared_memory_region| VirtioMapper {
            region: shared_memory_region.as_ref(),
            size: worker.shared_memory_size,
        });
    worker.fs.dispatch(
        request,
        &mut sender,
        mapper.as_ref().map(|x| x as &dyn fuse::Mapper),
    );
}
/// An implementation of `ReplySender` for virtio payload.
struct VirtioReplySender<'a> {
    work: VirtioQueueCallbackWork,
    mem: &'a GuestMemory,
}

impl fuse::ReplySender for VirtioReplySender<'_> {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let mut writer = VirtioPayloadWriter::new(self.mem, &self.work);
        let mut size = 0;

        // Write all the slices to the payload buffers.
        // N.B. write_vectored isn't used because it isn't guaranteed to write all the data.
        for buf in bufs {
            writer.write_all(buf)?;
            size += buf.len();
        }

        self.work.complete(size as u32);
        Ok(())
    }
}

struct VirtioMapper<'a> {
    region: &'a dyn MappedMemoryRegion,
    size: u64,
}

impl fuse::Mapper for VirtioMapper<'_> {
    fn map(
        &self,
        offset: u64,
        file: fuse::FileRef<'_>,
        file_offset: u64,
        len: u64,
        writable: bool,
    ) -> lx::Result<()> {
        let offset = offset.try_into().map_err(|_| lx::Error::EINVAL)?;
        let len = len.try_into().map_err(|_| lx::Error::EINVAL)?;
        self.region.map(offset, &file, file_offset, len, writable)?;
        Ok(())
    }

    fn unmap(&self, offset: u64, len: u64) -> lx::Result<()> {
        let offset = offset.try_into().map_err(|_| lx::Error::EINVAL)?;
        let len = len.try_into().map_err(|_| lx::Error::EINVAL)?;
        self.region.unmap(offset, len)?;
        Ok(())
    }

    fn clear(&self) {
        let result = self.region.unmap(0, self.size as usize);
        if let Err(result) = result {
            tracing::error!(
                error = &result as &dyn std::error::Error,
                "Failed to unmap shared memory"
            );
        }
    }
}
