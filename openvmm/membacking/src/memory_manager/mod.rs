// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM's memory manager.

mod device_memory;

pub use device_memory::DeviceMemoryMapper;

use crate::RemoteProcess;
use crate::mapping_manager::Mappable;
use crate::mapping_manager::MappingManager;
use crate::mapping_manager::MappingManagerClient;
use crate::mapping_manager::VaMapper;
use crate::mapping_manager::VaMapperError;
use crate::partition_mapper::PartitionMapper;
use crate::region_manager::MapParams;
use crate::region_manager::RegionHandle;
use crate::region_manager::RegionManager;
use guestmem::GuestMemory;
use hvdef::Vtl;
use inspect::Inspect;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use pal_async::DefaultPool;
use sparse_mmap::SparseMapping;
use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;

/// The OpenVMM memory manager.
#[derive(Debug, Inspect)]
pub struct GuestMemoryManager {
    /// Guest RAM allocation. None in private memory mode.
    #[inspect(skip)]
    guest_ram: Option<Mappable>,

    #[inspect(skip)]
    ram_regions: Arc<Vec<RamRegion>>,

    #[inspect(flatten)]
    mapping_manager: MappingManager,

    #[inspect(flatten)]
    region_manager: RegionManager,

    #[inspect(skip)]
    va_mapper: Arc<VaMapper>,

    #[inspect(skip)]
    _thread: JoinHandle<()>,

    vtl0_alias_map_offset: Option<u64>,
    pin_mappings: bool,
}

#[derive(Debug)]
struct RamRegion {
    range: MemoryRange,
    handle: RegionHandle,
}

/// Errors when attaching a partition to a [`GuestMemoryManager`].
#[derive(Error, Debug)]
pub enum PartitionAttachError {
    /// Failure to allocate a VA mapper.
    #[error("failed to reserve VA range for partition mapping")]
    VaMapper(#[source] VaMapperError),
    /// Failure to map memory into a partition.
    #[error("failed to attach partition to memory manager")]
    PartitionMapper(#[source] crate::partition_mapper::PartitionMapperError),
}

/// Errors creating a [`GuestMemoryManager`].
#[derive(Error, Debug)]
pub enum MemoryBuildError {
    /// RAM too large.
    #[error("ram size {0} is too large")]
    RamTooLarge(MemorySize),
    /// Couldn't allocate RAM.
    #[error("failed to allocate memory")]
    AllocationFailed(#[source] io::Error),
    /// Couldn't allocate hugetlb-backed RAM.
    #[error(
        "failed to reserve {page_count} hugetlb pages of {hugepage_size} each ({size} total); increase the hugetlb pool or reduce guest memory size"
    )]
    HugepageAllocationFailed {
        /// Total RAM backing size.
        size: MemorySize,
        /// Requested or default hugepage size.
        hugepage_size: MemorySize,
        /// Number of hugepages required.
        page_count: usize,
        /// The allocation error.
        #[source]
        error: io::Error,
    },
    /// Couldn't allocate VA mapper.
    #[error("failed to create VA mapper")]
    VaMapper(#[source] VaMapperError),
    /// Memory layout incompatible with VTL0 alias map.
    #[error("not enough guest address space available for the vtl0 alias map")]
    AliasMapWontFit,
    /// Memory layout incompatible with x86 legacy support.
    #[error("x86 support requires RAM to start at 0 and contain at least 1MB")]
    InvalidRamForX86,
    /// Private memory is incompatible with x86 legacy support.
    #[error("private memory is incompatible with x86 legacy support")]
    PrivateMemoryWithLegacy,
    /// Private memory is incompatible with existing memory backing.
    #[error("private memory is incompatible with existing memory backing")]
    PrivateMemoryWithExistingBacking,
    /// Failed to allocate private RAM range.
    #[error("failed to allocate private RAM range {1}")]
    PrivateRamAlloc(#[source] io::Error, MemoryRange),
    /// THP requires private memory mode.
    #[error("transparent huge pages requires private memory mode")]
    ThpWithoutPrivateMemory,
    /// THP is only supported on Linux.
    #[error("transparent huge pages is only supported on Linux")]
    ThpUnsupportedPlatform,
    /// Hugepage size is too large.
    #[error("hugepage size {0} is too large")]
    HugepageSizeTooLarge(MemorySize),
    /// Hugepages are only supported on Linux.
    #[error("hugepages are only supported on Linux")]
    HugepagesUnsupportedPlatform,
    /// Hugepages require shared memory mode.
    #[error("hugepages require shared memory mode")]
    HugepagesWithPrivateMemory,
    /// Hugepages are incompatible with existing memory backing.
    #[error("hugepages are incompatible with existing memory backing")]
    HugepagesWithExistingBacking,
    /// Hugepages are incompatible with x86 legacy RAM splitting.
    #[error("hugepages are incompatible with x86 legacy RAM splitting")]
    HugepagesWithLegacy,
    /// Invalid hugepage size.
    #[error("hugepage size {0} must be a power of two and at least the host page size")]
    InvalidHugepageSize(MemorySize),
    /// RAM size is not aligned to the hugepage size.
    #[error(
        "RAM size {ram_size} is not aligned to {hugepage_size} hugepages; choose a memory size that is a multiple of the hugepage size"
    )]
    HugepageRamSizeUnaligned {
        /// Total RAM backing size.
        ram_size: MemorySize,
        /// Required hugepage alignment.
        hugepage_size: MemorySize,
    },
    /// A RAM range is not aligned to the hugepage size.
    #[error(
        "RAM range {range} ({range_size}) is not aligned to {hugepage_size} hugepages; range start and size must both be multiples of the hugepage size"
    )]
    HugepageRamRangeUnaligned {
        /// The unaligned RAM range.
        range: MemoryRange,
        /// The RAM range size.
        range_size: MemorySize,
        /// Required hugepage alignment.
        hugepage_size: MemorySize,
    },
}

