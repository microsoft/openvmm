// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux kernel `vhost_vsock` backend.
//!
//! The virtio transport remains in OpenVMM, while `/dev/vhost-vsock` processes
//! the receive and transmit virtqueues. This exposes the guest directly in the
//! host's `AF_VSOCK` namespace.
//!
//! This backend requires file-backed guest RAM and identity-mapped DMA
//! addresses. It does not configure a vhost IOTLB for a virtual IOMMU.

use crate::spec::VsockConfig;
use anyhow::Context as _;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_event::Event;
use sparse_mmap::SparseMapping;
use std::os::fd::AsFd as _;
use std::os::fd::AsRawFd as _;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use virtio::DeviceTraits;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::VirtioDeviceType;
use vmcore::interrupt::EventProxy;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const QUEUE_COUNT: usize = 3;
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const EVENT_QUEUE: u16 = 2;

// Dirty logging is a vhost control feature, not a guest virtio feature.
const VHOST_F_LOG_ALL: u64 = 1 << 26;
// OpenVMM only exposes modern virtio devices.
const VIRTIO_F_ANY_LAYOUT: u64 = 1 << 27;
// OpenVMM's transport exposes ACCESS_PLATFORM, but this backend uses the
// direct GPA-to-HVA memory table and does not configure a vhost IOTLB.
const VIRTIO_F_ACCESS_PLATFORM: u64 = 1 << 33;
const MASKED_FEATURES: u64 = VHOST_F_LOG_ALL | VIRTIO_F_ANY_LAYOUT | VIRTIO_F_ACCESS_PLATFORM;

#[repr(C)]
#[derive(Copy, Clone)]
struct VhostVringState {
    index: u32,
    num: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct VhostVringAddr {
    index: u32,
    flags: u32,
    desc_user_addr: u64,
    used_user_addr: u64,
    avail_user_addr: u64,
    log_guest_addr: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct VhostVringFile {
    index: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout)]
struct VhostMemoryHeader {
    nregions: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout)]
struct VhostMemoryRegion {
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
    flags_padding: u64,
}

mod ioctl {
    use super::VhostMemoryHeader;
    use super::VhostVringAddr;
    use super::VhostVringFile;
    use super::VhostVringState;

    const VHOST_VIRTIO: u8 = 0xaf;

    nix::ioctl_read!(get_features, VHOST_VIRTIO, 0x00, u64);
    nix::ioctl_write_ptr!(set_features, VHOST_VIRTIO, 0x00, u64);
    nix::ioctl_none!(set_owner, VHOST_VIRTIO, 0x01);
    nix::ioctl_write_ptr!(set_mem_table, VHOST_VIRTIO, 0x03, VhostMemoryHeader);
    nix::ioctl_write_ptr!(set_vring_num, VHOST_VIRTIO, 0x10, VhostVringState);
    nix::ioctl_write_ptr!(set_vring_addr, VHOST_VIRTIO, 0x11, VhostVringAddr);
    nix::ioctl_write_ptr!(set_vring_base, VHOST_VIRTIO, 0x12, VhostVringState);
    nix::ioctl_readwrite!(get_vring_base, VHOST_VIRTIO, 0x12, VhostVringState);
    nix::ioctl_write_ptr!(set_vring_kick, VHOST_VIRTIO, 0x20, VhostVringFile);
    nix::ioctl_write_ptr!(set_vring_call, VHOST_VIRTIO, 0x21, VhostVringFile);
    nix::ioctl_write_ptr!(set_guest_cid, VHOST_VIRTIO, 0x60, u64);
    nix::ioctl_write_ptr!(set_running, VHOST_VIRTIO, 0x61, i32);
}

struct VhostBackend {
    fd: OwnedFd,
}

