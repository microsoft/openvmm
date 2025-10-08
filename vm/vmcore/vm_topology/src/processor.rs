// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor topology types.

pub mod aarch64;
pub mod x86;

cfg_if::cfg_if! {
    if #[cfg(guest_arch = "aarch64")] {
        pub use aarch64 as arch;
        pub use aarch64::Aarch64Topology as TargetTopology;
        pub use aarch64::Aarch64VpInfo as TargetVpInfo;
    } else if #[cfg(guest_arch = "x86_64")] {
        pub use x86 as arch;
        pub use x86::X86Topology as TargetTopology;
        pub use x86::X86VpInfo as TargetVpInfo;
    } else {
        compile_error!("Unsupported architecture");
    }
}
use thiserror::Error;

/// A description of the VM's processor topology.
///
/// Currently this just tracks the APIC IDs for the processors.
///
/// Build one with [`TopologyBuilder`].
#[cfg_attr(
    feature = "inspect",
    derive(inspect::Inspect),
    inspect(bound = "T: inspect::Inspect, T::ArchVpInfo: inspect::Inspect")
)]
#[derive(Debug, Clone)]
pub struct ProcessorTopology<T: ArchTopology = TargetTopology> {
    #[cfg_attr(feature = "inspect", inspect(iter_by_index))]
    vps: Vec<T::ArchVpInfo>,
    smt_enabled: bool,
    vps_per_socket: u32,
    arch: T,
}

/// Architecture-specific topology types.
pub trait ArchTopology: Sized {
    /// The architecture-specific VP info type.
    type ArchVpInfo: Copy + AsRef<VpInfo>;

    /// The architecture-specific [`TopologyBuilder`] generic.
    type BuilderState;

    /// Compute VP topology from a VP.
    fn vp_topology(topology: &ProcessorTopology<Self>, info: &Self::ArchVpInfo) -> VpTopologyInfo;
}

/// A builder for [`ProcessorTopology`].
#[derive(Debug)]
pub struct TopologyBuilder<T: ArchTopology> {
    vps_per_socket: u32,
    smt_enabled: bool,
    arch: T::BuilderState,
}

/// Error returned by [`TopologyBuilder::from_host_topology`].
#[derive(Debug, Error)]
pub enum HostTopologyError {
    /// Could not find the host topology.
    #[error("could not compute host topology via cpuid")]
    NotFound,
    /// The host topology has more than 2 threads per core.
    #[error("unsupported thread-per-core count {0}")]
    UnsupportedThreadsPerCore(u32),
}

/// Error when building a [`ProcessorTopology`].
#[derive(Debug, Error)]
pub enum InvalidTopology {
    /// Failed to configure at least one VP.
    #[error("must have at least one processor")]
    NoVps,
    /// Too many virtual processors.
    #[error("too many processors requested: {requested}, max {max}")]
    TooManyVps {
        /// The number of processors requested.
        requested: u32,
        /// The maximum number of processors.
        max: u32,
    },
    /// Not all processors will be addressable in XAPIC mode.
    #[error("too many processors or too high an APIC ID {0} for xapic mode")]
    ApicIdLimitExceeded(u32),
    /// VpInfo indices must be linear and start at 0
    #[error("vp indices don't start at 0 or don't count up")]
    InvalidVpIndices,
    /// Failed to query the topology information from Device Tree.
    #[error("failed to query memory topology from device tree")]
    StdIoError(#[source] std::io::Error),
}

impl<T: ArchTopology> TopologyBuilder<T> {
    /// Sets the number of VPs per socket.
    ///
    /// This does not need to be a power of 2, but it should be a multiple of 2
    /// if SMT is enabled.
    ///
    /// The number of VPs per socket will be rounded up to a power of 2 for
    /// purposes of defining the x2APIC ID.
    pub fn vps_per_socket(&mut self, count: u32) -> &mut Self {
        self.vps_per_socket = count.clamp(1, 32768);
        self
    }

