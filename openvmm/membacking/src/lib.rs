// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This implements OpenVMM's memory manager.
//!
//! Externally, this is managed by the [`GuestMemoryManager`] object.
//!
//! Internally, guest memory is made up of a set of _regions_, each of which has
//! a fixed length and can be dynamically mapped and unmapped into guest
//! physical address space. Regions have priority--if a higher priority region
//! overlaps a lower priority one, the higher priority region will be mapped
//! into the VM.
//!
//! Each region contains a set of _mappings_, which specify the mapping of
//! portion of an OS mappable object (file descriptor on Unix, section handle on
//! Windows) to an offset within the region. Many regions will have exactly one
//! mapping, but some may have a dynamic set of mappings; for example, virtiofs
//! dynamically maps and unmaps files into a pre-allocated MMIO region to
//! support DAX.
//!
//! The regions and their mappings are maintained by the region manager
//! (`RegionManager`). Its job is to determine the currently active set of
//! mappings, which it sends to the mapping manager (`MappingManager`).
//!
//! The mapping manager tracks the virtual address space mappers (`VaMapper`),
//! which are used to maintain a linear virtual address space for guest memory
//! access by VMM processes. The mapping manager is responsible for keeping
//! track of which VA mappers have which mappings, to send mappings to the
//! mappers upon request, and to send requests to tear down those mappings when
//! the set of active mappings changes.
//!
//! The VA mappers implement the
//! [`GuestMemoryAccess`](guestmem::GuestMemoryAccess) trait and can be
//! used to back [`GuestMemory`](guestmem::GuestMemory).
//!
//! The region manager also keeps a list of partition memory mappers. These are
//! used to map and unmap regions from GPA space. The partitions do not care
//! about the individual mappings but instead just reference the linear virtual
//! address space maintained by a VA mapper.
//!
//! Currently, there are some holes in this implementation:
//!
//! * Guest VSM (multiple VTLs) is not supported. There is some basic support
//!   for mapping memory into both a VTL0 and a VTL2 partition, and for mapping
//!   a VTL0 alias map into VTL2, but there is no support for page protections
//!   or per-VTL [`GuestMemory`](guestmem::GuestMemory) objects.
//!
//!   Supporting this could be implemented via separate VA spaces for VTL0 and
//!   VTL2 memory. However, eventually we will use the hypervisor's
//!   implementation of a combined partition for VTL0 and VTL2 (and VTL1), which
//!   will require handling this very differently.
//!
//! * There is no support for locking memory. We obviously have to "support"
//!   this because the lock APIs exist on `GuestMemory`, but currently we can
//!   tear down mappings while they are locked. This has two side effects:
//!
//!   1. We can segfault in normal Rust code if the guest does something to
//!      unmap some memory that is locked. E.g., it can cause us to segfault in
//!      vmbus code accessing a ring buffer. This is not a memory safety issue
//!      but it is certainly undesirable.
//!
//!   2. The guest might be able to access or mutate memory after the VMM has
//!      torn it down, e.g. by issuing a storage IO to it concurrently with
//!      unmapping it. The exact implications of this are not easy to reason
//!      about.

mod mapping_manager;
mod memory_manager;
mod partition_mapper;
mod region_manager;

#[cfg(windows)]
mod sys {
    pub type RemoteProcess = Box<dyn 'static + std::os::windows::io::AsHandle + Send + Sync>;
}

#[cfg(unix)]
mod sys {
    pub enum RemoteProcess {}
}

/// A remote process handle.
///
/// This is used when specifying the process to use for mapping memory into a
/// partition. This is necessary because on Windows, it is not possible to map
/// memory from a single process into multiple partitions.
///
/// On Unix, this is an empty (uninhabitable) enum.
pub type RemoteProcess = sys::RemoteProcess;

pub use memory_manager::DeviceMemoryMapper;
pub use memory_manager::GuestMemoryBuilder;
pub use memory_manager::GuestMemoryClient;
pub use memory_manager::GuestMemoryManager;
pub use memory_manager::MemoryBuildError;
pub use memory_manager::PartitionAttachError;
pub use memory_manager::RamVisibility;
pub use memory_manager::RamVisibilityControl;
pub use memory_manager::SharedMemoryBacking;
