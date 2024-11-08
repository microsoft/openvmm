// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for accessing a MANA device via VFIO on Linux.

#![cfg(target_os = "linux")]
#![cfg(feature = "vfio")]

use crate::interrupt::DeviceInterrupt;
use crate::interrupt::DeviceInterruptSource;
use crate::memory::MemoryBlock;
use crate::DeviceBacking;
use crate::DeviceRegisterIo;
use anyhow::Context;
use futures::FutureExt;
use futures_concurrency::future::Race;
use inspect::Inspect;
use inspect_counters::SharedCounter;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use uevent::UeventListener;
use vfio_sys::IommuType;
use vfio_sys::IrqInfo;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::AsBytes;
use zerocopy::FromBytes;

pub trait VfioDmaBuffer: 'static + Send + Sync {
    /// Create a new DMA buffer of the given `len` bytes. Guaranteed to be zero-initialized.
    fn create_dma_buffer(&self, len: usize) -> anyhow::Result<MemoryBlock>;
}

/// A device backend accessed via VFIO.
#[derive(Inspect)]
pub struct VfioDevice {
    pci_id: Arc<str>,
    #[inspect(skip)]
    _container: vfio_sys::Container,
    #[inspect(skip)]
    _group: vfio_sys::Group,
    #[inspect(skip)]
    device: Arc<vfio_sys::Device>,
    #[inspect(skip)]
    dma_buffer: Arc<dyn VfioDmaBuffer>,
    #[inspect(skip)]
    msix_info: IrqInfo,
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(iter_by_index)]
    interrupts: Vec<Option<InterruptState>>,
}

#[derive(Inspect)]
struct InterruptState {
    #[inspect(skip)]
    interrupt: DeviceInterrupt,
    target_cpu: Arc<AtomicU32>,
    #[inspect(skip)]
    _task: Task<()>,
}

impl VfioDevice {
    /// Creates a new VFIO-backed device for the PCI device with `pci_id`.
    pub async fn new(
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        dma_buffer: Arc<dyn VfioDmaBuffer>,
    ) -> anyhow::Result<Self> {
        let path = Path::new("/sys/bus/pci/devices").join(pci_id);

        // The vfio device attaches asynchronously after the PCI device is added,
        // so make sure that it has completed by checking for the vfio-dev subpath.
        let vmbus_device =
            std::fs::read_link(&path).context("failed to read link for pci device")?;
        let instance_path = Path::new("/sys").join(vmbus_device.strip_prefix("../../..")?);
        let vfio_arrived_path = instance_path.join("vfio-dev");
        let uevent_listener = UeventListener::new(&driver_source.simple())?;
        let wait_for_vfio_device = uevent_listener
            .wait_for_matching_child(&vfio_arrived_path, move |_, _| async move { Some(()) });
        let mut ctx = mesh::CancelContext::new().with_timeout(Duration::from_secs(1));
        // Ignore any errors and always attempt to open.
        let _ = ctx.until_cancelled(wait_for_vfio_device).await;

        let container = vfio_sys::Container::new()?;
        let group_id = vfio_sys::Group::find_group_for_device(&path)?;
        let group = vfio_sys::Group::open_noiommu(group_id)?;
        group.set_container(&container)?;
        if !group.status()?.viable() {
            anyhow::bail!("group is not viable");
        }

        container.set_iommu(IommuType::NoIommu)?;
        let device = group.open_device(path.file_name().unwrap().to_str().unwrap())?;
        let msix_info = device.irq_info(vfio_bindings::bindings::vfio::VFIO_PCI_MSIX_IRQ_INDEX)?;
        if msix_info.flags.noresize() {
            anyhow::bail!("unsupported: kernel does not support dynamic msix allocation");
        }

        Ok(Self {
            pci_id: pci_id.into(),
            _container: container,
            _group: group,
            device: Arc::new(device),
            dma_buffer,
            msix_info,
            driver_source: driver_source.clone(),
            interrupts: Vec::new(),
        })
    }

    fn map_bar(&self, n: u8) -> anyhow::Result<MappedRegionWithFallback> {
        if n >= 6 {
            anyhow::bail!("invalid bar");
        }
        let info = self.device.region_info(n.into())?;
        let mapping = self.device.map(info.offset, info.size as usize, true)?;
        sparse_mmap::initialize_try_copy();
        Ok(MappedRegionWithFallback {
            device: self.device.clone(),
            mapping,
            offset: info.offset,
            read_fallback: SharedCounter::new(),
            write_fallback: SharedCounter::new(),
        })
    }
}