impl VhostBackend {
    fn new(fd: OwnedFd, guest_cid: u32) -> anyhow::Result<(Self, u64)> {
        validate_guest_cid(guest_cid)?;

        let backend = Self { fd };

        // SAFETY: The fd refers to a vhost-vsock device and this ioctl has no argument.
        unsafe { ioctl::set_owner(backend.raw_fd()) }.context("VHOST_SET_OWNER failed")?;

        let cid = u64::from(guest_cid);
        // SAFETY: The ioctl copies a u64 from the supplied pointer.
        unsafe { ioctl::set_guest_cid(backend.raw_fd(), &cid) }
            .context("VHOST_VSOCK_SET_GUEST_CID failed")?;

        let mut features = 0;
        // SAFETY: The ioctl writes a u64 to the supplied pointer.
        unsafe { ioctl::get_features(backend.raw_fd(), &mut features) }
            .context("VHOST_GET_FEATURES failed")?;

        Ok((backend, features & !MASKED_FEATURES))
    }

    fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn set_features(&self, features: u64) -> anyhow::Result<()> {
        // SAFETY: The ioctl copies a u64 from the supplied pointer.
        unsafe { ioctl::set_features(self.raw_fd(), &features) }
            .context("VHOST_SET_FEATURES failed")?;
        Ok(())
    }

    fn set_memory(&self, memory: &VhostMemory) -> anyhow::Result<()> {
        let nregions =
            u32::try_from(memory.regions.len()).context("too many vhost memory regions")?;
        let mut table = Vec::with_capacity(
            size_of::<VhostMemoryHeader>() + memory.regions.len() * size_of::<VhostMemoryRegion>(),
        );
        table.extend_from_slice(
            VhostMemoryHeader {
                nregions,
                padding: 0,
            }
            .as_bytes(),
        );
        for region in &memory.regions {
            table.extend_from_slice(
                VhostMemoryRegion {
                    guest_phys_addr: region.guest_address,
                    memory_size: region.len(),
                    userspace_addr: region.host_address(),
                    flags_padding: 0,
                }
                .as_bytes(),
            );
        }

        // SAFETY: `table` contains a vhost_memory header followed by exactly
        // `nregions` vhost_memory_region entries, and the kernel copies it
        // during the ioctl.
        unsafe { ioctl::set_mem_table(self.raw_fd(), table.as_ptr().cast::<VhostMemoryHeader>()) }
            .context("VHOST_SET_MEM_TABLE failed")?;
        Ok(())
    }

    fn set_vring_num(&self, index: u16, size: u16) -> anyhow::Result<()> {
        let state = VhostVringState {
            index: u32::from(index),
            num: u32::from(size),
        };
        // SAFETY: The ioctl copies a VhostVringState from the supplied pointer.
        unsafe { ioctl::set_vring_num(self.raw_fd(), &state) }
            .with_context(|| format!("VHOST_SET_VRING_NUM failed for queue {index}"))?;
        Ok(())
    }

    fn set_vring_base(&self, index: u16, base: u32) -> anyhow::Result<()> {
        let state = VhostVringState {
            index: u32::from(index),
            num: base,
        };
        // SAFETY: The ioctl copies a VhostVringState from the supplied pointer.
        unsafe { ioctl::set_vring_base(self.raw_fd(), &state) }
            .with_context(|| format!("VHOST_SET_VRING_BASE failed for queue {index}"))?;
        Ok(())
    }

    fn get_vring_base(&self, index: u16) -> anyhow::Result<u32> {
        let mut state = VhostVringState {
            index: u32::from(index),
            num: 0,
        };
        // SAFETY: The ioctl reads and writes a VhostVringState through the
        // supplied pointer.
        unsafe { ioctl::get_vring_base(self.raw_fd(), &mut state) }
            .with_context(|| format!("VHOST_GET_VRING_BASE failed for queue {index}"))?;
        Ok(state.num)
    }

