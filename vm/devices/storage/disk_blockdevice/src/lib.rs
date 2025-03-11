// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(target_os = "linux")]

//! Implements the [`DiskIo`] trait for virtual disks backed by a raw block
//! device.

// UNSAFETY: Issuing IOs and calling ioctls.
#![expect(unsafe_code)]

mod ioctl;
mod nvme;

use anyhow::Context;
use async_trait::async_trait;
use blocking::unblock;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use disk_backend::pr::PersistentReservation;
use disk_backend::pr::ReservationCapabilities;
use disk_backend::pr::ReservationReport;
use disk_backend::pr::ReservationType;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use fs_err::PathExt;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use io_uring::opcode;
use io_uring::types;
use io_uring::types::RwFlags;
use mesh::MeshPayload;
use nvme::check_nvme_status;
use nvme_spec::nvm;
use pal::unix::affinity;
use pal_uring::Initiate;
use pal_uring::IoInitiator;
use scsi_buffers::BounceBufferTracker;
use scsi_buffers::RequestBuffers;
use std::fmt::Debug;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::FileTypeExt;
use std::os::unix::prelude::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use uevent::CallbackHandle;
use uevent::UeventListener;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;

pub struct BlockDeviceResolver {
    uring: Arc<dyn Initiate>,
    uevent_listener: Option<Arc<UeventListener>>,
    bounce_buffer_tracker: Arc<BounceBufferTracker>,
    always_bounce: bool,
}

impl BlockDeviceResolver {
    pub fn new(
        uring: Arc<dyn Initiate>,
        uevent_listener: Option<Arc<UeventListener>>,
        bounce_buffer_tracker: Arc<BounceBufferTracker>,
        always_bounce: bool,
    ) -> Self {
        Self {
            uring,
            uevent_listener,
            bounce_buffer_tracker,
            always_bounce,
        }
    }
}

#[derive(MeshPayload)]
pub struct OpenBlockDeviceConfig {
    pub file: fs::File,
}

impl ResourceId<DiskHandleKind> for OpenBlockDeviceConfig {
    const ID: &'static str = "block";
}

#[derive(Debug, Error)]
pub enum ResolveDiskError {
    #[error("failed to create new device")]
    NewDevice(#[source] NewDeviceError),
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, OpenBlockDeviceConfig> for BlockDeviceResolver {
    type Output = ResolvedDisk;
    type Error = ResolveDiskError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: OpenBlockDeviceConfig,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = BlockDevice::new(
            rsrc.file,
            input.read_only,
            self.uring.clone(),
            self.uevent_listener.as_deref(),
            self.bounce_buffer_tracker.clone(),
            self.always_bounce,
        )
        .await
        .map_err(ResolveDiskError::NewDevice)?;
        ResolvedDisk::new(disk).map_err(ResolveDiskError::InvalidDisk)
    }
}

/// Opens a file for use with [`BlockDevice`] or [`OpenBlockDeviceConfig`].
pub fn open_file_for_block(path: &Path, read_only: bool) -> std::io::Result<fs::File> {
    use std::os::unix::prelude::*;

    tracing::debug!(?path, read_only, "open_file_for_block");
    fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .custom_flags(libc::O_DIRECT)
        .open(path)
}

/// A storvsp disk backed by a raw block device.
#[derive(Inspect)]
#[inspect(extra = "BlockDevice::inspect_extra")]
pub struct BlockDevice {
    file: Arc<fs::File>,
    sector_size: u32,
    physical_sector_size: u32,
    sector_shift: u32,
    sector_count: AtomicU64,
    optimal_unmap_sectors: u32,
    read_only: bool,
    #[inspect(skip)]
    uring: Arc<dyn Initiate>,
    #[inspect(flatten)]
    device_type: DeviceType,
    supports_pr: bool,
    supports_fua: bool,
    #[inspect(skip)]
    _uevent_filter: Option<CallbackHandle>,
    resize_epoch: Arc<ResizeEpoch>,
    resized_acked: AtomicU64,
    #[inspect(skip)]
    bounce_buffer_tracker: Arc<BounceBufferTracker>,
    always_bounce: bool,
}

#[derive(Inspect, Debug, Default)]
#[inspect(transparent)]
struct ResizeEpoch {
    epoch: AtomicU64,
    #[inspect(skip)]
    event: event_listener::Event,
}

#[derive(Debug, Copy, Clone, Inspect)]
#[inspect(tag = "device_type")]
enum DeviceType {
    File {
        sector_count: u64,
    },
    UnknownBlock,
    NVMe {
        ns_id: u32,
        rescap: nvm::ReservationCapabilities,
    },
}

impl BlockDevice {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        match self.device_type {
            DeviceType::NVMe { .. } => {
                resp.field_mut_with("interrupt_aggregation", |new_value| {
                    self.inspect_interrupt_coalescing(new_value)
                });
            }
            DeviceType::UnknownBlock => {}
            DeviceType::File { .. } => {}
        }
    }

