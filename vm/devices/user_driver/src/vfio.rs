// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for accessing a MANA device via VFIO on Linux.

#![cfg(target_os = "linux")]
#![cfg(feature = "vfio")]

use crate::DeviceBacking;
use crate::DeviceRegisterIo;
use crate::DmaClient;
use crate::interrupt::DeviceInterrupt;
use crate::interrupt::DeviceInterruptSource;
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
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use uevent::UeventListener;
use vfio_bindings::bindings::vfio::VFIO_PCI_CONFIG_REGION_INDEX;
use vfio_sys::IommuType;
use vfio_sys::IrqInfo;
use vfio_sys::VfioErrata;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Clone)]
pub enum VfioDmaClients {
    PersistentOnly(Arc<dyn DmaClient>),
    EphemeralOnly(Arc<dyn DmaClient>),
    Split {
        persistent: Arc<dyn DmaClient>,
        ephemeral: Arc<dyn DmaClient>,
    },
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
    msix_info: IrqInfo,
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(iter_by_index)]
    interrupts: Vec<Option<InterruptState>>,
    #[inspect(skip)]
    config_space: vfio_sys::RegionInfo,
    #[inspect(skip)]
    dma_clients: VfioDmaClients,
}

#[derive(Inspect)]
struct InterruptState {
    #[inspect(skip)]
    interrupt: DeviceInterrupt,
    target_cpu: Arc<AtomicU32>,
    #[inspect(skip)]
    _task: Task<()>,
}

impl Drop for VfioDevice {
    fn drop(&mut self) {
        // Just for tracing ...
        tracing::trace!(pci_id = ?self.pci_id, "dropping vfio device");
    }
}

impl VfioDevice {
    /// Creates a new VFIO-backed device for the PCI device with `pci_id`.
    pub async fn new(
        driver_source: &VmTaskDriverSource,
        pci_id: impl AsRef<str>,
        dma_clients: VfioDmaClients,
    ) -> anyhow::Result<Self> {
        Self::restore(driver_source, pci_id, false, dma_clients).await
    }

    /// Creates a new VFIO-backed device for the PCI device with `pci_id`.
    /// or creates a device from the saved state if provided.
    pub async fn restore(
        driver_source: &VmTaskDriverSource,
        pci_id: impl AsRef<str>,
        keepalive: bool,
        dma_clients: VfioDmaClients,
    ) -> anyhow::Result<Self> {
        let pci_id = pci_id.as_ref();
        let path = Path::new("/sys/bus/pci/devices").join(pci_id);

        // The vfio device attaches asynchronously after the PCI device is added,
        // so make sure that it has completed by checking for the vfio-dev subpath.
        let vmbus_device =
            std::fs::read_link(&path).context("failed to read link for pci device")?;
        let instance_path = Path::new("/sys").join(vmbus_device.strip_prefix("../../..")?);
        let vfio_arrived_path = instance_path.join("vfio-dev");
        let uevent_listener = UeventListener::new(&driver_source.simple())?;
        let wait_for_vfio_device =
            uevent_listener.wait_for_matching_child(&vfio_arrived_path, async |_, _| Some(()));
        let mut ctx = mesh::CancelContext::new().with_timeout(Duration::from_secs(1));
        // Ignore any errors and always attempt to open.
        let _ = ctx.until_cancelled(wait_for_vfio_device).await;

        tracing::info!(pci_id, keepalive, "device arrived");
        vfio_sys::print_relevant_params();

        let workaround = VfioDelayWorkaroundConfig::new(driver_source.clone(), None);
        let container = vfio_sys::Container::new()?;
        let group_id = vfio_sys::Group::find_group_for_device(&path)?;
        let group = workaround
            .retry_unconditional("open_noiommu", async |_| {
                vfio_sys::Group::open_noiommu(group_id)
            })
            .await?;
        group.set_container(&container)?;
        if !group.status()?.viable() {
            anyhow::bail!("group is not viable");
        }

        container.set_iommu(IommuType::NoIommu)?;
        if keepalive {
            // Prevent physical hardware interaction when restoring.
            group.set_keep_alive(pci_id).await?;
        }
        tracing::debug!(pci_id, "about to open device");
        let device = group.open_device(pci_id).await?;
        let msix_info = device.irq_info(vfio_bindings::bindings::vfio::VFIO_PCI_MSIX_IRQ_INDEX)?;
        if msix_info.flags.noresize() {
            anyhow::bail!("unsupported: kernel does not support dynamic msix allocation");
        }

        let config_space = device.region_info(VFIO_PCI_CONFIG_REGION_INDEX)?;
        let this = Self {
            pci_id: pci_id.into(),
            _container: container,
            _group: group,
            device: Arc::new(device),
            msix_info,
            config_space,
            driver_source: driver_source.clone(),
            interrupts: Vec::new(),
            dma_clients,
        };

        tracing::debug!(pci_id, "enabling device...");
        // Ensure bus master enable and memory space enable are set, and that
        // INTx is disabled.
        this.enable_device()
            .with_context(|| format!("failed to enable device {pci_id}"))?;
        Ok(this)
    }