/// A mapped region that falls back to read/write if the memory mapped access
/// fails.
///
/// This should only happen for CVM, and only when the MMIO is emulated by the
/// host.
#[derive(Inspect)]
pub struct MappedRegionWithFallback {
    #[inspect(skip)]
    device: Arc<vfio_sys::Device>,
    #[inspect(skip)]
    mapping: vfio_sys::MappedRegion,
    offset: u64,
    read_fallback: SharedCounter,
    write_fallback: SharedCounter,
}

impl DeviceBacking for VfioDevice {
    type Registers = MappedRegionWithFallback;
    type DmaAllocator = LockedMemoryAllocator;

    fn id(&self) -> &str {
        &self.pci_id
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        (*self).map_bar(n)
    }

    fn host_allocator(&self) -> Self::DmaAllocator {
        LockedMemoryAllocator {
            dma_buffer: self.dma_buffer.clone(),
        }
    }

    fn max_interrupt_count(&self) -> u32 {
        self.msix_info.count
    }

    fn map_interrupt(&mut self, msix: u32, cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        if msix >= self.msix_info.count {
            anyhow::bail!("invalid msix index");
        }
        if self.interrupts.len() <= msix as usize {
            self.interrupts.resize_with(msix as usize + 1, || None);
        }

        let interrupt = &mut self.interrupts[msix as usize];
        if let Some(interrupt) = interrupt {
            // The interrupt has been mapped before. Just retarget it to the new
            // CPU on the next interrupt, if needed.
            if interrupt.target_cpu.load(Relaxed) != cpu {
                interrupt.target_cpu.store(cpu, Relaxed);
            }
            return Ok(interrupt.interrupt.clone());
        }

        let new_interrupt = {
            let name = format!("vfio-interrupt-{pci_id}-{msix}", pci_id = self.pci_id);
            let driver = self
                .driver_source
                .builder()
                .run_on_target(true)
                .target_vp(cpu)
                .build(&name);

            let event =
                PolledWait::new(&driver, Event::new()).context("failed to allocate polled wait")?;

            let source = DeviceInterruptSource::new();
            self.device
                .map_msix(msix, [event.get().as_fd()])
                .context("failed to map msix")?;

            // The interrupt's CPU affinity will be set by the task when it
            // starts. This can block the thread briefly, so it's better to do
            // it on the target CPU.
            let irq = vfio_sys::find_msix_irq(&self.pci_id, msix)
                .context("failed to find irq for msix")?;

            let target_cpu = Arc::new(AtomicU32::new(cpu));

            let interrupt = source.new_target();

            let task = driver.spawn(
                name,
                InterruptTask {
                    driver: driver.clone(),
                    target_cpu: target_cpu.clone(),
                    pci_id: self.pci_id.clone(),
                    msix,
                    irq,
                    event,
                    source,
                }
                .run(),
            );

            InterruptState {
                interrupt,
                target_cpu,
                _task: task,
            }
        };

        Ok(interrupt.insert(new_interrupt).interrupt.clone())
    }
}

struct InterruptTask {
    driver: VmTaskDriver,
    target_cpu: Arc<AtomicU32>,
    pci_id: Arc<str>,
    msix: u32,
    irq: u32,
    event: PolledWait<Event>,
    source: DeviceInterruptSource,
}

impl InterruptTask {
    async fn run(mut self) {
        let mut current_cpu = !0;
        loop {
            let next_cpu = self.target_cpu.load(Relaxed);
            let r = if next_cpu == current_cpu {
                self.event.wait().await
            } else {
                self.driver.retarget_vp(next_cpu);
                // Wait until the target CPU is ready before updating affinity,
                // since otherwise the CPU may not be online.
                enum Event {
                    TargetVpReady(()),
                    Interrupt(std::io::Result<()>),
                }
                match (
                    self.driver.wait_target_vp_ready().map(Event::TargetVpReady),
                    self.event.wait().map(Event::Interrupt),
                )
                    .race()
                    .await
                {
                    Event::TargetVpReady(()) => {
                        if let Err(err) = set_irq_affinity(self.irq, next_cpu) {
                            // This should only occur due to extreme low resources.
                            // However, it is not a fatal error--it will just result in
                            // worse performance--so do not panic.
                            tracing::error!(
                                pci_id = self.pci_id.as_ref(),
                                msix = self.msix,
                                irq = self.irq,
                                error = &err as &dyn std::error::Error,
                                "failed to set irq affinity"
                            );
                        }
                        current_cpu = next_cpu;
                        continue;
                    }
                    Event::Interrupt(r) => {
                        // An interrupt arrived while waiting for the VP to be
                        // ready. Signal and loop around to try again.
                        r
                    }
                }
            };

            r.expect("wait cannot fail on eventfd");
            self.source.signal();
        }
    }
}

fn set_irq_affinity(irq: u32, cpu: u32) -> std::io::Result<()> {
    fs_err::write(
        format!("/proc/irq/{}/smp_affinity_list", irq),
        cpu.to_string(),
    )
}