    fn set_vring_addr(&self, index: u16, desc: u64, used: u64, avail: u64) -> anyhow::Result<()> {
        let addr = VhostVringAddr {
            index: u32::from(index),
            flags: 0,
            desc_user_addr: desc,
            used_user_addr: used,
            avail_user_addr: avail,
            log_guest_addr: 0,
        };
        // SAFETY: The ioctl copies a VhostVringAddr from the supplied pointer.
        unsafe { ioctl::set_vring_addr(self.raw_fd(), &addr) }
            .with_context(|| format!("VHOST_SET_VRING_ADDR failed for queue {index}"))?;
        Ok(())
    }

    fn set_vring_kick(&self, index: u16, fd: RawFd) -> anyhow::Result<()> {
        let file = VhostVringFile {
            index: u32::from(index),
            fd,
        };
        // SAFETY: The ioctl copies a VhostVringFile from the supplied pointer.
        unsafe { ioctl::set_vring_kick(self.raw_fd(), &file) }
            .with_context(|| format!("VHOST_SET_VRING_KICK failed for queue {index}"))?;
        Ok(())
    }

    fn set_vring_call(&self, index: u16, fd: RawFd) -> anyhow::Result<()> {
        let file = VhostVringFile {
            index: u32::from(index),
            fd,
        };
        // SAFETY: The ioctl copies a VhostVringFile from the supplied pointer.
        unsafe { ioctl::set_vring_call(self.raw_fd(), &file) }
            .with_context(|| format!("VHOST_SET_VRING_CALL failed for queue {index}"))?;
        Ok(())
    }

    fn unbind_vring_events(&self, index: u16) {
        if let Err(error) = self.set_vring_kick(index, -1) {
            tracelimit::warn_ratelimited!(
                error = &*error as &dyn std::error::Error,
                index,
                "failed to unbind vhost-vsock kick event"
            );
        }
        if let Err(error) = self.set_vring_call(index, -1) {
            tracelimit::warn_ratelimited!(
                error = &*error as &dyn std::error::Error,
                index,
                "failed to unbind vhost-vsock call event"
            );
        }
    }

    fn set_running(&self, running: bool) -> anyhow::Result<()> {
        let running = i32::from(running);
        // SAFETY: The ioctl copies an i32 from the supplied pointer.
        unsafe { ioctl::set_running(self.raw_fd(), &running) }
            .with_context(|| format!("VHOST_VSOCK_SET_RUNNING({running}) failed"))?;
        Ok(())
    }
}

struct MappedRegion {
    guest_address: u64,
    mapping: SparseMapping,
}

impl MappedRegion {
    fn len(&self) -> u64 {
        self.mapping.len() as u64
    }

    fn host_address(&self) -> u64 {
        self.mapping.as_ptr() as usize as u64
    }

    fn contains(&self, gpa: u64, len: u64) -> bool {
        gpa.checked_sub(self.guest_address)
            .and_then(|offset| offset.checked_add(len))
            .is_some_and(|end| end <= self.len())
    }

    fn translate(&self, gpa: u64) -> anyhow::Result<u64> {
        let offset = gpa
            .checked_sub(self.guest_address)
            .context("guest physical address precedes mapped region")?;
        anyhow::ensure!(
            offset < self.len(),
            "guest physical address is outside mapped region"
        );
        self.host_address()
            .checked_add(offset)
            .context("host virtual address overflow")
    }

    fn read_u16(&self, gpa: u64) -> anyhow::Result<u16> {
        let offset = gpa
            .checked_sub(self.guest_address)
            .context("guest physical address precedes mapped region")?;
        anyhow::ensure!(
            offset
                .checked_add(size_of::<u16>() as u64)
                .is_some_and(|end| end <= self.len()),
            "u16 read crosses mapped region boundary"
        );
        let mut bytes = [0; 2];
        self.mapping
            .read_at(offset as usize, &mut bytes)
            .context("failed to read mapped guest memory")?;
        Ok(u16::from_le_bytes(bytes))
    }
}

struct VhostMemory {
    // Separate mappings make every RAM region eagerly accessible to the kernel;
    // the GuestMemory mapping itself may be populated lazily.
    regions: Vec<MappedRegion>,
}

