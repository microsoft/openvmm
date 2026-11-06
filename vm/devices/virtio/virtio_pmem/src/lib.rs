// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use std::fs;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
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

#[derive(InspectMut)]
pub struct Device {
    driver: VmTaskDriver,
    #[inspect(skip)]
    mappable: sparse_mmap::Mappable,
    #[inspect(skip)]
    padding: Option<sparse_mmap::Mappable>,
    #[inspect(skip)]
    region: Option<Arc<dyn guestmem::MappedMemoryRegion>>,
    file_len: usize,
    len: usize,
    writable: bool,
    #[inspect(mut)]
    worker: TaskControl<PmemWorker, PmemQueue>,
}

impl Device {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        file: fs::File,
        writable: bool,
    ) -> anyhow::Result<Self> {
        // Linux requires PMEM namespaces to satisfy memremap_pages' 2 MB
        // subsection alignment.
        const PMEM_ALIGNMENT: u64 = 0x200000;

        let metadata = file.metadata().context("failed to get metadata")?;
        let file_len = metadata.len();
        anyhow::ensure!(file_len != 0, "persistent memory file must not be empty");
        anyhow::ensure!(
            file_len.is_multiple_of(4096),
            "persistent memory file size must be a multiple of 4 KiB"
        );
        let len = file_len
            .checked_add(PMEM_ALIGNMENT - 1)
            .context("persistent memory file is too large")?
            & !(PMEM_ALIGNMENT - 1);
        let mut mapped_file_len = file_len;
        let padding = if file_len == len {
            None
        } else if writable {
            file.set_len(len)
                .context("failed to align persistent memory file")?;
            mapped_file_len = len;
            None
        } else {
            Some(
                sparse_mmap::alloc_shared_memory(
                    (len - file_len)
                        .try_into()
                        .context("persistent memory padding is too large")?,
                    "virtio-pmem-padding",
                )
                .context("failed to allocate persistent memory padding")?,
            )
        };
        let file_len = mapped_file_len
            .try_into()
            .context("persistent memory file is too large")?;
        let len = len
            .try_into()
            .context("persistent memory region is too large")?;
        let mappable = sparse_mmap::new_mappable_from_file(&file, writable, false)
            .context("failed to create file mapping")?;
        #[cfg(windows)]
        let flush_mapping = if writable {
            let mapping = sparse_mmap::SparseMapping::new(file_len)
                .context("failed to create flush mapping")?;
            mapping
                .map_file(0, file_len, &mappable, 0, true)
                .context("failed to map persistent memory for flushing")?;
            Some(Arc::new(mapping))
        } else {
            None
        };
        Ok(Self {
            driver: driver_source.simple(),
            worker: TaskControl::new(PmemWorker {
                writable,
                file: Arc::new(file),
                #[cfg(windows)]
                file_len,
                #[cfg(windows)]
                flush_mapping,
            }),
            mappable,
            padding,
            region: None,
            file_len,
            len,
            writable,
        })
    }
}

#[repr(C)]
struct PmemConfig {
    start: u64,
    size: u64,
}

/// The driver can discover the memory range via the shared memory capability.
const VIRTIO_PMEM_F_SHMEM_REGION: u32 = 0;

/// Shared memory region ID used by virtio-pmem for its memory range.
const VIRTIO_PMEM_SHMEM_REGION_ID: u8 = 0;

impl VirtioDevice for Device {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::PMEM,
            device_features: VirtioDeviceFeatures::new()
                .with_device_specific_low(1 << VIRTIO_PMEM_F_SHMEM_REGION)
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 1,
            device_register_length: size_of::<PmemConfig>() as u32,
            shared_memory: DeviceTraitsSharedMemory {
                id: VIRTIO_PMEM_SHMEM_REGION_ID,
                size: self.len as u64,
            },
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        // These fields are unused when SHMEM_REGION is negotiated, but are
        // required for drivers that discover the range via device config.
        let start = self
            .region
            .as_ref()
            .and_then(|region| region.guest_address())
            .unwrap_or(0);
        match offset {
            0 => start as u32,
            4 => (start >> 32) as u32,
            8 => self.len as u32,
            12 => ((self.len as u64) >> 32) as u32,
            _ => 0,
        }
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn set_shared_memory_region(
        &mut self,
        region: &Arc<dyn guestmem::MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        region
            .map(0, &self.mappable, 0, self.file_len, self.writable)
            .context("failed to map persistent memory file")?;
        if let Some(padding) = &self.padding {
            region
                .map(
                    self.file_len,
                    padding,
                    0,
                    self.len - self.file_len,
                    self.writable,
                )
                .context("failed to map persistent memory padding")?;
        }

        self.region = Some(region.clone());
        Ok(())
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        assert_eq!(idx, 0);

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

        self.worker.insert(
            self.driver.clone(),
            "virtio-pmem-queue",
            PmemQueue {
                queue,
                mem: resources.guest_memory,
            },
        );
        self.worker.start();
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        assert_eq!(idx, 0);
        if !self.worker.has_state() {
            return None;
        }
        self.worker.stop().await;
        let state = self.worker.remove().queue.queue_state();
        Some(state)
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

#[derive(InspectMut)]
struct PmemWorker {
    writable: bool,
    file: Arc<fs::File>,
    #[cfg(windows)]
    file_len: usize,
    #[cfg(windows)]
    #[inspect(skip)]
    flush_mapping: Option<Arc<sparse_mmap::SparseMapping>>,
}

impl PmemWorker {
    async fn flush(&self) -> anyhow::Result<()> {
        let file = self.file.clone();
        #[cfg(windows)]
        let flush_mapping = self.flush_mapping.clone();
        #[cfg(windows)]
        let file_len = self.file_len;
        blocking::unblock(move || {
            #[cfg(windows)]
            flush_mapping
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("flush mapping was not created"))?
                .flush_view(0, file_len)
                .context("failed to flush mapped persistent memory")?;
            file.sync_all()
                .context("failed to flush persistent memory file")
        })
        .await
    }
}

impl InspectTaskMut<PmemQueue> for PmemWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut PmemQueue>) {
        req.respond().merge(self).merge(state);
    }
}

