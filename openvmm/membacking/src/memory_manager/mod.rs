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

/// The OpenVMM memory manager.
#[derive(Debug, Inspect)]
pub struct GuestMemoryManager {
    /// Guest RAM allocations. One per backing request. Empty only when
    /// there are no backing requests (no RAM at all).
    #[inspect(skip)]
    guest_ram: Vec<RamBacking>,

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

/// A single RAM backing allocation — one memfd or anonymous region.
#[derive(Debug)]
struct RamBacking {
    /// The file-backed memory handle. `None` for private (anonymous) backings.
    mappable: Option<Mappable>,
    /// GPA ranges covered by this backing.
    ranges: Vec<MemoryRange>,
    /// Prefetch pages at build time.
    prefetch: bool,
    /// THP is enabled for this backing.
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    transparent_hugepages: bool,
    /// Host NUMA node for this backing. `None` means OS default placement.
    host_numa_node: Option<u32>,
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
    /// Failed to map RAM into VA space.
    #[error("failed to map RAM range {range}")]
    RamMapping {
        /// The GPA range that failed to map.
        range: MemoryRange,
        /// The mapping error.
        #[source]
        error: mesh::error::RemoteError,
    },
    /// Failed to enable RAM region.
    #[error("failed to enable RAM region {range}")]
    RamRegionEnable {
        /// The GPA range that failed.
        range: MemoryRange,
        /// The error.
        #[source]
        error: mesh::error::RemoteError,
    },
    /// Memory layout incompatible with VTL0 alias map.
    #[error("not enough guest address space available for the vtl0 alias map")]
    AliasMapWontFit,
    /// Memory layout incompatible with x86 legacy support.
    #[error("x86 support requires RAM to start at 0 and contain at least 1MB")]
    InvalidRamForX86,
    /// Private memory is incompatible with x86 legacy support.
    #[error("private memory is incompatible with x86 legacy support")]
    PrivateMemoryWithLegacy,
    /// Private memory is incompatible with an existing memory backing.
    #[error("private memory is incompatible with an existing memory backing")]
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
    /// Host NUMA node binding is only supported on Linux and Windows.
    #[error("host NUMA node binding is only supported on Linux and Windows")]
    HostNumaNodeUnsupportedPlatform,
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

/// A request to allocate one RAM backing region (one memfd or anonymous
/// allocation). For non-NUMA VMs, a single request covers all RAM. For
/// NUMA VMs, one request per node with memory.
///
/// Construct via [`RamBackingRequest::new`].
#[derive(Debug)]
pub struct RamBackingRequest {
    ranges: Vec<MemoryRange>,
    prefetch: bool,
    private_memory: bool,
    transparent_hugepages: bool,
    hugepages: bool,
    hugepage_size: Option<u64>,
    existing_mappable: Option<Mappable>,
    host_numa_node: Option<u32>,
}

impl RamBackingRequest {
    /// Creates a new backing request covering the given GPA ranges.
    ///
    /// The backing's allocation size is the sum of the range lengths.
    /// Defaults to shared file-backed memory with no prefetch.
    pub fn new(ranges: Vec<MemoryRange>) -> Self {
        Self {
            ranges,
            prefetch: false,
            private_memory: false,
            transparent_hugepages: false,
            hugepages: false,
            hugepage_size: None,
            existing_mappable: None,
            host_numa_node: None,
        }
    }

    /// Prefetch (pre-fault) all pages at build time.
    pub fn prefetch(mut self, enable: bool) -> Self {
        self.prefetch = enable;
        self
    }

    /// Use private anonymous memory instead of shared file-backed memory.
    pub fn private_memory(mut self, enable: bool) -> Self {
        self.private_memory = enable;
        self
    }

    /// Enable Transparent Huge Pages (requires `private_memory`, Linux only).
    pub fn transparent_hugepages(mut self, enable: bool) -> Self {
        self.transparent_hugepages = enable;
        self
    }

    /// Enable explicit hugetlb memfd backing with an optional size
    /// override (default: 2 MB). Incompatible with `private_memory`.
    pub fn hugepages(mut self, size: Option<u64>) -> Self {
        self.hugepages = true;
        self.hugepage_size = size;
        self
    }