impl VhostMemory {
    async fn new(guest_memory: &GuestMemory) -> anyhow::Result<Self> {
        let sharing = guest_memory
            .sharing()
            .context("kernel vhost-vsock requires shared file-backed guest memory")?;
        let mut shared_regions = sharing
            .get_regions()
            .await
            .map_err(anyhow::Error::from_boxed)
            .context("failed to query shareable guest-memory regions")?;
        anyhow::ensure!(
            !shared_regions.is_empty(),
            "guest memory has no shareable RAM regions"
        );
        shared_regions.sort_by_key(|region| region.guest_address);

        let page_size = SparseMapping::page_size() as u64;
        let mut previous_end = 0;
        let mut regions = Vec::with_capacity(shared_regions.len());
        for region in shared_regions {
            anyhow::ensure!(region.size != 0, "guest-memory region is empty");
            let end = region
                .guest_address
                .checked_add(region.size)
                .context("guest-memory region overflows the GPA address space")?;
            anyhow::ensure!(
                region.guest_address >= previous_end,
                "guest-memory regions overlap"
            );
            anyhow::ensure!(
                region.guest_address.is_multiple_of(page_size)
                    && region.size.is_multiple_of(page_size)
                    && region.file_offset.is_multiple_of(page_size),
                "guest-memory region is not page aligned"
            );

            let len = usize::try_from(region.size)
                .context("guest-memory region does not fit the host address space")?;
            let mapping = SparseMapping::new(len).with_context(|| {
                format!(
                    "failed to reserve mapping for GPA {:#x}",
                    region.guest_address
                )
            })?;
            mapping
                .map_file(0, len, region.file.as_ref(), region.file_offset, true)
                .with_context(|| {
                    format!(
                        "failed to map guest-memory region {:#x}..{end:#x}",
                        region.guest_address
                    )
                })?;
            regions.push(MappedRegion {
                guest_address: region.guest_address,
                mapping,
            });
            previous_end = end;
        }

        Ok(Self { regions })
    }

    fn translate(&self, gpa: u64) -> anyhow::Result<u64> {
        let region = self
            .regions
            .iter()
            .find(|region| region.contains(gpa, 1))
            .with_context(|| format!("guest physical address {gpa:#x} is not in RAM"))?;
        region.translate(gpa)
    }

    fn read_used_index(&self, params: &QueueParams) -> anyhow::Result<u16> {
        let gpa = params
            .used_addr
            .checked_add(2)
            .context("used ring index address overflow")?;
        let region = self
            .regions
            .iter()
            .find(|region| region.contains(gpa, size_of::<u16>() as u64))
            .with_context(|| format!("used ring index address {gpa:#x} is not in RAM"))?;
        region.read_u16(gpa)
    }
}

struct QueueRuntime {
    params: QueueParams,
    _kick: Option<Event>,
    _call: Option<Event>,
    _proxy: Option<EventProxy>,
}

impl QueueRuntime {
    fn event_queue(params: QueueParams) -> Self {
        Self {
            params,
            _kick: None,
            _call: None,
            _proxy: None,
        }
    }
}

/// A virtio-vsock device whose data path is handled by Linux `vhost_vsock`.
#[derive(InspectMut)]
pub struct VhostVsockDevice {
    guest_cid: u32,
    #[inspect(skip)]
    driver: VmTaskDriver,
    #[inspect(skip)]
    backend: VhostBackend,
    #[inspect(hex)]
    kernel_features: u64,
    #[inspect(skip)]
    memory: Option<VhostMemory>,
    #[inspect(skip)]
    queues: [Option<QueueRuntime>; QUEUE_COUNT],
    negotiated_features: VirtioDeviceFeatures,
    features_set: bool,
    running: bool,
}

