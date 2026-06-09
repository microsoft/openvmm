// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::virtio_util::VirtioPayloadReader;
use crate::virtio_util::VirtioPayloadWriter;
use anyhow::Context as _;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use std::io;
use std::io::Write;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Default request queue count when the caller does not specify one. Two
/// queues let the guest's FUSE-request hashing send concurrent operations
/// on different inodes to different host workers, which is the whole
/// point of having more than one queue, while keeping the per-device
/// footprint (MSI-X vectors, kernel worker threads, host tasks) modest.
///
/// Callers that know the appropriate concurrency for their environment
/// (e.g., guest vCPU count for an in-VMM device, or host parallelism for
/// a host service like `wsldevicehost`) should pass an explicit value via
/// [`VirtioFsDevice::with_num_request_queues`].
const DEFAULT_NUM_REQUEST_QUEUES: u32 = 2;

/// Upper bound for the request queue count. Past this, virtio-fs's
/// hash-based queue selection has diminishing returns, and each extra queue
/// costs a guest MSI-X vector plus a kernel worker thread.
const MAX_REQUEST_QUEUES: u32 = 8;

/// Options controlling how a [`VirtioFsDevice`] is created.
///
/// Use [`VirtioFsDeviceOptions::default`] for the in-VMM defaults, then
/// override individual fields as needed.
#[derive(Debug, Clone, Copy)]
pub struct VirtioFsDeviceOptions {
    /// Number of FUSE request queues. Clamped to `[1, MAX_REQUEST_QUEUES]`.
    pub num_request_queues: u32,
    /// Maximum size, in bytes, of a single DMA mapping the transport can
    /// perform, or `None` if the transport imposes no additional limit
    /// (the in-VMM path, where DMA targets guest memory directly).
    ///
    /// Bounce-buffered transports have a hard per-mapping cap: a guest forced
    /// to use the Linux swiotlb can only map 256 KiB at a time
    /// (`IO_TLB_SEGSIZE * IO_TLB_SIZE`), regardless of the total swiotlb
    /// window. Without a limit, the guest virtio-fs driver builds single
    /// physically-contiguous request buffers up to the negotiated FUSE
    /// `max_write` (1 MiB by default), which the transport then fails to map,
    /// producing cascading I/O errors. Setting this to the transport's
    /// per-mapping limit clamps the negotiated `max_write` accordingly.
    ///
    /// Must be non-zero when `Some`; a zero limit is a misconfiguration and
    /// will panic during device construction.
    pub max_dma_mapping_size: Option<u32>,
}

impl Default for VirtioFsDeviceOptions {
    fn default() -> Self {
        Self {
            num_request_queues: DEFAULT_NUM_REQUEST_QUEUES,
            max_dma_mapping_size: None,
        }
    }
}

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
    #[inspect(skip)]
    fs: Arc<fuse::Session>,
    #[inspect(skip)]
    workers: Vec<TaskControl<VirtioFsWorker, VirtioFsQueue>>,
    shmem_size: u64,
    #[inspect(skip)]
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    #[inspect(skip)]
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
    /// The maximum write size offered to the FUSE client during negotiation,
    /// and a hard upper bound on it: a filesystem `init` callback may lower
    /// the value actually negotiated, but never raise it above this. Clamped
    /// below the 1 MiB default for bounce-buffered transports (see
    /// [`VirtioFsDeviceOptions::max_dma_mapping_size`]).
    max_write: u32,
}

impl VirtioFsDevice {
    /// Creates a new `VirtioFsDevice` with the specified mount tag.
    ///
    /// The number of FUSE request queues defaults to
    /// `DEFAULT_NUM_REQUEST_QUEUES`. Callers that know the appropriate
    /// concurrency for their environment (e.g., guest vCPU count for an
    /// in-VMM device, or host parallelism for a host service) should use
    /// [`Self::with_num_request_queues`] instead.
    pub fn new<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        Self::with_options(
            driver_source,
            tag,
            fs,
            shmem_size,
            notify_corruption,
            VirtioFsDeviceOptions::default(),
        )
    }

    /// Creates a new `VirtioFsDevice` with an explicit number of FUSE
    /// request queues. The value is clamped to `[1, MAX_REQUEST_QUEUES]`.
    pub fn with_num_request_queues<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
        num_request_queues: u32,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        Self::with_options(
            driver_source,
            tag,
            fs,
            shmem_size,
            notify_corruption,
            VirtioFsDeviceOptions {
                num_request_queues,
                ..Default::default()
            },
        )
    }

    /// Creates a new `VirtioFsDevice` with the given [`VirtioFsDeviceOptions`].
    ///
    /// This is the most general constructor; [`Self::new`] and
    /// [`Self::with_num_request_queues`] are thin wrappers over it.
    pub fn with_options<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
        options: VirtioFsDeviceOptions,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        let num_request_queues = options.num_request_queues.clamp(1, MAX_REQUEST_QUEUES);

        // On a bounce-buffered transport, clamp the negotiated max_write to the
        // transport's per-mapping DMA limit so the guest never builds request
        // buffers it cannot map. The in-VMM path leaves this unset and keeps
        // the full default.
        let max_write = match options.max_dma_mapping_size {
            Some(limit) => {
                // A zero per-mapping limit is a misconfiguration: it would
                // negotiate a max_write of 0, producing an unusable device.
                assert!(limit > 0, "max_dma_mapping_size must be non-zero");
                fuse::DEFAULT_MAX_WRITE.min(limit)
            }
            None => fuse::DEFAULT_MAX_WRITE,
        };

        let mut config = VirtioFsDeviceConfig {
            tag: [0; 36],
            num_request_queues,
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
            fs: Arc::new(fuse::Session::with_max_write(fs, max_write)),
            workers: Vec::new(),
            shmem_size,
            shared_memory_region: None,
            notify_corruption,
            max_write,
        }
    }
}