    fn enable_device(&self) -> anyhow::Result<()> {
        let offset = pci_core::spec::cfg_space::HeaderType00::STATUS_COMMAND.0;
        let status_command = self.read_config(offset)?;
        let command = pci_core::spec::cfg_space::Command::from(status_command as u16);

        let command = command
            .with_bus_master(true)
            .with_intx_disable(true)
            .with_mmio_enabled(true);

        let status_command = (status_command & 0xffff0000) | u16::from(command) as u32;
        self.write_config(offset, status_command)?;
        Ok(())
    }

    pub fn read_config(&self, offset: u16) -> anyhow::Result<u32> {
        if offset as u64 > self.config_space.size - 4 {
            anyhow::bail!("invalid config offset");
        }

        let mut buf = [0u8; 4];
        self.device
            .as_ref()
            .as_ref()
            .read_at(&mut buf, self.config_space.offset + offset as u64)
            .context("failed to read config")?;

        Ok(u32::from_ne_bytes(buf))
    }

    pub fn write_config(&self, offset: u16, data: u32) -> anyhow::Result<()> {
        if offset as u64 > self.config_space.size - 4 {
            anyhow::bail!("invalid config offset");
        }

        tracing::trace!(pci_id = ?self.pci_id, offset, data, "writing config");
        let buf = data.to_ne_bytes();
        self.device
            .as_ref()
            .as_ref()
            .write_at(&buf, self.config_space.offset + offset as u64)
            .context("failed to write config")?;

        Ok(())
    }

    /// Maps PCI BAR[n] to VA space.
    fn map_bar(&self, n: u8) -> anyhow::Result<MappedRegionWithFallback> {
        if n >= 6 {
            anyhow::bail!("invalid bar");
        }
        let info = self.device.region_info(n.into())?;
        let mapping = self.device.map(info.offset, info.size as usize, true)?;
        trycopy::initialize_try_copy();
        Ok(MappedRegionWithFallback {
            device: self.device.clone(),
            mapping,
            len: info.size as usize,
            offset: info.offset,
            read_fallback: SharedCounter::new(),
            write_fallback: SharedCounter::new(),
        })
    }
}

struct VfioDelayWorkaroundConfig {
    driver: VmTaskDriverSource,
    enodev_sleep_duration: Duration,
}

impl VfioDelayWorkaroundConfig {
    const DEFAULT_ENODEV_SLEEP_DURATION: Duration = Duration::from_millis(250);
    pub fn new(driver: VmTaskDriverSource, enodev_sleep_duration: Option<Duration>) -> Self {
        Self {
            driver,
            enodev_sleep_duration: enodev_sleep_duration
                .unwrap_or(Self::DEFAULT_ENODEV_SLEEP_DURATION),
        }
    }

    pub async fn sleep(&self) {
        pal_async::timer::PolledTimer::new(&self.driver.simple())
            .sleep(self.enodev_sleep_duration)
            .await;
    }