const DEFAULT_HUGEPAGE_SIZE: u64 = 2 * 1024 * 1024;

/// Explicit hugetlb memfd backing configuration.
#[derive(Debug, Copy, Clone)]
struct HugepageConfig {
    size: Option<u64>,
}

fn validate_hugepage_size(size: u64) -> Result<usize, MemoryBuildError> {
    if !size.is_power_of_two() || size < SparseMapping::page_size() as u64 {
        return Err(MemoryBuildError::InvalidHugepageSize(MemorySize(size)));
    }
    size.try_into()
        .map_err(|_| MemoryBuildError::HugepageSizeTooLarge(MemorySize(size)))
}

/// A byte count displayed in a human-readable format in error messages.
#[derive(Debug, Copy, Clone)]
pub struct MemorySize(
    /// The size in bytes.
    pub u64,
);

impl std::fmt::Display for MemorySize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * KB;
        const GB: u64 = 1024 * MB;
        const TB: u64 = 1024 * GB;

        for (unit, suffix) in [(TB, "TB"), (GB, "GB"), (MB, "MB"), (KB, "KB")] {
            if self.0 != 0 && self.0.is_multiple_of(unit) {
                return write!(f, "{} {suffix}", self.0 / unit);
            }
        }

        write!(f, "{} bytes", self.0)
    }
}

fn validate_hugepage_ram_alignment(
    ram_size: u64,
    ram_ranges: &[MemoryRange],
    hugepage_size: u64,
) -> Result<(), MemoryBuildError> {
    if !ram_size.is_multiple_of(hugepage_size) {
        return Err(MemoryBuildError::HugepageRamSizeUnaligned {
            ram_size: MemorySize(ram_size),
            hugepage_size: MemorySize(hugepage_size),
        });
    }
    for &range in ram_ranges {
        if !range.start().is_multiple_of(hugepage_size)
            || !range.len().is_multiple_of(hugepage_size)
        {
            return Err(MemoryBuildError::HugepageRamRangeUnaligned {
                range,
                range_size: MemorySize(range.len()),
                hugepage_size: MemorySize(hugepage_size),
            });
        }
    }
    Ok(())
}