#[derive(InspectMut)]
struct PmemQueue {
    queue: VirtioQueue,
    mem: GuestMemory,
}

impl AsyncRun<PmemQueue> for PmemWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut PmemQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    let bytes = process_pmem_request(self, &state.mem, &work).await;
                    state.queue.complete(work, bytes);
                }
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "queue error");
                    break;
                }
            }
        }
        Ok(())
    }
}

async fn process_pmem_request(
    worker: &PmemWorker,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
) -> u32 {
    let mut req = [0; 4];
    let err = match work.read(mem, &mut req) {
        Ok(_) => match u32::from_le_bytes(req) {
            0 if !worker.writable => {
                // Ignore the request for read-only devices.
                0
            }
            0 => match worker.flush().await {
                Ok(()) => 0,
                Err(err) => {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "flush error"
                    );
                    u32::MAX
                }
            },
            n => {
                tracing::error!(n, "unsupported request");
                u32::MAX
            }
        },
        Err(err) => {
            tracing::error!(error = &err as &dyn std::error::Error, "invalid descriptor");
            u32::MAX
        }
    };
    let _ = work.write(mem, &u32::to_le_bytes(err));
    4
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigSpace;
    use guestmem::MappableGuestMemory;
    use guestmem::MappedMemoryRegion;
    use guestmem::MemoryMapper;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use pci_core::msi::MsiConnection;
    use pci_core::spec::caps::CapabilityId;
    use sparse_mmap::AsMappableRef;
    use sparse_mmap::SparseMapping;
    use std::io;
    use test_with_tracing::test;
    use virtio::PciInterruptModel;
    use virtio::spec::pci::VirtioPciCapType;
    use virtio::transport::VirtioPciDevice;
    use vmcore::save_restore::AsyncSaveRestore;
    use vmcore::vm_task::SingleDriverBackend;

    type TestGuestAddress = Arc<Mutex<Option<u64>>>;

    struct TestGuestMapping(TestGuestAddress);

    impl MappableGuestMemory for TestGuestMapping {
        fn map_to_guest(&mut self, gpa: u64, _writable: bool) -> io::Result<()> {
            *self.0.lock() = Some(gpa);
            Ok(())
        }

        fn unmap_from_guest(&mut self) {
            *self.0.lock() = None;
        }
    }

    struct TestRegion(SparseMapping, TestGuestAddress);

    impl MappedMemoryRegion for TestRegion {
        fn guest_address(&self) -> Option<u64> {
            *self.1.lock()
        }

        fn map(
            &self,
            offset: usize,
            section: &dyn AsMappableRef,
            file_offset: u64,
            len: usize,
            writable: bool,
        ) -> io::Result<()> {
            self.0.map_file(offset, len, section, file_offset, writable)
        }

        fn unmap(&self, offset: usize, len: usize) -> io::Result<()> {
            self.0.unmap(offset, len)
        }
    }

    struct TestMapper(TestGuestAddress);

    impl MemoryMapper for TestMapper {
        fn new_region(
            &self,
            len: usize,
            _debug_name: String,
        ) -> io::Result<(Box<dyn MappableGuestMemory>, Arc<dyn MappedMemoryRegion>)> {
            Ok((
                Box::new(TestGuestMapping(self.0.clone())),
                Arc::new(TestRegion(SparseMapping::new(len)?, self.0.clone())),
            ))
        }
    }

    fn pci_read(dev: &mut VirtioPciDevice, offset: u16) -> u32 {
        let mut value = 0;
        dev.pci_cfg_read(
            offset,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        )
        .unwrap();
        value
    }

    fn pci_write(dev: &mut VirtioPciDevice, offset: u16, value: u32) {
        dev.pci_cfg_write(offset, ByteEnabledDwordWrite::with_all_bytes_enabled(value))
            .unwrap();
    }

    fn find_cap(dev: &mut VirtioPciDevice, kind: VirtioPciCapType) -> u16 {
        let mut offset = pci_read(dev, 0x34) as u16;
        while offset != 0 {
            let header = pci_read(dev, offset).to_le_bytes();
            if header[0] == CapabilityId::VENDOR_SPECIFIC.0 && header[3] == kind.0 {
                return offset;
            }
            offset = header[1] as u16;
        }
        panic!("missing virtio capability {kind:?}");
    }

    async fn read_config(dev: &mut VirtioPciDevice, address: u64) -> u64 {
        let mut data = [0; 8];
        match dev.mmio_read(address, &mut data) {
            IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
            other => panic!("expected deferred config read, got {other:?}"),
        }
        u64::from_le_bytes(data)
    }

    async fn read_window(dev: &mut VirtioPciDevice, cap: u16, offset: u32, len: u32) -> u32 {
        pci_write(dev, cap + 8, offset);
        pci_write(dev, cap + 12, len);
        let mut value = 0;
        let mut data = [0; 4];
        match dev.pci_cfg_read(
            cap + 16,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        ) {
            IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
            other => panic!("expected deferred config read, got {other:?}"),
        }
        u32::from_le_bytes(data)
    }

    #[async_test]
    async fn pmem_config_tracks_shared_memory_bar(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let file = tempfile::tempfile().unwrap();
        file.set_len(0x401000).unwrap();
        let device = Device::new(&driver_source, file, false).unwrap();
        let mapped_address = Arc::new(Mutex::new(None));
        let msi = MsiConnection::new();
        let mut dev = VirtioPciDevice::new(
            Box::new(device),
            &driver,
            GuestMemory::empty(),
            PciInterruptModel::Msix(&msi.target()),
            None,
            &mut ExternallyManagedMmioIntercepts,
            Some(&TestMapper(mapped_address.clone())),
        )
        .unwrap();
        let config_cap = find_cap(&mut dev, VirtioPciCapType::DEVICE_CFG);
        let config_offset = pci_read(&mut dev, config_cap + 8);
        let window_cap = find_cap(&mut dev, VirtioPciCapType::PCI_CFG);
        let shmem_cap = find_cap(&mut dev, VirtioPciCapType::SHARED_MEMORY_CFG);
        assert_eq!(pci_read(&mut dev, shmem_cap + 12), 0x600000);
        const BAR0: u64 = 0x100000000;
        pci_write(&mut dev, 0x14, (BAR0 >> 32) as u32);
        pci_write(&mut dev, 0x10, BAR0 as u32);
        pci_write(&mut dev, 0x04, 2);

        // No device-specific feature has been negotiated.
        let config_address = BAR0 + config_offset as u64;
        assert_eq!(read_config(&mut dev, config_address).await, 0);
        assert_eq!(read_config(&mut dev, config_address + 8).await, 0x600000);

        let mut saved = None;
        for start in [0x1234_5680_0000u64, 0xabcd_1200_0000] {
            pci_write(&mut dev, 0x04, 0);
            pci_write(&mut dev, 0x24, (start >> 32) as u32);
            pci_write(&mut dev, 0x20, start as u32);
            pci_write(&mut dev, 0x04, 2);
            assert_eq!(*mapped_address.lock(), Some(start));
            assert_eq!(read_config(&mut dev, config_address).await, start);
            assert_eq!(read_config(&mut dev, config_address + 8).await, 0x600000);
            assert_eq!(
                read_window(&mut dev, window_cap, config_offset, 4).await,
                start as u32
            );
            assert_eq!(
                read_window(&mut dev, window_cap, config_offset + 4, 4).await,
                (start >> 32) as u32
            );
            assert_eq!(
                read_window(&mut dev, window_cap, config_offset + 6, 2).await,
                (start >> 48) as u32
            );
            assert_eq!(
                read_window(&mut dev, window_cap, config_offset + 8, 4).await,
                0x600000
            );
            if saved.is_none() {
                saved = Some(dev.save().await.unwrap());
            }
        }
        dev.restore(saved.unwrap()).await.unwrap();
        assert_eq!(
            read_config(&mut dev, config_address).await,
            mapped_address.lock().unwrap()
        );
        assert_eq!(
            read_config(&mut dev, config_address).await,
            0x1234_5680_0000
        );

        pci_write(&mut dev, 0x04, 0);
        assert_eq!(*mapped_address.lock(), None);
        assert_eq!(read_window(&mut dev, window_cap, config_offset, 4).await, 0);
        assert_eq!(
            read_window(&mut dev, window_cap, config_offset + 4, 4).await,
            0
        );
    }
}