    // There is a small race window in the 6.1 kernel between when the
    // vfio device is visible to userspace, and when it is added to its
    // internal list. Try one more time on ENODEV failure after a brief
    // sleep.
    pub async fn retry_on_enodev<F, T>(&self, op_name: &str, mut f: F) -> anyhow::Result<T>
    where
        F: AsyncFnMut(bool) -> anyhow::Result<T>,
    {
        match f(false).await {
            Err(e) if VfioErrata::from_error(&e) == VfioErrata::DelayWorkaround => {
                tracing::warn!("vfio operation {op_name} got ENODEV, retrying after delay");
                self.sleep().await;
                f(true).await
            }
            r => r,
        }
    }

    pub async fn retry_unconditional<F, T>(&self, op_name: &str, mut f: F) -> anyhow::Result<T>
    where
        F: AsyncFnMut(bool) -> anyhow::Result<T>,
    {
        match f(false).await {
            Err(e) => {
                tracing::warn!("vfio operation {op_name} failed with {e}, retrying after delay");
                self.sleep().await;
                f(true).await
            }
            r => r,
        }
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
    len: usize,
    read_fallback: SharedCounter,
    write_fallback: SharedCounter,
}

impl DeviceBacking for VfioDevice {
    type Registers = MappedRegionWithFallback;

    fn id(&self) -> &str {
        &self.pci_id
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        (*self).map_bar(n)
    }

    fn dma_client(&self) -> Arc<dyn DmaClient> {
        // Default to the only present client, or if both are available default to the
        // persistent client.
        match &self.dma_clients {
            VfioDmaClients::EphemeralOnly(client) => client.clone(),
            VfioDmaClients::PersistentOnly(client) => client.clone(),
            VfioDmaClients::Split {
                persistent,
                ephemeral: _,
            } => persistent.clone(),
        }
    }

    fn dma_client_for(&self, pool: crate::DmaPool) -> anyhow::Result<Arc<dyn DmaClient>> {
        match &self.dma_clients {
            VfioDmaClients::PersistentOnly(client) => match pool {
                crate::DmaPool::Persistent => Ok(client.clone()),
                crate::DmaPool::Ephemeral => {
                    anyhow::bail!(
                        "ephemeral dma pool requested but only persistent client available"
                    )
                }
            },
            VfioDmaClients::EphemeralOnly(client) => match pool {
                crate::DmaPool::Ephemeral => Ok(client.clone()),
                crate::DmaPool::Persistent => {
                    anyhow::bail!(
                        "persistent dma pool requested but only ephemeral client available"
                    )
                }
            },
            VfioDmaClients::Split {
                persistent,
                ephemeral,
            } => match pool {
                crate::DmaPool::Persistent => Ok(persistent.clone()),
                crate::DmaPool::Ephemeral => Ok(ephemeral.clone()),
            },
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

    fn unmap_all_interrupts(&mut self) -> anyhow::Result<()> {
        if self.interrupts.is_empty() {
            return Ok(());
        }

        let count = self.interrupts.len() as u32;
        self.device
            .unmap_msix(0, count)
            .context("failed to unmap all msix vectors")?;

        // Clear local bookkeeping so re-mapping works correctly later.
        self.interrupts.clear();

        Ok(())
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
    fn len(&self) -> usize {
        self.len()
    }

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
        assert!(
            offset <= self.mapping.len() - size_of::<T>() && offset.is_multiple_of(align_of::<T>())
        );
        if cfg!(feature = "mmio_simulate_fallback") {
            return std::ptr::NonNull::dangling().as_ptr();
        }
        // SAFETY: the offset is validated to be in bounds.
        unsafe { self.mapping.as_ptr().byte_add(offset).cast() }
    }

    fn read_from_mapping<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
    ) -> Result<T, trycopy::MemoryError> {
        // SAFETY: the offset is validated to be in bounds and aligned.
        unsafe { trycopy::try_read_volatile(self.mapping::<T>(offset)) }
    }

    fn write_to_mapping<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
        data: T,
    ) -> Result<(), trycopy::MemoryError> {
        // SAFETY: the offset is validated to be in bounds and aligned.
        unsafe { trycopy::try_write_volatile(self.mapping::<T>(offset), &data) }
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
    fn len(&self) -> usize {
        self.len
    }

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