impl VhostVsockDevice {
    /// Takes ownership of an open `/dev/vhost-vsock` fd and assigns `guest_cid`.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        vhost: OwnedFd,
        guest_cid: u32,
    ) -> anyhow::Result<Self> {
        let (backend, kernel_features) = VhostBackend::new(vhost, guest_cid)?;
        Ok(Self {
            guest_cid,
            driver: driver_source.simple(),
            backend,
            kernel_features,
            memory: None,
            queues: [const { None }; QUEUE_COUNT],
            negotiated_features: VirtioDeviceFeatures::new(),
            features_set: false,
            running: false,
        })
    }

    async fn prepare_backend(
        &mut self,
        guest_memory: &GuestMemory,
        features: &VirtioDeviceFeatures,
    ) -> anyhow::Result<()> {
        if self.memory.is_none() {
            let memory = VhostMemory::new(guest_memory).await?;
            self.backend.set_memory(&memory)?;
            self.memory = Some(memory);
        }

        let negotiated =
            VirtioDeviceFeatures::from_bits(features.into_bits() & self.kernel_features);
        anyhow::ensure!(
            negotiated.version_1(),
            "the host vhost-vsock backend does not support modern virtio"
        );
        anyhow::ensure!(
            !features.ring_packed() || negotiated.ring_packed(),
            "the host vhost-vsock backend does not support packed queues"
        );

        if self.features_set {
            anyhow::ensure!(
                negotiated.into_bits() == self.negotiated_features.into_bits(),
                "virtio features changed while vhost-vsock queues were active"
            );
        } else {
            self.backend.set_features(negotiated.into_bits())?;
            self.negotiated_features = negotiated;
            self.features_set = true;
        }
        Ok(())
    }

    fn memory(&self) -> &VhostMemory {
        self.memory
            .as_ref()
            .expect("memory is configured before data queues")
    }

    fn configure_data_queue(
        &self,
        index: u16,
        resources: QueueResources,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<QueueRuntime> {
        let params = resources.params;
        if !self.negotiated_features.ring_packed() {
            // VHOST_GET_VRING_BASE only returns the available index for split
            // queues, so stop_queue must read the used index from guest memory.
            // Validate the location while the guest-provided queue parameters
            // can still be rejected. Guest-memory mappings are static after
            // this point, so a later read failure is an internal invariant
            // violation.
            self.memory()
                .read_used_index(&params)
                .context("split used ring index is not readable")?;
        }
        self.backend.set_vring_num(index, params.size)?;
        self.backend.set_vring_base(
            index,
            encode_vring_base(self.negotiated_features, initial_state),
        )?;
        self.backend.set_vring_addr(
            index,
            self.memory().translate(params.desc_addr)?,
            self.memory().translate(params.used_addr)?,
            self.memory().translate(params.avail_addr)?,
        )?;

        let kick = resources.event;
        self.backend
            .set_vring_kick(index, kick.as_fd().as_raw_fd())?;

        let (call, proxy) = match resources.notify.event_or_proxy(&self.driver) {
            Ok(value) => value,
            Err(error) => {
                self.backend.unbind_vring_events(index);
                return Err(error).context("failed to obtain a virtio interrupt event");
            }
        };
        if let Err(error) = self.backend.set_vring_call(index, call.as_fd().as_raw_fd()) {
            self.backend.unbind_vring_events(index);
            return Err(error);
        }

        Ok(QueueRuntime {
            params,
            _kick: Some(kick),
            _call: Some(call),
            _proxy: proxy,
        })
    }

    fn stop_running(&mut self) {
        if !self.running {
            return;
        }
        if let Err(error) = self.backend.set_running(false) {
            tracelimit::warn_ratelimited!(
                error = &*error as &dyn std::error::Error,
                "failed to stop vhost-vsock"
            );
        }
        self.running = false;
    }
}