/// A builder for [`GuestMemoryManager`].
pub struct GuestMemoryBuilder {
    existing_mapping: Option<SharedMemoryBacking>,
    vtl0_alias_map: Option<u64>,
    prefetch_ram: bool,
    pin_mappings: bool,
    x86_legacy_support: bool,
    private_memory: bool,
    transparent_hugepages: bool,
    hugepages: Option<HugepageConfig>,
}

impl GuestMemoryBuilder {
    /// Returns a new builder.
    pub fn new() -> Self {
        Self {
            existing_mapping: None,
            vtl0_alias_map: None,
            pin_mappings: false,
            prefetch_ram: false,
            x86_legacy_support: false,
            private_memory: false,
            transparent_hugepages: false,
            hugepages: None,
        }
    }

    /// Specifies an existing memory backing to use.
    pub fn existing_backing(mut self, mapping: Option<SharedMemoryBacking>) -> Self {
        self.existing_mapping = mapping;
        self
    }

    /// Specifies the offset of the VTL0 alias map, if enabled for VTL2. This is
    /// a mirror of VTL0 memory into a high portion of the VM's physical address
    /// space.
    pub fn vtl0_alias_map(mut self, offset: Option<u64>) -> Self {
        self.vtl0_alias_map = offset;
        self
    }

    /// Specify whether to pin mappings in memory. This is used to support
    /// device assignment for devices that require the IOMMU to be programmed
    /// for all addresses.
    pub fn pin_mappings(mut self, enable: bool) -> Self {
        self.pin_mappings = enable;
        self
    }

    /// Specify whether to prefetch RAM mappings. This improves boot performance
    /// by reducing memory intercepts at the cost of pre-allocating all of RAM.
    pub fn prefetch_ram(mut self, enable: bool) -> Self {
        self.prefetch_ram = enable;
        self
    }

    /// Enables legacy x86 support.
    ///
    /// When set, create separate RAM regions for the various low memory ranges
    /// that are special on x86 platforms. Specifically:
    ///
    /// 1. Create a separate RAM region for the VGA VRAM window:
    ///    0xa0000-0xbffff.
    /// 2. Create separate RAM regions within 0xc0000-0xfffff for control by PAM
    ///    registers.
    ///
    /// The caller can use [`RamVisibilityControl`] to adjust the visibility of
    /// these ranges.
    pub fn x86_legacy_support(mut self, enable: bool) -> Self {
        self.x86_legacy_support = enable;
        self
    }

    /// Enables private anonymous memory for guest RAM.
    ///
    /// When set, guest RAM is backed by anonymous pages (`mmap
    /// MAP_ANONYMOUS` on Linux, `VirtualAlloc` on Windows) rather than
    /// shared file-backed sections. This supports decommit to release
    /// physical pages back to the host.
    ///
    /// This is incompatible with [`x86_legacy_support`](Self::x86_legacy_support)
    /// and [`existing_backing`](Self::existing_backing).
    pub fn private_memory(mut self, enable: bool) -> Self {
        self.private_memory = enable;
        self
    }

    /// Enables Transparent Huge Pages for guest RAM.
    ///
    /// When set, `madvise(MADV_HUGEPAGE)` is called on private RAM allocations
    /// to allow khugepaged to collapse 4K pages into 2MB huge pages.
    /// Requires [`private_memory`](Self::private_memory) and Linux; `build()`
    /// will return an error if either condition is not met.
    pub fn transparent_hugepages(mut self, enable: bool) -> Self {
        self.transparent_hugepages = enable;
        self
    }

    /// Enables explicit hugetlb memfd backing for guest RAM.
    pub fn hugepages(mut self, size: Option<u64>) -> Self {
        self.hugepages = Some(HugepageConfig { size });
        self
    }