    fn inspect_interrupt_coalescing(&self, new_value: Option<&str>) -> anyhow::Result<String> {
        let coalescing = if let Some(new_value) = new_value {
            let coalescing = (|| {
                let (threshold, time) = new_value.split_once(' ')?;
                Some(
                    nvme::InterruptCoalescing::new()
                        .with_aggregation_threshold(threshold.parse().ok()?)
                        .with_aggregation_time(time.parse().ok()?),
                )
            })()
            .context("expected `<aggregation_threshold> <aggregation_time>`")?;
            nvme::nvme_set_features_interrupt_coalescing(&self.file, coalescing)?;
            coalescing
        } else if let Ok(coalescing) = nvme::nvme_get_features_interrupt_coalescing(&self.file) {
            coalescing
        } else {
            return Ok("not supported".into());
        };
        Ok(format!(
            "{} {}",
            coalescing.aggregation_threshold(),
            coalescing.aggregation_time()
        ))
    }
}

/// New device error
#[derive(Debug, Error)]
pub enum NewDeviceError {
    #[error("block device ioctl error")]
    IoctlError(#[from] DiskError),
    #[error("failed to read device metadata")]
    DeviceMetadata(#[source] anyhow::Error),
    #[error("invalid file type, not a file or block device")]
    InvalidFileType,
    #[error("invalid disk size {0:#x}")]
    InvalidDiskSize(u64),
}

impl BlockDevice {
    /// Constructs a new `BlockDevice` backed by the specified file.
    ///
    /// # Arguments
    /// * `file` - The backing device opened for raw access.
    /// * `read_only` - Indicates whether the device is opened for read-only access.
    /// * `uring` - The IO uring to use for issuing IOs.
    /// * `always_bounce` - Whether to always use bounce buffers for IOs, even for those that are aligned.
    pub async fn new(
        file: fs::File,
        read_only: bool,
        uring: Arc<dyn Initiate>,
        uevent_listener: Option<&UeventListener>,
        bounce_buffer_tracker: Arc<BounceBufferTracker>,
        always_bounce: bool,
    ) -> Result<BlockDevice, NewDeviceError> {
        let initiator = uring.initiator();
        assert!(initiator.probe(opcode::Read::CODE));
        assert!(initiator.probe(opcode::Write::CODE));
        assert!(initiator.probe(opcode::Readv::CODE));
        assert!(initiator.probe(opcode::Writev::CODE));
        assert!(initiator.probe(opcode::Fsync::CODE));

        let metadata = file.metadata().map_err(DiskError::Io)?;

        let mut uevent_filter = None;
        let resize_epoch = Arc::new(ResizeEpoch::default());

        let devmeta = if metadata.file_type().is_block_device() {
            let rdev = metadata.rdev();
            // SAFETY: just parsing bits out of a u64.
            let (major, minor) = unsafe { (libc::major(rdev), libc::minor(rdev)) };

            // Register for resize events.
            if let Some(uevent_listener) = uevent_listener {
                let resize_epoch = resize_epoch.clone();
                uevent_filter = Some(
                    uevent_listener
                        .add_block_resize_callback(major, minor, {
                            move || {
                                tracing::info!(major, minor, "disk resized");
                                resize_epoch.epoch.fetch_add(1, Ordering::SeqCst);
                                resize_epoch.event.notify(usize::MAX);
                            }
                        })
                        .await,
                );
            }

            DeviceMetadata::from_block_device(&file, major, minor)
                .map_err(NewDeviceError::DeviceMetadata)?
        } else if metadata.file_type().is_file() {
            DeviceMetadata::from_file(&metadata).map_err(NewDeviceError::DeviceMetadata)?
        } else {
            return Err(NewDeviceError::InvalidFileType);
        };

        let sector_size = devmeta.logical_block_size;
        let sector_shift = sector_size.trailing_zeros();
        let physical_sector_size = devmeta.physical_block_size.max(sector_size);
        let sector_count = devmeta.disk_size >> sector_shift;
        let unmap_granularity = devmeta.discard_granularity >> sector_shift;
        let file = Arc::new(file);
        let device = BlockDevice {
            file,
            sector_size,
            physical_sector_size,
            sector_shift: sector_size.trailing_zeros(),
            sector_count: sector_count.into(),
            optimal_unmap_sectors: unmap_granularity,
            read_only,
            uring,
            device_type: devmeta.device_type,
            supports_pr: devmeta.supports_pr,
            supports_fua: devmeta.fua,
            _uevent_filter: uevent_filter,
            resize_epoch,
            resized_acked: 0.into(),
            bounce_buffer_tracker,
            always_bounce,
        };

        Ok(device)
    }

    fn initiator(&self) -> &IoInitiator {
        self.uring.initiator()
    }

    fn handle_resize(&self) {
        if let Err(err) = self.handle_resize_inner() {
            tracing::error!(
                error = &err as &dyn std::error::Error,
                "failed to update disk size"
            );
        }
    }

    fn handle_resize_inner(&self) -> std::io::Result<()> {
        let mut acked = self.resized_acked.load(Ordering::SeqCst);
        loop {
            let epoch = self.resize_epoch.epoch.load(Ordering::SeqCst);
            if acked == epoch {
                break Ok(());
            }

            let size_in_bytes = ioctl::query_block_device_size_in_bytes(&self.file)?;

            let new_sector_count = size_in_bytes / self.sector_size as u64;
            let original_sector_count = self.sector_count.load(Ordering::SeqCst);

            tracing::debug!(original_sector_count, new_sector_count, "resize");
            if original_sector_count != new_sector_count {
                tracing::info!(
                    original_sector_count,
                    new_sector_count,
                    "Disk size updating..."
                );
                self.sector_count.store(new_sector_count, Ordering::SeqCst);
            }

            acked = self
                .resized_acked
                .compare_exchange(acked, epoch, Ordering::SeqCst, Ordering::SeqCst)
                .unwrap_or_else(|x| x);
        }
    }

    fn map_io_error(&self, err: std::io::Error) -> DiskError {
        if !matches!(self.device_type, DeviceType::File { .. }) {
            match err.raw_os_error() {
                Some(libc::EBADE) => return DiskError::ReservationConflict,
                Some(libc::ENOSPC) => return DiskError::IllegalBlock,
                _ => {}
            }
        }
        DiskError::Io(err)
    }
}

struct DeviceMetadata {
    device_type: DeviceType,
    disk_size: u64,
    logical_block_size: u32,
    physical_block_size: u32,
    discard_granularity: u32,
    supports_pr: bool,
    fua: bool,
}

impl DeviceMetadata {
    fn from_block_device(file: &fs::File, major: u32, minor: u32) -> anyhow::Result<Self> {
        // Ensure the sysfs path exists.
        let devpath = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
        devpath
            .fs_err_metadata()
            .context("could not find sysfs path for block device")?;

        let mut supports_pr = false;

        // Check for NVMe by looking for the namespace ID.
        let device_type = match fs_err::read_to_string(devpath.join("nsid")) {
            Ok(ns_id) => {
                let ns_id = ns_id
                    .trim()
                    .parse()
                    .context("failed to parse NVMe namespace ID")?;

                let rescap = nvme::nvme_identify_namespace_data(file, ns_id)?.rescap;
                let oncs = nvme::nvme_identify_controller_data(file)?.oncs;
                tracing::debug!(rescap = ?rescap, oncs = ?oncs, "get identify data");
                supports_pr = oncs.reservations() && u8::from(rescap) != 0;
                Some(DeviceType::NVMe { ns_id, rescap })
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => Err(err).context("failed to read NVMe namespace ID")?,
        };

        // Fall back to unknown.
        let device_type = device_type.unwrap_or(DeviceType::UnknownBlock);

        fn read_val<T: FromStr>(devpath: &Path, path: &str, msg: &str) -> anyhow::Result<T>
        where
            T::Err: 'static + std::error::Error + Send + Sync,
        {
            fs_err::read_to_string(devpath.join(path))
                .with_context(|| format!("failed to read {msg}"))?
                .trim()
                .parse()
                .with_context(|| format!("failed to parse {msg}"))
        }

        let logical_block_size = read_val(&devpath, "queue/logical_block_size", "sector size")?;
        let physical_block_size = read_val(
            &devpath,
            "queue/physical_block_size",
            "physical sector size",
        )?;

        // sys/dev/block/*/*/size shows the size in 512-byte
        // sectors irrespective of the block device
        let disk_size = read_val::<u64>(&devpath, "size", "disk size")? * 512;
        let discard_granularity =
            read_val(&devpath, "queue/discard_granularity", "discard granularity")?;

        let fua = read_val::<u8>(&devpath, "queue/fua", "fua")? != 0;

        Self {
            device_type,
            disk_size,
            logical_block_size,
            physical_block_size,
            discard_granularity,
            supports_pr,
            fua,
        }
        .validate()
    }

    fn from_file(metadata: &fs::Metadata) -> anyhow::Result<Self> {
        let logical_block_size = 512;
        Self {
            device_type: DeviceType::File {
                sector_count: metadata.len() / logical_block_size as u64,
            },
            disk_size: metadata.size(),
            logical_block_size,
            physical_block_size: metadata.blksize() as u32,
            discard_granularity: 0,
            supports_pr: false,
            fua: false,
        }
        .validate()
    }

    fn validate(self) -> anyhow::Result<Self> {
        let Self {
            device_type: _,
            disk_size,
            logical_block_size,
            physical_block_size,
            discard_granularity,
            supports_pr: _,
            fua: _,
        } = self;
        if logical_block_size < 512 || !logical_block_size.is_power_of_two() {
            anyhow::bail!("invalid sector size {logical_block_size}");
        }
        if !physical_block_size.is_power_of_two() {
            anyhow::bail!("invalid physical sector size {physical_block_size}");
        }
        if disk_size % logical_block_size as u64 != 0 {
            anyhow::bail!("invalid disk size {disk_size:#x}");
        }
        if discard_granularity % logical_block_size != 0 {
            anyhow::bail!("invalid discard granularity {discard_granularity}");
        }
        Ok(self)
    }
}

impl DiskIo for BlockDevice {
    fn disk_type(&self) -> &str {
        "block_device"
    }

    fn sector_count(&self) -> u64 {
        if self.resize_epoch.epoch.load(Ordering::Relaxed)
            != self.resized_acked.load(Ordering::Relaxed)
        {
            self.handle_resize();
        }
        self.sector_count.load(Ordering::Relaxed)
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        self.supports_fua
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn pr(&self) -> Option<&dyn PersistentReservation> {
        if self.supports_pr { Some(self) } else { None }
    }

    async fn eject(&self) -> Result<(), DiskError> {
        let file = self.file.clone();
        unblock(move || {
            ioctl::lockdoor(&file, false)?;
            ioctl::eject(&file)
        })
        .await
        .map_err(|err| self.map_io_error(err))?;
        Ok(())
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let io_size = buffers.len();
        tracing::trace!(sector, io_size, "read_vectored");

        let mut bounce_buffer = None;
        let locked;
        let should_bounce = self.always_bounce || !buffers.is_aligned(self.sector_size() as usize);
        let io_vecs = if !should_bounce {
            locked = buffers.lock(true)?;
            locked.io_vecs()
        } else {
            tracing::trace!("double buffering IO");

            bounce_buffer
                .insert(
                    self.bounce_buffer_tracker
                        .acquire_bounce_buffers(buffers.len(), affinity::get_cpu_number() as usize)
                        .await,
                )
                .buffer
                .io_vecs()
        };

        // SAFETY: the buffers for the IO are this stack, and they will be
        // kept alive for the duration of the IO since we immediately call
        // await on the IO.
        let (r, _) = unsafe {
            self.initiator().issue_io((), |_| {
                opcode::Readv::new(
                    types::Fd(self.file.as_raw_fd()),
                    io_vecs.as_ptr().cast(),
                    io_vecs.len() as u32,
                )
                .offset((sector * self.sector_size() as u64) as _)
                .build()
            })
        }
        .await;

        let bytes_read = r.map_err(|err| self.map_io_error(err))?;
        tracing::trace!(bytes_read, "read_vectored");
        if bytes_read != io_size as i32 {
            return Err(DiskError::IllegalBlock);
        }

        if let Some(mut bounce_buffer) = bounce_buffer {
            buffers
                .writer()
                .write(bounce_buffer.buffer.as_mut_bytes())?;
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let io_size = buffers.len();
        tracing::trace!(sector, io_size, "write_vectored");

        // Ensure the write doesn't extend the file.
        if let DeviceType::File { sector_count } = self.device_type {
            if sector + (io_size as u64 >> self.sector_shift) > sector_count {
                return Err(DiskError::IllegalBlock);
            }
        }

        let mut bounce_buffer;
        let locked;
        let should_bounce = self.always_bounce || !buffers.is_aligned(self.sector_size() as usize);
        let io_vecs = if !should_bounce {
            locked = buffers.lock(false)?;
            locked.io_vecs()
        } else {
            tracing::trace!("double buffering IO");
            bounce_buffer = self
                .bounce_buffer_tracker
                .acquire_bounce_buffers(buffers.len(), affinity::get_cpu_number() as usize)
                .await;
            buffers.reader().read(bounce_buffer.buffer.as_mut_bytes())?;
            bounce_buffer.buffer.io_vecs()
        };

        // Documented in Linux manual page: https://man7.org/linux/man-pages/man2/readv.2.html
        // It's only defined in linux_gnu but not in linux_musl. So we have to define it.
        const RWF_DSYNC: RwFlags = 0x00000002;

        // SAFETY: the buffers for the IO are this stack, and they will be
        // kept alive for the duration of the IO since we immediately call
        // await on the IO.
        let (r, _) = unsafe {
            self.initiator().issue_io((), |_| {
                opcode::Writev::new(
                    types::Fd(self.file.as_raw_fd()),
                    io_vecs.as_ptr().cast::<libc::iovec>(),
                    io_vecs.len() as _,
                )
                .offset((sector * self.sector_size() as u64) as _)
                .rw_flags(if fua { RWF_DSYNC } else { 0 })
                .build()
            })
        }
        .await;

        let bytes_written = r.map_err(|err| self.map_io_error(err))?;
        tracing::trace!(bytes_written, "write_vectored");
        if bytes_written != io_size as i32 {
            return Err(DiskError::IllegalBlock);
        }

        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        // SAFETY: No data buffers.
        unsafe {
            self.initiator()
                .issue_io((), |_| {
                    opcode::Fsync::new(types::Fd(self.file.as_raw_fd())).build()
                })
                .await
                .0
                .map_err(|err| self.map_io_error(err))?;
        }
        Ok(())
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        loop {
            let listen = self.resize_epoch.event.listen();
            let current = self.sector_count();
            if current != sector_count {
                break current;
            }
            listen.await;
        }
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        let file = self.file.clone();
        let file_offset = sector_offset << self.sector_shift;
        let length = sector_count << self.sector_shift;
        tracing::debug!(file = ?file, file_offset, length, "unmap_async");
        match unblock(move || ioctl::discard(&file, file_offset, length)).await {
            Ok(()) => {}
            Err(_) if sector_offset + sector_count > self.sector_count() => {
                return Err(DiskError::IllegalBlock);
            }
            Err(err) => return Err(self.map_io_error(err)),
        }
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        if self.optimal_unmap_sectors == 0 {
            UnmapBehavior::Ignored
        } else {
            UnmapBehavior::Unspecified
        }
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.optimal_unmap_sectors
    }
}

#[async_trait::async_trait]
impl PersistentReservation for BlockDevice {
    fn capabilities(&self) -> ReservationCapabilities {
        match &self.device_type {
            &DeviceType::NVMe { rescap, .. } => {
                nvme_common::from_nvme_reservation_capabilities(rescap)
            }
            DeviceType::File { .. } | DeviceType::UnknownBlock => unreachable!(),
        }
    }

    async fn report(&self) -> Result<ReservationReport, DiskError> {
        assert!(matches!(self.device_type, DeviceType::NVMe { .. }));
        self.nvme_persistent_reservation_report()
            .await
            .map_err(|err| self.map_io_error(err))
    }

    async fn register(
        &self,
        current_key: Option<u64>,
        new_key: u64,
        ptpl: Option<bool>,
    ) -> Result<(), DiskError> {
        assert!(matches!(self.device_type, DeviceType::NVMe { .. }));

        // The Linux kernel interface to register does not allow ptpl to be
        // configured. We could manually issue an NVMe command, but this code
        // path is not really used anyway.
        if ptpl == Some(false) {
            tracing::warn!("ignoring guest request to disable persist through power loss");
        }

        let file = self.file.clone();
        unblock(move || {
            ioctl::pr_register(
                &file,
                current_key.unwrap_or(0),
                new_key,
                if current_key.is_none() {
                    ioctl::PR_FL_IGNORE_KEY
                } else {
                    0
                },
            )
        })
        .await
        .and_then(check_nvme_status)
        .map_err(|err| self.map_io_error(err))?;
        Ok(())
    }

    async fn reserve(&self, key: u64, reservation_type: ReservationType) -> Result<(), DiskError> {
        assert!(matches!(self.device_type, DeviceType::NVMe { .. }));
        let file = self.file.clone();
        unblock(move || ioctl::pr_reserve(&file, reservation_type, key))
            .await
            .and_then(check_nvme_status)
            .map_err(|err| self.map_io_error(err))?;
        Ok(())
    }

    async fn release(&self, key: u64, reservation_type: ReservationType) -> Result<(), DiskError> {
        assert!(matches!(self.device_type, DeviceType::NVMe { .. }));
        let file = self.file.clone();
        unblock(move || ioctl::pr_release(&file, reservation_type, key))
            .await
            .and_then(check_nvme_status)
            .map_err(|err| self.map_io_error(err))?;
        Ok(())
    }

    async fn clear(&self, key: u64) -> Result<(), DiskError> {
        assert!(matches!(self.device_type, DeviceType::NVMe { .. }));
        let file = self.file.clone();
        unblock(move || ioctl::pr_clear(&file, key))
            .await
            .and_then(check_nvme_status)
            .map_err(|err| self.map_io_error(err))?;
        Ok(())
    }

    async fn preempt(
        &self,
        current_key: u64,
        preempt_key: u64,
        reservation_type: ReservationType,
        abort: bool,
    ) -> Result<(), DiskError> {
        assert!(matches!(self.device_type, DeviceType::NVMe { .. }));
        let file = self.file.clone();
        unblock(move || {
            ioctl::pr_preempt(&file, reservation_type, current_key, preempt_key, abort)
        })
        .await
        .and_then(check_nvme_status)
        .map_err(|err| self.map_io_error(err))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use guestmem::GuestMemory;
    use hvdef::HV_PAGE_SIZE;
    use hvdef::HV_PAGE_SIZE_USIZE;
    use once_cell::sync::OnceCell;
    use pal_async::async_test;
    use pal_uring::IoUringPool;
    use scsi_buffers::OwnedRequestBuffers;

    fn is_buggy_kernel() -> bool {
        // 5.13 kernels seem to have a bug with io_uring where tests hang.
        let output = String::from_utf8(
            std::process::Command::new("uname")
                .arg("-r")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();

        output.contains("5.13")
    }

    fn new_block_device() -> Result<BlockDevice, NewDeviceError> {
        // TODO: switch to std::sync::OnceLock once `get_or_try_init` is stable
        static POOL: OnceCell<Arc<IoInitiator>> = OnceCell::new();

        let initiator = POOL
            .get_or_try_init(|| {
                let pool = IoUringPool::new("test", 16)?;
                let initiator = pool.client().initiator().clone();
                std::thread::spawn(|| pool.run());
                Ok(Arc::new(initiator))
            })
            .map_err(|err| NewDeviceError::IoctlError(DiskError::Io(err)))?;

        let bounce_buffer_tracker = Arc::new(BounceBufferTracker::new(
            2048,
            affinity::num_procs() as usize,
        ));

        let test_file = tempfile::tempfile().unwrap();
        test_file.set_len(1024 * 64).unwrap();
        block_on(BlockDevice::new(
            test_file.try_clone().unwrap(),
            false,
            initiator.clone(),
            None,
            bounce_buffer_tracker,
            false,
        ))
    }

    macro_rules! get_block_device_or_skip {
        () => {
            match new_block_device() {
                Ok(pool) => {
                    if is_buggy_kernel() {
                        println!("Test case skipped (buggy kernel version)");
                        return;
                    }

                    pool
                }
                Err(NewDeviceError::IoctlError(DiskError::Io(err)))
                    if err.raw_os_error() == Some(libc::ENOSYS) =>
                {
                    println!("Test case skipped (no IO-Uring support)");
                    return;
                }
                Err(err) => panic!("{}", err),
            }
        };
    }

    async fn run_async_disk_io(fua: bool) {
        let disk = get_block_device_or_skip!();

        let test_guest_mem = GuestMemory::allocate(0x8000);
        test_guest_mem
            .write_at(0, &(0..0x8000).map(|x| x as u8).collect::<Vec<_>>())
            .unwrap();

        let write_buffers = OwnedRequestBuffers::new(&[3, 2, 1, 0]);
        disk.write_vectored(&write_buffers.buffer(&test_guest_mem), 0, fua)
            .await
            .unwrap();

        if !fua {
            disk.sync_cache().await.unwrap();
        }

        let read_buffers = OwnedRequestBuffers::new(&[7, 6, 5, 4]);
        disk.read_vectored(&read_buffers.buffer(&test_guest_mem), 0)
            .await
            .unwrap();

        let mut source = vec![0u8; 4 * HV_PAGE_SIZE_USIZE];
        test_guest_mem.read_at(0, &mut source).unwrap();
        let mut target = vec![0u8; 4 * HV_PAGE_SIZE_USIZE];
        test_guest_mem
            .read_at(4 * HV_PAGE_SIZE, &mut target)
            .unwrap();
        assert_eq!(source, target);
    }

    #[async_test]
    async fn test_async_disk_io() {
        run_async_disk_io(false).await;
    }

    #[async_test]
    async fn test_async_disk_io_fua() {
        run_async_disk_io(true).await;
    }

    async fn run_async_disk_io_unaligned(fua: bool) {
        let disk = get_block_device_or_skip!();

        let test_guest_mem = GuestMemory::allocate(0x8000);
        test_guest_mem
            .write_at(0, &(0..0x8000).map(|x| x as u8).collect::<Vec<_>>())
            .unwrap();

        let write_buffers =
            OwnedRequestBuffers::new_unaligned(&[0, 1, 2, 3], 512, 3 * HV_PAGE_SIZE_USIZE);

        disk.write_vectored(&write_buffers.buffer(&test_guest_mem), 0, fua)
            .await
            .unwrap();

        if !fua {
            disk.sync_cache().await.unwrap();
        }

        let read_buffers =
            OwnedRequestBuffers::new_unaligned(&[4, 5, 6, 7], 512, 3 * HV_PAGE_SIZE_USIZE);
        disk.read_vectored(&read_buffers.buffer(&test_guest_mem), 0)
            .await
            .unwrap();

        let mut source = vec![0u8; 3 * HV_PAGE_SIZE_USIZE];
        test_guest_mem.read_at(512, &mut source).unwrap();
        let mut target = vec![0u8; 3 * HV_PAGE_SIZE_USIZE];
        test_guest_mem
            .read_at(4 * HV_PAGE_SIZE + 512, &mut target)
            .unwrap();
        assert_eq!(source, target);
    }

    #[async_test]
    async fn test_async_disk_io_unaligned() {
        run_async_disk_io_unaligned(false).await;
    }

    #[async_test]
    async fn test_async_disk_io_unaligned_fua() {
        run_async_disk_io_unaligned(true).await;
    }

    #[async_test]
    async fn test_illegal_lba() {
        let disk = get_block_device_or_skip!();
        let gm = GuestMemory::allocate(512);
        match disk
            .write_vectored(
                &OwnedRequestBuffers::linear(0, 512, true).buffer(&gm),
                i64::MAX as u64 / 512,
                false,
            )
            .await
        {
            Err(DiskError::IllegalBlock) => {}
            r => panic!("unexpected result: {:?}", r),
        }
    }
}