impl VirtioDevice for VhostVsockDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VirtioDeviceType::VSOCK,
            device_features: VirtioDeviceFeatures::from_bits(self.kernel_features),
            max_queues: QUEUE_COUNT as u16,
            device_register_length: size_of::<VsockConfig>() as u32,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        let config = VsockConfig {
            guest_cid: u64::from(self.guest_cid).to_le(),
        };
        let bytes = config.as_bytes();
        let offset = usize::from(offset);
        if offset + 4 > bytes.len() {
            return 0;
        }
        u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracelimit::warn_ratelimited!(offset, val, "vhost-vsock: unexpected config write");
    }

    async fn start_queue(
        &mut self,
        index: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        let queue_index = usize::from(index);
        anyhow::ensure!(queue_index < QUEUE_COUNT, "invalid queue index {index}");
        anyhow::ensure!(
            self.queues[queue_index].is_none(),
            "virtio queue {index} is already started"
        );

        if index == EVENT_QUEUE {
            self.queues[queue_index] = Some(QueueRuntime::event_queue(resources.params));
            return Ok(());
        }

        self.prepare_backend(&resources.guest_memory, features)
            .await?;
        let runtime = self.configure_data_queue(index, resources, initial_state)?;
        self.queues[queue_index] = Some(runtime);

        if self.queues[RX_QUEUE as usize].is_some()
            && self.queues[TX_QUEUE as usize].is_some()
            && !self.running
        {
            if let Err(error) = self.backend.set_running(true) {
                for data_queue in [RX_QUEUE, TX_QUEUE] {
                    if self.queues[data_queue as usize].take().is_some() {
                        let _ = self.backend.get_vring_base(data_queue);
                        self.backend.unbind_vring_events(data_queue);
                    }
                }
                return Err(error);
            }
            self.running = true;
        }

        Ok(())
    }

    async fn stop_queue(&mut self, index: u16) -> Option<QueueState> {
        let queue_index = usize::from(index);
        let runtime = self.queues.get_mut(queue_index)?.take()?;
        if index == EVENT_QUEUE {
            return Some(QueueState::default());
        }

        self.stop_running();
        let base = self.backend.get_vring_base(index);
        self.backend.unbind_vring_events(index);

        // TODO: Remove once the trait shape supports it
        drop(runtime._kick);
        drop(runtime._call);
        drop(runtime._proxy);

        match base {
            Ok(base) => {
                let split_used_index = if self.negotiated_features.ring_packed() {
                    0
                } else {
                    self.memory()
                        .read_used_index(&runtime.params)
                        .unwrap_or_else(|error| {
                            panic!("validated vhost-vsock used index became unreadable: {error:#}")
                        })
                };
                Some(decode_vring_base(
                    self.negotiated_features,
                    base,
                    split_used_index,
                ))
            }
            Err(error) => {
                tracelimit::warn_ratelimited!(
                    error = &*error as &dyn std::error::Error,
                    index,
                    "failed to stop vhost-vsock queue"
                );
                None
            }
        }
    }

    async fn reset(&mut self) {
        self.stop_running();
        for index in [RX_QUEUE, TX_QUEUE] {
            if self.queues[index as usize].is_some() {
                let _ = self.backend.get_vring_base(index);
                self.backend.unbind_vring_events(index);
            }
        }
        self.queues = [const { None }; QUEUE_COUNT];
        self.negotiated_features = VirtioDeviceFeatures::new();
        self.features_set = false;
    }
}

fn validate_guest_cid(guest_cid: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        guest_cid > 2 && guest_cid != u32::MAX,
        "vhost-vsock guest CID must be between 3 and {}",
        u32::MAX - 1
    );
    Ok(())
}

fn encode_vring_base(features: VirtioDeviceFeatures, state: Option<QueueState>) -> u32 {
    if features.ring_packed() {
        let avail = state.map_or(0x8000, |state| state.avail_index);
        let used = state.map_or(0x8000, |state| state.used_index);
        u32::from(avail) | (u32::from(used) << 16)
    } else {
        u32::from(state.map_or(0, |state| state.avail_index))
    }
}