    /// Builds the memory backing, allocating memory if existing memory was not
    /// provided by [`existing_backing`](Self::existing_backing).
    pub async fn build(
        self,
        mem_layout: &MemoryLayout,
    ) -> Result<GuestMemoryManager, MemoryBuildError> {
        // Validate private memory constraints.
        if self.private_memory {
            if self.x86_legacy_support {
                return Err(MemoryBuildError::PrivateMemoryWithLegacy);
            }
            if self.existing_mapping.is_some() {
                return Err(MemoryBuildError::PrivateMemoryWithExistingBacking);
            }
        }

        // Validate THP constraints.
        if self.transparent_hugepages {
            if !self.private_memory {
                return Err(MemoryBuildError::ThpWithoutPrivateMemory);
            }
            if !cfg!(target_os = "linux") {
                return Err(MemoryBuildError::ThpUnsupportedPlatform);
            }
        }

        let ram_size = mem_layout.ram_size() + mem_layout.vtl2_range().map_or(0, |r| r.len());

        let mut ram_ranges = mem_layout
            .ram()
            .iter()
            .map(|x| x.range)
            .chain(mem_layout.vtl2_range())
            .collect::<Vec<_>>();

        let hugepage_size = if let Some(hugepages) = self.hugepages {
            if !cfg!(target_os = "linux") {
                return Err(MemoryBuildError::HugepagesUnsupportedPlatform);
            }
            if self.private_memory {
                return Err(MemoryBuildError::HugepagesWithPrivateMemory);
            }
            if self.existing_mapping.is_some() {
                return Err(MemoryBuildError::HugepagesWithExistingBacking);
            }
            if self.x86_legacy_support {
                return Err(MemoryBuildError::HugepagesWithLegacy);
            }
            let size = validate_hugepage_size(hugepages.size.unwrap_or(DEFAULT_HUGEPAGE_SIZE))?;
            validate_hugepage_ram_alignment(ram_size, &ram_ranges, size as u64)?;
            Some(size)
        } else {
            None
        };

        let memory: Option<Mappable> = if self.private_memory {
            // Private memory mode: no shared file-backed allocation.
            // RAM will be backed by anonymous pages in the VaMapper's SparseMapping.
            None
        } else if let Some(memory) = self.existing_mapping {
            Some(memory.guest_ram)
        } else {
            let ram_size = ram_size
                .try_into()
                .map_err(|_| MemoryBuildError::RamTooLarge(MemorySize(ram_size)))?;
            let guest_ram = if let Some(hugepage_size) = hugepage_size {
                sparse_mmap::alloc_shared_memory_hugetlb(ram_size, "guest-ram", Some(hugepage_size))
                    .map_err(|error| MemoryBuildError::HugepageAllocationFailed {
                        size: MemorySize(ram_size as u64),
                        hugepage_size: MemorySize(hugepage_size as u64),
                        page_count: ram_size / hugepage_size,
                        error,
                    })?
            } else {
                sparse_mmap::alloc_shared_memory(ram_size, "guest-ram")
                    .map_err(MemoryBuildError::AllocationFailed)?
            };
            Some(guest_ram.into())
        };

        // Spawn a thread to handle memory requests.
        //
        // FUTURE: move this to a task once the GuestMemory deadlocks are resolved.
        let (thread, spawner) = DefaultPool::spawn_on_thread("memory_manager");

        let max_addr =
            (mem_layout.end_of_layout()).max(mem_layout.vtl2_range().map_or(0, |r| r.end()));

        let vtl0_alias_map_offset = if let Some(offset) = self.vtl0_alias_map {
            if max_addr > offset {
                return Err(MemoryBuildError::AliasMapWontFit);
            }
            Some(offset)
        } else {
            None
        };

        let mapping_manager =
            MappingManager::new(&spawner, max_addr, self.private_memory, hugepage_size);
        let va_mapper = mapping_manager
            .client()
            .new_mapper()
            .await
            .map_err(MemoryBuildError::VaMapper)?;

        let region_manager = RegionManager::new(&spawner, mapping_manager.client().clone());

        if self.x86_legacy_support {
            if ram_ranges[0].start() != 0 || ram_ranges[0].end() < 0x100000 {
                return Err(MemoryBuildError::InvalidRamForX86);
            }

            // Split RAM ranges to support PAM registers and VGA RAM.
            let range_starts = [
                0,
                0xa0000,
                0xc0000,
                0xc4000,
                0xc8000,
                0xcc000,
                0xd0000,
                0xd4000,
                0xd8000,
                0xdc000,
                0xe0000,
                0xe4000,
                0xe8000,
                0xec000,
                0xf0000,
                0x100000,
                ram_ranges[0].end(),
            ];

            ram_ranges.splice(
                0..1,
                range_starts
                    .iter()
                    .zip(range_starts.iter().skip(1))
                    .map(|(&start, &end)| MemoryRange::new(start..end)),
            );
        }

        // In private memory mode, eagerly commit all RAM ranges with
        // anonymous memory. alloc_range() handles both Linux (mmap MAP_FIXED)
        // and Windows (MEM_REPLACE_PLACEHOLDER).
        if self.private_memory {
            for range in &ram_ranges {
                va_mapper
                    .alloc_range(range.start() as usize, range.len() as usize)
                    .map_err(|e| MemoryBuildError::PrivateRamAlloc(e, *range))?;
                va_mapper.set_range_name(
                    range.start() as usize,
                    range.len() as usize,
                    "guest-ram-private",
                );
            }

            // Mark private RAM as THP-eligible so khugepaged can collapse
            // 4K pages into 2MB huge pages.
            #[cfg(target_os = "linux")]
            if self.transparent_hugepages {
                for range in &ram_ranges {
                    if let Err(e) =
                        va_mapper.madvise_hugepage(range.start() as usize, range.len() as usize)
                    {
                        tracing::warn!(
                            error = &e as &dyn std::error::Error,
                            range = %range,
                            "failed to mark RAM as THP eligible"
                        );
                    }
                }
            }
        }

        let mut ram_regions = Vec::new();
        let mut start = 0;
        for range in &ram_ranges {
            let region = region_manager
                .client()
                .new_region("ram".into(), *range, RAM_PRIORITY, true)
                .await
                .expect("regions cannot overlap yet");

            if let Some(ref memory) = memory {
                // File-backed mode: add mapping for this RAM range.
                region
                    .add_mapping(
                        MemoryRange::new(0..range.len()),
                        memory.clone(),
                        start,
                        true,
                    )
                    .await;
            }
            // In private_memory mode, skip add_mapping — no file-backed RAM.
            // The SparseMapping VA is already committed via alloc_range() above.

            region
                .map(MapParams {
                    writable: true,
                    executable: true,
                    prefetch: self.prefetch_ram && !self.private_memory,
                })
                .await;

            ram_regions.push(RamRegion {
                range: *range,
                handle: region,
            });
            start += range.len();
        }

        let gm = GuestMemoryManager {
            guest_ram: memory,
            _thread: thread,
            ram_regions: Arc::new(ram_regions),
            mapping_manager,
            region_manager,
            va_mapper,
            vtl0_alias_map_offset,
            pin_mappings: self.pin_mappings,
        };
        Ok(gm)
    }
}

