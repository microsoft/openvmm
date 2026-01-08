// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(dead_code)]

use crate::virtio_util::VirtioPayloadReader;
use crate::virtio_util::VirtioPayloadWriter;
use async_trait::async_trait;
use guestmem::GuestMemory;
use guestmem::MappedMemoryRegion;
use pal_async::task::Spawn;
use std::io;
use std::io::Write;
use std::sync::Arc;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueueCallbackWork;
use virtio::VirtioQueueState;
use virtio::VirtioQueueWorker;
use virtio::VirtioQueueWorkerContext;
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
pub struct VirtioFsDevice {
    name: Box<str>,

    driver: VmTaskDriver,
    config: VirtioFsDeviceConfig,
    memory: GuestMemory,
    fs: Arc<fuse::Session>,
    workers: Vec<TaskControl<VirtioQueueWorker, VirtioQueueState>>,
    exit_event: event_listener::Event,
    shmem_size: u64,
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
            name: format!("virtio-fs-{}", tag).into(),
            driver: driver_source.simple(),
            config,
            memory,
            fs: Arc::new(fuse::Session::new(fs)),
            workers: Vec::new(),
            exit_event: event_listener::Event::new(),
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

    fn enable(&mut self, resources: Resources) {
        self.workers = resources
            .queues
            .into_iter()
            .filter_map(|queue_resources| {
                if !queue_resources.params.enable {
                    return None;
                }
                let worker = VirtioFsWorker {
                    fs: self.fs.clone(),
                    mem: self.memory.clone(),
                    shared_memory_region: resources.shared_memory_region.clone(),
                    shared_memory_size: resources.shared_memory_size,
                    notify_corruption: self.notify_corruption.clone(),
                };
                let worker = VirtioQueueWorker::new(self.driver.clone(), Box::new(worker));
                Some(worker.into_running_task(
                    "virtiofs-virtio-queue".to_string(),
                    self.memory.clone(),
                    resources.features.clone(),
                    queue_resources,
                    self.exit_event.listen(),
                ))
            })
            .collect();
    }

    fn disable(&mut self) {
        self.exit_event.notify(usize::MAX);
        let mut workers = self.workers.drain(..).collect::<Vec<_>>();
        self.driver
            .spawn("shutdown-virtiofs-queues".to_owned(), async move {
                futures::future::join_all(workers.iter_mut().map(async |worker| {
                    worker.stop().await;
                }))
                .await;
            })
            .detach();
    }
}

struct VirtioFsWorker {
    fs: Arc<fuse::Session>,
    mem: GuestMemory,
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    shared_memory_size: u64,
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
}

#[async_trait]
impl VirtioQueueWorkerContext for VirtioFsWorker {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        if let Err(err) = work {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "Failed processing queue"
            );
            return false;
        }

        let mut work = work.unwrap();
        // Parse the request.
        let reader = VirtioPayloadReader::new(&self.mem, &work);
        let request = match fuse::Request::new(reader) {
            Ok(request) => request,
            Err(e) => {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "[virtiofs] Invalid FUSE message, error"
                );
                // Often this will result in the guest failing the device as there is no response to a request.
                (self.notify_corruption)();
                // This only happens if even the header couldn't be parsed, so there's no way
                // to send an error reply since the request's unique ID isn't known.
                work.complete(0);
                return true;
            }
        };

        // Dispatch to the file system.
        let mut sender = VirtioReplySender {
            work,
            mem: &self.mem,
        };
        let mapper = self
            .shared_memory_region
            .as_ref()
            .map(|shared_memory_region| VirtioMapper {
                region: shared_memory_region.as_ref(),
                size: self.shared_memory_size,
            });
        self.fs.dispatch(
            request,
            &mut sender,
            mapper.as_ref().map(|x| x as &dyn fuse::Mapper),
        );
        true
    }
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