impl VirtioDevice for VirtioFsDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::FS,
            device_features: VirtioDeviceFeatures::new()
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 1 + self.config.num_request_queues as u16,
            device_register_length: self.config.as_bytes().len() as u32,
            shared_memory: DeviceTraitsSharedMemory {
                id: 0,
                size: self.shmem_size,
            },
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
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

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracing::warn!(offset, val, "[virtiofs] Unknown write",);
    }

    fn set_shared_memory_region(
        &mut self,
        region: &Arc<dyn MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        self.shared_memory_region = Some(region.clone());
        Ok(())
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        let mut tc = TaskControl::new(VirtioFsWorker {
            fs: self.fs.clone(),
            shared_memory_region: self.shared_memory_region.clone(),
            shared_memory_size: self.shmem_size,
            notify_corruption: self.notify_corruption.clone(),
        });

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create polled wait")?;
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        tc.insert(
            self.driver.clone(),
            &*self.task_name,
            VirtioFsQueue {
                queue,
                mem: resources.guest_memory,
            },
        );
        tc.start();

        let idx = idx as usize;
        if idx >= self.workers.len() {
            self.workers.resize_with(idx + 1, || {
                TaskControl::new(VirtioFsWorker {
                    fs: self.fs.clone(),
                    shared_memory_region: None,
                    shared_memory_size: 0,
                    notify_corruption: self.notify_corruption.clone(),
                })
            });
        }
        self.workers[idx] = tc;
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        let idx = idx as usize;
        if idx >= self.workers.len() || !self.workers[idx].has_state() {
            return None;
        }
        self.workers[idx].stop().await;
        let state = self.workers[idx].remove().queue.queue_state();
        Some(state)
    }

    async fn reset(&mut self) {
        self.workers.clear();
        if let Some(region) = &self.shared_memory_region {
            if let Err(e) = region.unmap(0, self.shmem_size as usize) {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "failed to unmap DAX region on reset"
                );
            }
        }
        self.shared_memory_region = None;
        self.fs.destroy();
    }
}

struct VirtioFsWorker {
    fs: Arc<fuse::Session>,
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    shared_memory_size: u64,
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
}

struct VirtioFsQueue {
    queue: VirtioQueue,
    mem: GuestMemory,
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
                    let bytes = process_virtiofs_request(self, &state.mem, &work);
                    state.queue.complete(work, bytes);
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

fn process_virtiofs_request(
    worker: &VirtioFsWorker,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
) -> u32 {
    // Parse the request.
    let reader = VirtioPayloadReader::new(mem, work);
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
            return 0;
        }
    };

    // Dispatch to the file system. The sender writes the reply into guest
    // memory but does not complete the descriptor—completion happens once,
    // after dispatch returns. For FUSE no-reply operations (Forget,
    // BatchForget, Destroy), send() is never called and bytes_written
    // stays 0.
    let mut sender = VirtioReplySender {
        work,
        mem,
        bytes_written: 0,
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
    sender.bytes_written
}
/// An implementation of `ReplySender` for virtio payload.
///
/// Writes the FUSE reply into guest memory and records the byte count.
/// Does not complete the descriptor—the caller is responsible for that.
struct VirtioReplySender<'a> {
    work: &'a VirtioQueueCallbackWork,
    mem: &'a GuestMemory,
    bytes_written: u32,
}