/// The backing objects used to transfer guest memory between processes.
#[derive(Debug, MeshPayload)]
pub struct SharedMemoryBacking {
    guest_ram: Mappable,
}

impl SharedMemoryBacking {
    /// Create a SharedMemoryBacking from a mappable handle/fd.
    pub fn from_mappable(guest_ram: Mappable) -> Self {
        Self { guest_ram }
    }
}

/// A mesh-serializable object for providing access to guest memory.
#[derive(Debug, MeshPayload)]
pub struct GuestMemoryClient {
    mapping_manager: MappingManagerClient,
}

impl GuestMemoryClient {
    /// Retrieves a [`GuestMemory`] object to access guest memory from this
    /// process.
    ///
    /// This call will ensure only one VA mapper is allocated per process, so
    /// this is safe to call many times without allocating tons of virtual
    /// address space.
    pub async fn guest_memory(&self) -> Result<GuestMemory, VaMapperError> {
        Ok(GuestMemory::new(
            "ram",
            self.mapping_manager.new_mapper().await?,
        ))
    }
}

// The region priority for RAM. Overrides anything else.
const RAM_PRIORITY: u8 = 255;

// The region priority for device memory.
const DEVICE_PRIORITY: u8 = 0;

impl GuestMemoryManager {
    /// Returns an object to access guest memory.
    pub fn client(&self) -> GuestMemoryClient {
        GuestMemoryClient {
            mapping_manager: self.mapping_manager.client().clone(),
        }
    }