    /// Sets whether SMT (hyperthreading) is enabled.
    ///
    /// This is ignored if `vps_per_socket` is 1.
    pub fn smt_enabled(&mut self, enabled: bool) -> &mut Self {
        self.smt_enabled = enabled;
        self
    }
}

impl<
    #[cfg(feature = "inspect")] T: ArchTopology + inspect::Inspect,
    #[cfg(not(feature = "inspect"))] T: ArchTopology,
> ProcessorTopology<T>
{
    /// Returns the number of VPs.
    pub fn vp_count(&self) -> u32 {
        self.vps.len() as u32
    }

    /// Returns information for the given processor by VP index.
    ///
    /// Panics if the VP index is out of range.
    pub fn vp(&self, vp_index: VpIndex) -> VpInfo {
        *self.vps[vp_index.index() as usize].as_ref()
    }

    /// Returns information for the given processor by VP index, including
    /// architecture-specific information.
    ///
    /// Panics if the VP index is out of range.
    pub fn vp_arch(&self, vp_index: VpIndex) -> T::ArchVpInfo {
        self.vps[vp_index.index() as usize]
    }

    /// Returns an iterator over all VPs.
    pub fn vps(&self) -> impl '_ + ExactSizeIterator<Item = VpInfo> + Clone {
        self.vps.iter().map(|vp| *vp.as_ref())
    }

    /// Returns an iterator over all VPs, including architecture-specific information.
    pub fn vps_arch(&self) -> impl '_ + ExactSizeIterator<Item = T::ArchVpInfo> + Clone {
        self.vps.iter().copied()
    }

    /// Returns whether SMT (hyperthreading) is enabled.
    pub fn smt_enabled(&self) -> bool {
        self.smt_enabled
    }

    /// Returns the number of VPs per socket.
    ///
    /// This will always be a power of 2. The number of VPs actually populated
    /// in a socket may be smaller than this.
    pub fn reserved_vps_per_socket(&self) -> u32 {
        self.vps_per_socket.next_power_of_two()
    }

    /// Computes the processor topology information for a VP.
    pub fn vp_topology(&self, vp_index: VpIndex) -> VpTopologyInfo {
        T::vp_topology(self, &self.vp_arch(vp_index))
    }
}

/// Per-processor topology information.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[derive(Debug, Copy, Clone)]
pub struct VpInfo {
    /// The VP index of the processor.
    pub vp_index: VpIndex,
    /// The virtual NUMA node of the processor.
    pub vnode: u32,
}

impl AsRef<VpInfo> for VpInfo {
    fn as_ref(&self) -> &VpInfo {
        self
    }
}

impl VpInfo {
    /// Returns true if this is the BSP.
    pub fn is_bsp(&self) -> bool {
        self.vp_index.is_bsp()
    }
}

/// Topology information about a virtual processor.
pub struct VpTopologyInfo {
    /// The socket index.
    pub socket: u32,
    /// The core index within the socket.
    pub core: u32,
    /// The thread index within the core.
    pub thread: u32,
}

/// The virtual processor index.
///
/// This value is used inside the VMM to identify the processor. It is expected
/// to be used as an index into processor arrays, so it starts at zero and has
/// no gaps.
///
/// VP index zero is special in that it is always present and is always the BSP.
///
/// The same value is exposed to the guest operating system as the HV VP index,
/// via the Microsoft hypervisor guest interface. This constrains the HV VP
/// index to start at zero and have no gaps, which is not required by the
/// hypervisor interface, but it matches the behavior of Hyper-V and is not a
/// practical limitation.
///
/// This value is distinct from the APIC ID, although they are often the same
/// for all processors in small VMs and some in large VMs. Be careful not to use
/// them interchangeably.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect), inspect(transparent))]
pub struct VpIndex(u32);

impl VpIndex {
    /// Returns `index` as a VP index.
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// VP index zero, corresponding to the boot processor (BSP).
    ///
    /// Note that this being a constant means that the BSP's HV VP index
    /// observed by the guest will always be zero. This is consistent with
    /// Hyper-V and is not a practical limitation.
    ///
    /// Note that the APIC ID of the BSP might not be zero.
    pub const BSP: Self = Self::new(0);

    /// Returns the VP index value.
    pub fn index(&self) -> u32 {
        self.0
    }

    /// Returns true if this is the index of the BSP (0).
    pub fn is_bsp(&self) -> bool {
        *self == Self::BSP
    }
}