impl DeviceRegisterIo for vfio_sys::MappedRegion {
    fn read_u32(&self, offset: usize) -> u32 {
        self.read_u32(offset)
    }

    fn read_u64(&self, offset: usize) -> u64 {
        self.read_u64(offset)
    }

    fn write_u32(&self, offset: usize, data: u32) {
        self.write_u32(offset, data)
    }

    fn write_u64(&self, offset: usize, data: u64) {
        self.write_u64(offset, data)
    }
}

impl MappedRegionWithFallback {
    fn mapping<T>(&self, offset: usize) -> *mut T {
        assert!(offset <= self.mapping.len() - size_of::<T>() && offset % align_of::<T>() == 0);
        if cfg!(feature = "mmio_simulate_fallback") {
            return std::ptr::NonNull::dangling().as_ptr();
        }
        // SAFETY: the offset is validated to be in bounds.
        unsafe { self.mapping.as_ptr().byte_add(offset).cast() }
    }

    fn read_from_mapping<T: AsBytes + FromBytes>(
        &self,
        offset: usize,
    ) -> Result<T, sparse_mmap::MemoryError> {
        // SAFETY: the offset is validated to be in bounds and aligned.
        unsafe { sparse_mmap::try_read_volatile(self.mapping::<T>(offset)) }
    }

    fn write_to_mapping<T: AsBytes + FromBytes>(
        &self,
        offset: usize,
        data: T,
    ) -> Result<(), sparse_mmap::MemoryError> {
        // SAFETY: the offset is validated to be in bounds and aligned.
        unsafe { sparse_mmap::try_write_volatile(self.mapping::<T>(offset), &data) }
    }

    fn read_from_file(&self, offset: usize, buf: &mut [u8]) {
        tracing::trace!(offset, n = buf.len(), "read");
        self.read_fallback.increment();
        let n = self
            .device
            .as_ref()
            .as_ref()
            .read_at(buf, self.offset + offset as u64)
            .expect("valid mapping");
        assert_eq!(n, buf.len());
    }

    fn write_to_file(&self, offset: usize, buf: &[u8]) {
        tracing::trace!(offset, n = buf.len(), "write");
        self.write_fallback.increment();
        let n = self
            .device
            .as_ref()
            .as_ref()
            .write_at(buf, self.offset + offset as u64)
            .expect("valid mapping");
        assert_eq!(n, buf.len());
    }
}

impl DeviceRegisterIo for MappedRegionWithFallback {
    fn read_u32(&self, offset: usize) -> u32 {
        self.read_from_mapping(offset).unwrap_or_else(|_| {
            let mut buf = [0u8; 4];
            self.read_from_file(offset, &mut buf);
            u32::from_ne_bytes(buf)
        })
    }

    fn read_u64(&self, offset: usize) -> u64 {
        self.read_from_mapping(offset).unwrap_or_else(|_| {
            let mut buf = [0u8; 8];
            self.read_from_file(offset, &mut buf);
            u64::from_ne_bytes(buf)
        })
    }

    fn write_u32(&self, offset: usize, data: u32) {
        self.write_to_mapping(offset, data).unwrap_or_else(|_| {
            self.write_to_file(offset, &data.to_ne_bytes());
        })
    }

    fn write_u64(&self, offset: usize, data: u64) {
        self.write_to_mapping(offset, data).unwrap_or_else(|_| {
            self.write_to_file(offset, &data.to_ne_bytes());
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PciDeviceResetMethod {
    NoReset,
    Acpi,
    Flr,
    AfFlr,
    Pm,
    Bus,
}

pub fn vfio_set_device_reset_method(
    pci_id: impl AsRef<str>,
    method: PciDeviceResetMethod,
) -> std::io::Result<()> {
    let reset_method = match method {
        PciDeviceResetMethod::NoReset => "\0".as_bytes(),
        PciDeviceResetMethod::Acpi => "acpi\0".as_bytes(),
        PciDeviceResetMethod::Flr => "flr\0".as_bytes(),
        PciDeviceResetMethod::AfFlr => "af_flr\0".as_bytes(),
        PciDeviceResetMethod::Pm => "pm\0".as_bytes(),
        PciDeviceResetMethod::Bus => "bus\0".as_bytes(),
    };

    let path: std::path::PathBuf = ["/sys/bus/pci/devices", pci_id.as_ref(), "reset_method"]
        .iter()
        .collect();
    fs_err::write(path, reset_method)?;
    Ok(())
}

pub struct LockedMemoryAllocator {
    dma_buffer: Arc<dyn VfioDmaBuffer>,
}

impl crate::HostDmaAllocator for LockedMemoryAllocator {
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<MemoryBlock> {
        self.dma_buffer.create_dma_buffer(len)
    }
}