    /// Returns an object to map device memory into the VM.
    pub fn device_memory_mapper(&self) -> DeviceMemoryMapper {
        DeviceMemoryMapper::new(self.region_manager.client().clone())
    }

    /// Returns a client for registering DMA mappers (VFIO, iommufd).
    pub fn dma_mapper_client(&self) -> crate::region_manager::DmaMapperClient {
        crate::region_manager::DmaMapperClient::new(self.region_manager.client())
    }

    /// Returns an object for manipulating the visibility state of different RAM
    /// regions.
    pub fn ram_visibility_control(&self) -> RamVisibilityControl {
        RamVisibilityControl {
            regions: self.ram_regions.clone(),
        }
    }

    /// Returns the shared memory resources that can be used to reconstruct the
    /// memory backing.
    ///
    /// This can be used with [`GuestMemoryBuilder::existing_backing`] to create a
    /// new memory manager with the same memory state. Only one instance of this
    /// type should be managing a given memory backing at a time, though, or the
    /// guest may see unpredictable results.
    ///
    /// Returns `None` in private memory mode, where there is no shared
    /// file-backed allocation.
    pub fn shared_memory_backing(&self) -> Option<SharedMemoryBacking> {
        let guest_ram = self.guest_ram.clone()?;
        Some(SharedMemoryBacking { guest_ram })
    }

    /// Attaches the guest memory to a partition, mapping it to the guest
    /// physical address space.
    ///
    /// If `process` is provided, then allocate a VA range in that process for
    /// the guest memory, and map the memory into the partition from that
    /// process. This is necessary to work around WHP's lack of support for
    /// mapping multiple partitions from a single process.
    ///
    /// TODO: currently, all VTLs will get the same mappings--no support for
    /// per-VTL memory protections is supported.
    pub async fn attach_partition(
        &mut self,
        vtl: Vtl,
        partition: &Arc<dyn virt::PartitionMemoryMap>,
        process: Option<RemoteProcess>,
    ) -> Result<(), PartitionAttachError> {
        let va_mapper = if let Some(process) = process {
            self.mapping_manager
                .client()
                .new_remote_mapper(process)
                .await
                .map_err(PartitionAttachError::VaMapper)?
        } else {
            self.va_mapper.clone()
        };

        if vtl == Vtl::Vtl2 {
            if let Some(offset) = self.vtl0_alias_map_offset {
                let partition =
                    PartitionMapper::new(partition, va_mapper.clone(), offset, self.pin_mappings);
                self.region_manager
                    .client()
                    .add_partition(partition)
                    .await
                    .map_err(PartitionAttachError::PartitionMapper)?;
            }
        }

        let partition = PartitionMapper::new(partition, va_mapper, 0, self.pin_mappings);
        self.region_manager
            .client()
            .add_partition(partition)
            .await
            .map_err(PartitionAttachError::PartitionMapper)?;
        Ok(())
    }
}

/// A client to the [`GuestMemoryManager`] used to control the visibility of
/// RAM regions.
pub struct RamVisibilityControl {
    regions: Arc<Vec<RamRegion>>,
}

/// The RAM visibility for use with [`RamVisibilityControl::set_ram_visibility`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RamVisibility {
    /// RAM is unmapped, so reads and writes will go to device memory or MMIO.
    Unmapped,
    /// RAM is read-only. Writes will go to device memory or MMIO.
    ///
    /// Note that writes will take exits even if there is mapped device memory.
    ReadOnly,
    /// RAM is read-write by the guest.
    ReadWrite,
}

/// An error returned by [`RamVisibilityControl::set_ram_visibility`].
#[derive(Debug, Error)]
#[error("{0} is not a controllable RAM range")]
pub struct InvalidRamRegion(MemoryRange);