    /// Reuse an existing file-backed memory handle (restore path).
    /// When set, no new allocation is performed for this backing.
    pub fn existing_mappable(mut self, mappable: Mappable) -> Self {
        self.existing_mappable = Some(mappable);
        self
    }

    /// Bind this backing's memory to a specific host NUMA node
    /// (Linux: `mbind(MPOL_BIND)`, Windows: `MemExtendedParameterNumaNode`).
    ///
    /// Only supported on Linux and Windows; returns
    /// [`MemoryBuildError::HostNumaNodeUnsupportedPlatform`] at build time on
    /// other targets.
    pub fn host_numa_node(mut self, node: Option<u32>) -> Self {
        self.host_numa_node = node;
        self
    }
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
    vtl0_alias_map: Option<u64>,
    pin_mappings: bool,
    x86_legacy_support: bool,
    backing_requests: Vec<RamBackingRequest>,
}

impl GuestMemoryBuilder {
    /// Returns a new builder.
    pub fn new() -> Self {
        Self {
            vtl0_alias_map: None,
            pin_mappings: false,
            x86_legacy_support: false,
            backing_requests: Vec::new(),
        }
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

    /// Adds a RAM backing request. Call once per backing (one per NUMA node,
    /// or once for a non-NUMA VM).
    pub fn add_backing(mut self, request: RamBackingRequest) -> Self {
        self.backing_requests.push(request);
        self
    }

    /// Builds the memory backing, allocating one memfd or anonymous region
    /// per backing request.
    ///
    /// Each [`RamBackingRequest`] produces one RAM backing. File-backed
    /// requests allocate a memfd (or reuse `existing_mappable` if set);
    /// private requests use anonymous pages.
    pub async fn build(self, max_addr: u64) -> Result<GuestMemoryManager, MemoryBuildError> {
        let backing_requests = self.backing_requests;

        // Validate per-request constraints.
        for req in &backing_requests {
            if req.private_memory && self.x86_legacy_support {
                return Err(MemoryBuildError::PrivateMemoryWithLegacy);
            }
            if req.private_memory && req.existing_mappable.is_some() {
                return Err(MemoryBuildError::PrivateMemoryWithExistingBacking);
            }
            if req.transparent_hugepages {
                if !req.private_memory {
                    return Err(MemoryBuildError::ThpWithoutPrivateMemory);
                }
                if !cfg!(target_os = "linux") {
                    return Err(MemoryBuildError::ThpUnsupportedPlatform);
                }
            }
            if req.host_numa_node.is_some()
                && cfg!(not(any(target_os = "linux", target_os = "windows")))
            {
                return Err(MemoryBuildError::HostNumaNodeUnsupportedPlatform);
            }
            if req.hugepages {
                if !cfg!(target_os = "linux") {
                    return Err(MemoryBuildError::HugepagesUnsupportedPlatform);
                }
                if req.private_memory {
                    return Err(MemoryBuildError::HugepagesWithPrivateMemory);
                }
                if self.x86_legacy_support {
                    return Err(MemoryBuildError::HugepagesWithLegacy);
                }
            }
        }

        // Validate x86 legacy support: at least one backing must contain a
        // range starting at GPA 0 and covering at least 1MB.
        if self.x86_legacy_support {
            let has_low_mem = backing_requests.iter().any(|req| {
                req.ranges
                    .iter()
                    .any(|r| r.start() == 0 && r.end() >= 0x100000)
            });
            if !has_low_mem {
                return Err(MemoryBuildError::InvalidRamForX86);
            }
        }

        // Compute the maximum hugepage size across all backings (used for
        // VA alignment in the MappingManager).
        let max_hugepage_size = {
            let mut max: Option<usize> = None;
            for req in &backing_requests {
                if req.hugepages {
                    let size =
                        validate_hugepage_size(req.hugepage_size.unwrap_or(DEFAULT_HUGEPAGE_SIZE))?;
                    max = Some(max.map_or(size, |m: usize| m.max(size)));
                }
            }
            max
        };

        // Allocate per-backing memory and collect private ranges.
        let num_backings = backing_requests.len();
        let mut backings = Vec::with_capacity(num_backings);
        let mut private_ranges = Vec::new();
        for (i, req) in backing_requests.into_iter().enumerate() {
            let size: u64 = req.ranges.iter().map(|r| r.len()).sum();

            if req.private_memory {
                private_ranges.extend_from_slice(&req.ranges);
                backings.push(RamBacking {
                    mappable: None,
                    ranges: req.ranges,
                    prefetch: req.prefetch,
                    transparent_hugepages: req.transparent_hugepages,
                    host_numa_node: req.host_numa_node,
                });
                continue;
            }

            // Shared (file-backed) backing: reuse existing or allocate fresh.
            let mappable = if let Some(existing) = req.existing_mappable {
                existing
            } else {
                let backing_size: usize = size
                    .try_into()
                    .map_err(|_| MemoryBuildError::RamTooLarge(MemorySize(size)))?;
                let name = if num_backings == 1 {
                    "guest-ram".into()
                } else {
                    format!("guest-ram-{i}")
                };
                if req.hugepages {
                    let hugepage_size =
                        validate_hugepage_size(req.hugepage_size.unwrap_or(DEFAULT_HUGEPAGE_SIZE))?;
                    validate_hugepage_ram_alignment(size, &req.ranges, hugepage_size as u64)?;
                    sparse_mmap::alloc_shared_memory_hugetlb(
                        backing_size,
                        &name,
                        Some(hugepage_size),
                    )
                    .map_err(|error| MemoryBuildError::HugepageAllocationFailed {
                        size: MemorySize(size),
                        hugepage_size: MemorySize(hugepage_size as u64),
                        page_count: backing_size / hugepage_size,
                        error,
                    })?
                    .into()
                } else {
                    sparse_mmap::alloc_shared_memory(backing_size, &name)
                        .map_err(MemoryBuildError::AllocationFailed)?
                        .into()
                }
            };

            backings.push(RamBacking {
                mappable: Some(mappable),
                ranges: req.ranges,
                prefetch: req.prefetch,
                transparent_hugepages: false,
                host_numa_node: req.host_numa_node,
            });
        }

        // Spawn a thread to handle memory requests.
        //
        // FUTURE: move this to a task once the GuestMemory deadlocks are resolved.
        let (thread, spawner) = DefaultPool::spawn_on_thread("memory_manager");

        let vtl0_alias_map_offset = if let Some(offset) = self.vtl0_alias_map {
            if max_addr > offset {
                return Err(MemoryBuildError::AliasMapWontFit);
            }
            Some(offset)
        } else {
            None
        };

        let mapping_manager =
            MappingManager::new(&spawner, max_addr, private_ranges, max_hugepage_size);

        let va_mapper = mapping_manager
            .client()
            .new_mapper(true)
            .await
            .map_err(MemoryBuildError::VaMapper)?;

        let region_manager = RegionManager::new(&spawner, mapping_manager.client().clone());

        // Build RAM regions from each backing's ranges.
        let mut ram_regions = Vec::new();
        for backing in &backings {
            let mut file_offset = 0u64;
            for range in &backing.ranges {
                // Split for x86 legacy PAM/VGA regions if needed.
                let sub_ranges =
                    if self.x86_legacy_support && range.start() == 0 && range.end() >= 0x100000 {
                        let range_end = range.end();
                        let range_starts = [
                            0u64, 0xa0000, 0xc0000, 0xc4000, 0xc8000, 0xcc000, 0xd0000, 0xd4000,
                            0xd8000, 0xdc000, 0xe0000, 0xe4000, 0xe8000, 0xec000, 0xf0000,
                            0x100000, range_end,
                        ];
                        range_starts
                            .iter()
                            .zip(range_starts.iter().skip(1))
                            .map(|(&s, &e)| MemoryRange::new(s..e))
                            .collect::<Vec<_>>()
                    } else {
                        vec![*range]
                    };

                for sub_range in &sub_ranges {
                    let region = region_manager
                        .client()
                        .new_region(
                            "ram".into(),
                            *sub_range,
                            RAM_PRIORITY,
                            crate::region_manager::MappingType::Ram,
                        )
                        .await
                        .expect("regions cannot overlap yet");

                    if let Some(ref mappable) = backing.mappable {
                        region
                            .add_mapping(
                                MemoryRange::new(0..sub_range.len()),
                                mappable.clone(),
                                file_offset,
                                true,
                                backing.host_numa_node,
                            )
                            .await
                            .map_err(|error| MemoryBuildError::RamMapping {
                                range: *sub_range,
                                error,
                            })?;
                    } else {
                        va_mapper
                            .alloc_range(
                                sub_range.start() as usize,
                                sub_range.len() as usize,
                                backing.host_numa_node,
                            )
                            .map_err(|e| MemoryBuildError::PrivateRamAlloc(e, *sub_range))?;
                        va_mapper.set_range_name(
                            sub_range.start() as usize,
                            sub_range.len() as usize,
                            "guest-ram-private",
                        );

                        #[cfg(target_os = "linux")]
                        if backing.transparent_hugepages {
                            if let Err(e) = va_mapper.madvise_hugepage(
                                sub_range.start() as usize,
                                sub_range.len() as usize,
                            ) {
                                tracing::warn!(
                                    error = &e as &dyn std::error::Error,
                                    range = %sub_range,
                                    "failed to mark RAM as THP eligible"
                                );
                            }
                        }
                    }

                    region
                        .map(MapParams {
                            writable: true,
                            executable: true,
                            prefetch: backing.prefetch && backing.mappable.is_some(),
                        })
                        .await
                        .map_err(|error| MemoryBuildError::RamRegionEnable {
                            range: *sub_range,
                            error,
                        })?;

                    ram_regions.push(RamRegion {
                        range: *sub_range,
                        handle: region,
                    });
                    file_offset += sub_range.len();
                }
            }
        }

        let gm = GuestMemoryManager {
            guest_ram: backings,
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

    /// Returns the mappable, consuming this backing.
    pub fn into_mappable(self) -> Mappable {
        self.guest_ram
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
            self.mapping_manager.new_mapper(false).await?,
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
    /// The returned mappable can be passed back via
    /// [`RamBackingRequest::existing_mappable`] to create a new memory
    /// manager with the same memory state. Only one instance of this type
    /// should be managing a given memory backing at a time, though, or the
    /// guest may see unpredictable results.
    ///
    /// Returns `None` unless there is exactly one backing and it is
    /// file-backed. This currently means multi-backing and private-memory
    /// configurations cannot be restarted.
    pub fn shared_memory_backing(&self) -> Option<SharedMemoryBacking> {
        // Require exactly one backing, and it must be file-backed.
        if self.guest_ram.len() != 1 {
            return None;
        }
        Some(SharedMemoryBacking {
            guest_ram: self.guest_ram[0].mappable.clone()?,
        })
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
#[derive(Clone)]
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
pub enum RamVisibilityError {
    /// The range is not a controllable RAM region.
    #[error("{0} is not a controllable RAM range")]
    InvalidRange(MemoryRange),
    /// Failed to map the region.
    #[error("failed to map RAM range {range}")]
    Map {
        /// The range that failed.
        range: MemoryRange,
        /// The error.
        #[source]
        error: mesh::error::RemoteError,
    },
}

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
    ) -> Result<(), RamVisibilityError> {
        let region = self
            .regions
            .iter()
            .find(|region| region.range == range)
            .ok_or(RamVisibilityError::InvalidRange(range))?;

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
                    .map_err(|error| RamVisibilityError::Map { range, error })?;
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

    /// Build a GuestMemoryManager with the given backing range groups,
    /// and return a GuestMemory handle for read/write testing.
    async fn build_and_get_memory(
        backing_ranges: &[&[MemoryRange]],
    ) -> (GuestMemoryManager, GuestMemory) {
        let max_addr = backing_ranges
            .iter()
            .flat_map(|ranges| ranges.iter())
            .map(|r| r.end())
            .max()
            .unwrap_or(0);

        let mut builder = GuestMemoryBuilder::new();
        for ranges in backing_ranges {
            builder = builder.add_backing(RamBackingRequest::new(ranges.to_vec()));
        }
        let mgr = builder.build(max_addr).await.unwrap();
        let gm = mgr.client().guest_memory().await.unwrap();
        (mgr, gm)
    }

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

    #[test]
    fn test_single_backing() {
        DefaultPool::run_with(|_| async {
            let page = SparseMapping::page_size() as u64;
            let r = MemoryRange::new(0..4 * page);
            let (_mgr, gm) = build_and_get_memory(&[&[r]]).await;

            let pattern = vec![0xAB; page as usize];
            gm.write_at(0, &pattern).unwrap();
            let mut buf = vec![0u8; page as usize];
            gm.read_at(0, &mut buf).unwrap();
            assert_eq!(buf, pattern);

            // Second page should be zeroed.
            gm.read_at(page, &mut buf).unwrap();
            assert_eq!(buf, vec![0u8; page as usize]);
        });
    }

    #[test]
    fn test_two_backings() {
        DefaultPool::run_with(|_| async {
            let page = SparseMapping::page_size() as u64;
            let r0 = MemoryRange::new(0..2 * page);
            let r1 = MemoryRange::new(2 * page..4 * page);
            let (_mgr, gm) = build_and_get_memory(&[&[r0], &[r1]]).await;

            // Write distinct patterns into each backing's region.
            let pattern_a = vec![0xAA; page as usize];
            let pattern_b = vec![0xBB; page as usize];
            gm.write_at(0, &pattern_a).unwrap();
            gm.write_at(2 * page, &pattern_b).unwrap();

            let mut buf = vec![0u8; page as usize];
            gm.read_at(0, &mut buf).unwrap();
            assert_eq!(buf, pattern_a, "backing 0 should have pattern_a");

            gm.read_at(2 * page, &mut buf).unwrap();
            assert_eq!(buf, pattern_b, "backing 1 should have pattern_b");

            // Unwritten pages within each backing should be zeroed.
            gm.read_at(page, &mut buf).unwrap();
            assert_eq!(buf, vec![0u8; page as usize]);
            gm.read_at(3 * page, &mut buf).unwrap();
            assert_eq!(buf, vec![0u8; page as usize]);
        });
    }

    #[test]
    fn test_two_backings_different_sizes() {
        DefaultPool::run_with(|_| async {
            let page = SparseMapping::page_size() as u64;
            let r0 = MemoryRange::new(0..page);
            let r1 = MemoryRange::new(page..4 * page);
            let (_mgr, gm) = build_and_get_memory(&[&[r0], &[r1]]).await;

            let pattern_a = vec![0x11; page as usize];
            let pattern_b = vec![0x22; page as usize];
            gm.write_at(0, &pattern_a).unwrap();
            gm.write_at(page, &pattern_b).unwrap();

            let mut buf = vec![0u8; page as usize];
            gm.read_at(0, &mut buf).unwrap();
            assert_eq!(buf, pattern_a);
            gm.read_at(page, &mut buf).unwrap();
            assert_eq!(buf, pattern_b);

            // Last page of backing 1.
            let pattern_c = vec![0x33; page as usize];
            gm.write_at(3 * page, &pattern_c).unwrap();
            gm.read_at(3 * page, &mut buf).unwrap();
            assert_eq!(buf, pattern_c);

            // Middle page of backing 1 should be zeroed.
            gm.read_at(2 * page, &mut buf).unwrap();
            assert_eq!(buf, vec![0u8; page as usize]);
        });
    }

    #[test]
    fn test_two_backings_with_gap() {
        DefaultPool::run_with(|_| async {
            let page = SparseMapping::page_size() as u64;
            let r0 = MemoryRange::new(0..2 * page);
            let r1 = MemoryRange::new(4 * page..6 * page);

            let mgr = GuestMemoryBuilder::new()
                .add_backing(RamBackingRequest::new(vec![r0]))
                .add_backing(RamBackingRequest::new(vec![r1]))
                .build(r1.end())
                .await
                .unwrap();
            let gm = mgr.client().guest_memory().await.unwrap();

            let pattern_a = vec![0xCC; page as usize];
            let pattern_b = vec![0xDD; page as usize];
            gm.write_at(0, &pattern_a).unwrap();
            gm.write_at(4 * page, &pattern_b).unwrap();

            let mut buf = vec![0u8; page as usize];
            gm.read_at(0, &mut buf).unwrap();
            assert_eq!(buf, pattern_a);
            gm.read_at(4 * page, &mut buf).unwrap();
            assert_eq!(buf, pattern_b);
        });
    }
}