fn decode_vring_base(
    features: VirtioDeviceFeatures,
    base: u32,
    split_used_index: u16,
) -> QueueState {
    if features.ring_packed() {
        QueueState {
            avail_index: base as u16,
            used_index: (base >> 16) as u16,
        }
    } else {
        QueueState {
            avail_index: base as u16,
            used_index: split_used_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guestmem::GuestMemorySharing;
    use guestmem::ProvideShareableRegions;
    use guestmem::ShareableRegion;
    use guestmem::ShareableRegionError;
    use pal_async::async_test;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use test_with_tracing::test;

    struct ShareableGuestMemory {
        mapping: SparseMapping,
        file: Arc<sparse_mmap::Mappable>,
        size: u64,
    }

    impl ShareableGuestMemory {
        fn new(size: usize) -> Self {
            let file = sparse_mmap::alloc_shared_memory(size, "vhost-vsock-test").unwrap();
            let mapping = SparseMapping::new(size).unwrap();
            mapping.map_file(0, size, &file, 0, true).unwrap();
            Self {
                mapping,
                file: Arc::new(file),
                size: size as u64,
            }
        }
    }

    // SAFETY: `mapping` is stable for the lifetime of this object and `file`
    // refers to the same fully committed shared backing.
    unsafe impl guestmem::GuestMemoryAccess for ShareableGuestMemory {
        fn mapping(&self) -> Option<NonNull<u8>> {
            NonNull::new(self.mapping.as_ptr().cast())
        }

        fn max_address(&self) -> u64 {
            self.size
        }

        fn sharing(&self) -> Option<GuestMemorySharing> {
            Some(GuestMemorySharing::new(TestRegionProvider {
                file: self.file.clone(),
                size: self.size,
            }))
        }
    }

    struct TestRegionProvider {
        file: Arc<sparse_mmap::Mappable>,
        size: u64,
    }

    impl ProvideShareableRegions for TestRegionProvider {
        async fn get_regions(&self) -> Result<Vec<ShareableRegion>, ShareableRegionError> {
            Ok(vec![ShareableRegion {
                guest_address: 0,
                size: self.size,
                file: self.file.clone(),
                file_offset: 0,
            }])
        }
    }

    #[test]
    fn validates_guest_cids() {
        assert!(validate_guest_cid(2).is_err());
        assert!(validate_guest_cid(3).is_ok());
        assert!(validate_guest_cid(u32::MAX - 1).is_ok());
        assert!(validate_guest_cid(u32::MAX).is_err());
    }

    #[test]
    fn encodes_queue_state() {
        let split = VirtioDeviceFeatures::new();
        let state = QueueState {
            avail_index: 7,
            used_index: 5,
        };
        assert_eq!(encode_vring_base(split, Some(state)), 7);
        assert_eq!(
            decode_vring_base(split, 7, 5),
            QueueState {
                avail_index: 7,
                used_index: 5,
            }
        );

        let packed = VirtioDeviceFeatures::new().with_ring_packed(true);
        assert_eq!(encode_vring_base(packed, None), 0x8000_8000);
        assert_eq!(
            decode_vring_base(packed, 0x8005_8007, 0),
            QueueState {
                avail_index: 0x8007,
                used_index: 0x8005,
            }
        );
    }

    #[async_test]
    async fn maps_shareable_guest_memory() {
        let guest_memory = GuestMemory::new("vhost-vsock-test", ShareableGuestMemory::new(0x2000));
        guest_memory.write_at(0x102, &[0x34, 0x12]).unwrap();

        let memory = VhostMemory::new(&guest_memory).await.unwrap();
        assert_eq!(memory.regions.len(), 1);
        assert_eq!(
            memory
                .read_used_index(&QueueParams {
                    used_addr: 0x100,
                    ..Default::default()
                })
                .unwrap(),
            0x1234
        );
        assert!(
            memory
                .read_used_index(&QueueParams {
                    used_addr: 0x1ffe,
                    ..Default::default()
                })
                .is_err()
        );
        assert_eq!(
            memory.translate(0x123).unwrap(),
            memory.regions[0].host_address() + 0x123
        );
        assert!(memory.translate(0x2000).is_err());
    }
}
