// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM implementation of the virt::generic interfaces.

#![cfg(all(target_os = "linux", guest_is_native))]
#![expect(missing_docs)]
// UNSAFETY: Calling KVM APIs and manually managing memory.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

mod arch;
mod gsi;
mod memory;
#[cfg(guest_arch = "x86_64")]
mod snp;

pub use arch::Kvm;

use guestmem::GuestMemory;
use inspect::Inspect;
use memory::KvmMemoryBackingMode;
use memory::KvmMemoryRangeState;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::sync::Arc;
use thiserror::Error;
use virt::state::StateError;

/// Returns whether KVM is available on this machine.
pub fn is_available() -> Result<bool, KvmError> {
    match std::fs::metadata("/dev/kvm") {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(KvmError::AvailableCheck(err)),
    }
}

use arch::KvmVpInner;
#[cfg(guest_arch = "x86_64")]
use snp::SnpLaunchState;
use std::sync::atomic::Ordering;
use virt::InitialPageImportType;
use virt::VpIndex;
use vmcore::vmtime::VmTimeAccess;

#[derive(Error, Debug)]
pub enum KvmError {
    #[error("operation not supported")]
    NotSupported,
    #[error("vtl2 is not supported on this hypervisor")]
    Vtl2NotSupported,
    #[error("isolation is not supported on this hypervisor")]
    IsolationNotSupported,
    #[error("failed to open /dev/sev")]
    OpenSev(#[source] std::io::Error),
    #[error("unsupported SNP launch page import type: {0:?}")]
    UnsupportedSnpPageImportType(InitialPageImportType),
    #[error("kvm error")]
    Kvm(#[from] kvm::Error),
    #[error("failed to stat /dev/kvm")]
    AvailableCheck(#[source] std::io::Error),
    #[error(transparent)]
    State(#[from] Box<StateError<KvmError>>),
    #[error("invalid state while restoring: {0}")]
    InvalidState(&'static str),
    #[error("unsupported isolation configuration: {0}")]
    UnsupportedIsolationConfiguration(&'static str),
    #[error("cannot resize KVM guest_memfd memory slot")]
    CannotResizeGuestMemfdSlot,
    #[error("private memory range is not contained in guest_memfd private memory")]
    InvalidPrivateMemoryRange,
    #[error("SNP launch range is not page aligned")]
    UnalignedSnpLaunchRange,
    #[error("SNP launch range is not contained in guestmemfd private memory")]
    InvalidSnpLaunchRange,
    #[error("too many CPUID entries for SNP launch page: {0}")]
    TooManySnpCpuidEntries(usize),
    #[error("invalid KVM_HC_MAP_GPA_RANGE request")]
    InvalidMapGpaRange,
    #[error("unsupported KVM_HC_MAP_GPA_RANGE attributes: {0:#x}")]
    UnsupportedMapGpaRangeAttributes(u64),
    #[error("failed to discard shared backing after private conversion")]
    DiscardSharedBacking(#[source] std::io::Error),
    #[error("failed to discard private backing after shared conversion")]
    DiscardPrivateBacking(#[source] std::io::Error),
    #[error("SNP launch is already in progress")]
    SnpLaunchInProgress,
    #[error("SNP launch previously failed")]
    SnpLaunchFailed,
    #[error("misaligned gic base address")]
    Misaligned,
    #[error("host does not support GICv2 or GICv3")]
    NoGic,
    #[error("host does not support required cpu capabilities")]
    Capabilities(virt::PartitionCapabilitiesError),
    #[cfg(guest_arch = "x86_64")]
    #[error("nested virtualization was requested but the host does not support it")]
    NestedVirtUnsupported,
    #[cfg(guest_arch = "x86_64")]
    #[error("unsupported CPU vendor")]
    UnsupportedCpuVendor,
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to compute topology cpuid")]
    TopologyCpuid(#[source] virt::x86::topology::UnknownVendor),
}

#[derive(Inspect)]
pub struct KvmPartition {
    #[inspect(flatten)]
    inner: Arc<KvmPartitionInner>,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    synic_ports: Arc<virt::synic::SynicPorts<KvmPartitionInner>>,
    #[inspect(skip)]
    irqfd_state: Arc<gsi::KvmIrqFdState>,
}

#[derive(Inspect)]
struct KvmPartitionInner {
    #[inspect(skip)]
    kvm: kvm::Partition,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    sev: Option<std::fs::File>,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    snp_launch_state: Mutex<SnpLaunchState>,
    memory: Mutex<KvmMemoryRangeState>,
    memory_backing_mode: KvmMemoryBackingMode,
    #[inspect(iter_by_index)]
    ram_ranges: Vec<MemoryRange>,
    hv1_enabled: bool,
    gm: GuestMemory,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    bsp_cpuid: Vec<kvm::kvm_cpuid_entry2>,
    #[inspect(skip)]
    vps: Vec<KvmVpInner>,
    #[inspect(skip)]
    gsi_routing: Mutex<gsi::GsiRouting>,
    caps: virt::PartitionCapabilities,

    // This is used for debugging via Inspect
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,

    #[cfg(guest_arch = "x86_64")]
    reserved_vps_per_socket: u32,

    /// Whether the host allows advertising `MCG_CMCI_P` in the guest's
    /// `IA32_MCG_CAP` (required for KVM to expose the CMCI LVT register).
    #[cfg(guest_arch = "x86_64")]
    mce_cmci_supported: bool,

    /// The GIC device fd, kept alive for the VM lifetime.
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    _gic_device: kvm::Device,
    /// The ITS device fd, kept alive for the VM lifetime.
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    _its_device: Option<kvm::Device>,
    /// MSI controller configuration (v2m, ITS, or none).
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    gic_msi: vm_topology::processor::aarch64::GicMsiController,
    /// Total configured GIC interrupt count (SGIs + PPIs + SPIs).
    #[cfg(guest_arch = "aarch64")]
    gic_nr_irqs: u32,
    #[cfg(guest_arch = "x86_64")]
    synic_ports: virt::synic::SynicPortMap,
}

// TODO: Chunk this up into smaller types.
#[derive(Debug, Error)]
enum KvmRunVpError {
    #[error("KVM internal error: {0:#x}")]
    InternalError(u32),
    #[error("invalid vp state")]
    InvalidVpState,
    #[error("failed to run VP")]
    Run(#[source] kvm::Error),
    #[error("unhandled system event type: {0:#x}")]
    UnhandledSystemEvent(u32),
    #[cfg(guest_arch = "x86_64")]
    #[error("unhandled KVM hypercall: nr={nr:#x}, flags={flags:#x}")]
    UnhandledHypercall { nr: u64, flags: u64 },
    #[cfg(guest_arch = "x86_64")]
    #[error(
        "SEV guest requested termination: ghcb_msr={ghcb_msr:#x} reason_set={reason_set:#x} reason={reason:#x}"
    )]
    SevTermination {
        ghcb_msr: u64,
        reason_set: u64,
        reason: u64,
    },
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to inject an extint interrupt")]
    ExtintInterrupt(#[source] kvm::Error),
}

pub struct KvmProcessorBinder {
    partition: Arc<KvmPartitionInner>,
    vpindex: VpIndex,
    vmtime: VmTimeAccess,
}

impl KvmPartitionInner {
    #[cfg(guest_arch = "x86_64")]
    fn bsp(&self) -> &KvmVpInner {
        &self.vps[0]
    }

    fn vp(&self, vp_index: VpIndex) -> Option<&KvmVpInner> {
        self.vps.get(vp_index.index() as usize)
    }

    fn evaluate_vp(&self, vp_index: VpIndex) {
        let Some(vp) = self.vp(vp_index) else { return };
        vp.set_eval(true, Ordering::Relaxed);

        #[cfg(guest_arch = "x86_64")]
        self.kvm.vp(vp.vp_info().apic_id).force_exit();

        #[cfg(guest_arch = "aarch64")]
        self.kvm.vp(vp.vp_info().base.vp_index.index()).force_exit();
    }
}