impl RamVisibilityControl {
    /// Sets the visibility of a RAM region.
    ///
    /// A whole region's visibility must be controlled at once, or an error will
    /// be returned. [`GuestMemoryBuilder::x86_legacy_support`] can be used to
    /// ensure that there are RAM regions corresponding to x86 memory ranges
    /// that need to be controlled.
    pub async fn set_ram_visibility(
        &self,
        range: MemoryRange,
        visibility: RamVisibility,
    ) -> Result<(), InvalidRamRegion> {
        let region = self
            .regions
            .iter()
            .find(|region| region.range == range)
            .ok_or(InvalidRamRegion(range))?;

        match visibility {
            RamVisibility::ReadWrite | RamVisibility::ReadOnly => {
                region
                    .handle
                    .map(MapParams {
                        writable: matches!(visibility, RamVisibility::ReadWrite),
                        executable: true,
                        prefetch: false,
                    })
                    .await
            }
            RamVisibility::Unmapped => region.handle.unmap().await,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn test_validate_hugepage_size() {
        let page_size = SparseMapping::page_size() as u64;
        assert!(validate_hugepage_size(page_size).is_ok());
        assert!(matches!(
            validate_hugepage_size(page_size / 2),
            Err(MemoryBuildError::InvalidHugepageSize(_))
        ));
        assert!(matches!(
            validate_hugepage_size(3 * 1024 * 1024),
            Err(MemoryBuildError::InvalidHugepageSize(_))
        ));
    }

    #[test]
    fn test_validate_hugepage_ram_alignment() {
        const HUGEPAGE_SIZE: u64 = 2 * 1024 * 1024;

        validate_hugepage_ram_alignment(
            4 * 1024 * 1024,
            &[
                MemoryRange::new(0..HUGEPAGE_SIZE),
                MemoryRange::new(2 * HUGEPAGE_SIZE..3 * HUGEPAGE_SIZE),
            ],
            HUGEPAGE_SIZE,
        )
        .unwrap();

        assert!(matches!(
            validate_hugepage_ram_alignment(3 * 1024 * 1024, &[], HUGEPAGE_SIZE),
            Err(MemoryBuildError::HugepageRamSizeUnaligned { .. })
        ));
        assert!(matches!(
            validate_hugepage_ram_alignment(
                HUGEPAGE_SIZE,
                &[MemoryRange::new(0..1024 * 1024)],
                HUGEPAGE_SIZE,
            ),
            Err(MemoryBuildError::HugepageRamRangeUnaligned { .. })
        ));
    }

    #[test]
    fn test_hugepage_ram_size_alignment_error_message() {
        let error =
            validate_hugepage_ram_alignment(257 * 1024 * 1024, &[], 2 * 1024 * 1024).unwrap_err();

        assert_eq!(
            error.to_string(),
            "RAM size 257 MB is not aligned to 2 MB hugepages; choose a memory size that is a multiple of the hugepage size"
        );
    }

    #[test]
    fn test_hugepage_ram_range_alignment_error_message() {
        let error = validate_hugepage_ram_alignment(
            2 * 1024 * 1024,
            &[MemoryRange::new(0..1024 * 1024)],
            2 * 1024 * 1024,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "RAM range 0x0-0x100000 (1 MB) is not aligned to 2 MB hugepages; range start and size must both be multiples of the hugepage size"
        );
    }

    #[test]
    fn test_hugepage_allocation_error_message() {
        let error = MemoryBuildError::HugepageAllocationFailed {
            size: MemorySize(1024 * 1024 * 1024),
            hugepage_size: MemorySize(2 * 1024 * 1024),
            page_count: 512,
            error: io::Error::new(io::ErrorKind::OutOfMemory, "Cannot allocate memory"),
        };

        assert_eq!(
            error.to_string(),
            "failed to reserve 512 hugetlb pages of 2 MB each (1 GB total); increase the hugetlb pool or reduce guest memory size"
        );
        assert_eq!(
            error.source().unwrap().to_string(),
            "Cannot allocate memory"
        );
    }
}