impl fuse::ReplySender for VirtioReplySender<'_> {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let mut writer = VirtioPayloadWriter::new(self.mem, self.work);
        let mut size = 0;

        // Write all the slices to the payload buffers.
        // N.B. write_vectored isn't used because it isn't guaranteed to write all the data.
        for buf in bufs {
            writer.write_all(buf)?;
            size += buf.len();
        }

        self.bytes_written = size as u32;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VirtioFs;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use vmcore::vm_task::SingleDriverBackend;

    fn make_device(
        driver: &DefaultDriver,
        num_request_queues: Option<u32>,
    ) -> (VirtioFsDevice, tempfile::TempDir) {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new(tmpdir.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = match num_request_queues {
            Some(n) => {
                VirtioFsDevice::with_num_request_queues(&driver_source, "testfs", fs, 0, None, n)
            }
            None => VirtioFsDevice::new(&driver_source, "testfs", fs, 0, None),
        };
        (device, tmpdir)
    }

    fn make_device_with_options(
        driver: &DefaultDriver,
        options: VirtioFsDeviceOptions,
    ) -> (VirtioFsDevice, tempfile::TempDir) {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new(tmpdir.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioFsDevice::with_options(&driver_source, "testfs", fs, 0, None, options);
        (device, tmpdir)
    }

    #[async_test]
    async fn new_uses_default_num_request_queues(driver: DefaultDriver) {
        let (device, _tmp) = make_device(&driver, None);
        assert_eq!(device.config.num_request_queues, DEFAULT_NUM_REQUEST_QUEUES);
        assert_eq!(
            device.traits().max_queues,
            1 + DEFAULT_NUM_REQUEST_QUEUES as u16
        );
    }

    #[async_test]
    async fn with_num_request_queues_clamps_above_max(driver: DefaultDriver) {
        let (device, _tmp) = make_device(&driver, Some(1000));
        assert_eq!(device.config.num_request_queues, MAX_REQUEST_QUEUES);
        assert_eq!(device.traits().max_queues, 1 + MAX_REQUEST_QUEUES as u16);
    }

    #[async_test]
    async fn with_num_request_queues_clamps_below_one(driver: DefaultDriver) {
        // A request for zero queues must be clamped up to one so the device
        // always exposes at least one request virtqueue.
        let (device, _tmp) = make_device(&driver, Some(0));
        assert_eq!(device.config.num_request_queues, 1);
        // 1 hiprio queue + 1 request queue.
        assert_eq!(device.traits().max_queues, 2);
    }

    #[async_test]
    async fn with_num_request_queues_accepts_value_in_range(driver: DefaultDriver) {
        let (device, _tmp) = make_device(&driver, Some(3));
        assert_eq!(device.config.num_request_queues, 3);
        assert_eq!(device.traits().max_queues, 4);
    }

    #[async_test]
    async fn new_uses_default_max_write(driver: DefaultDriver) {
        // The in-VMM path (no DMA mapping limit) keeps the full default.
        let (device, _tmp) = make_device(&driver, None);
        assert_eq!(device.max_write, fuse::DEFAULT_MAX_WRITE);
    }

    #[async_test]
    async fn with_options_clamps_max_write_for_bounce_buffered(driver: DefaultDriver) {
        // A bounce-buffered (swiotlb) transport has a 256 KiB per-mapping
        // limit, so the negotiated max_write must be clamped down from the
        // 1 MiB default.
        const SWIOTLB_MAX_WRITE: u32 = 256 * 1024;
        let (device, _tmp) = make_device_with_options(
            &driver,
            VirtioFsDeviceOptions {
                max_dma_mapping_size: Some(SWIOTLB_MAX_WRITE),
                ..Default::default()
            },
        );
        assert_eq!(device.max_write, SWIOTLB_MAX_WRITE);
    }

    #[async_test]
    async fn with_options_does_not_raise_max_write_above_default(driver: DefaultDriver) {
        // A transport limit larger than the default must not increase
        // max_write beyond the default.
        let (device, _tmp) = make_device_with_options(
            &driver,
            VirtioFsDeviceOptions {
                max_dma_mapping_size: Some(4 * 1024 * 1024),
                ..Default::default()
            },
        );
        assert_eq!(device.max_write, fuse::DEFAULT_MAX_WRITE);
    }

    #[async_test]
    #[should_panic(expected = "max_dma_mapping_size must be non-zero")]
    async fn with_options_rejects_zero_dma_mapping_size(driver: DefaultDriver) {
        // A zero per-mapping limit is a misconfiguration and must be rejected
        // early rather than producing a device with max_write of 0.
        let _ = make_device_with_options(
            &driver,
            VirtioFsDeviceOptions {
                max_dma_mapping_size: Some(0),
                ..Default::default()
            },
        );
    }
}
