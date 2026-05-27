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

pub use arch::Kvm;

use guestmem::GuestMemory;
use inspect::Inspect;
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
use hvdef::Vtl;
use std::sync::atomic::Ordering;
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
    #[error("kvm error")]
    Kvm(#[from] kvm::Error),
    #[error("failed to stat /dev/kvm")]
    AvailableCheck(#[source] std::io::Error),
    #[error(transparent)]
    State(#[from] Box<StateError<KvmError>>),
    #[error("invalid state while restoring: {0}")]
    InvalidState(&'static str),
    #[error("misaligned gic base address")]
    Misaligned,
    #[error("host does not support GICv2 or GICv3")]
    NoGic,
    #[error("host does not support required cpu capabilities")]
    Capabilities(virt::PartitionCapabilitiesError),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to compute topology cpuid")]
    TopologyCpuid(#[source] virt::x86::topology::UnknownVendor),
}

#[derive(Debug, Inspect)]
struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    ranges: Vec<Option<KvmMemoryRange>>,
}

#[derive(Inspect)]
pub struct KvmPartition {
    #[inspect(flatten)]
    inner: Arc<KvmPartitionInner>,
    #[inspect(skip)]
    synic_ports: Arc<virt::synic::SynicPorts<KvmPartitionInner>>,
    #[inspect(skip)]
    irqfd_state: Arc<gsi::KvmIrqFdState>,
}

#[derive(Inspect)]
struct KvmPartitionInner {
    #[inspect(skip)]
    kvm: kvm::Partition,
    memory: Mutex<KvmMemoryRangeState>,
    hv1_enabled: bool,
    gm: GuestMemory,
    #[inspect(skip)]
    vps: Vec<KvmVpInner>,
    #[inspect(skip)]
    gsi_routing: Mutex<gsi::GsiRouting>,
    caps: virt::PartitionCapabilities,

    // This is used for debugging via Inspect
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,

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
    #[cfg_attr(guest_arch = "x86_64", expect(dead_code))]
    #[error("unhandled system event type: {0:#x}")]
    UnhandledSystemEvent(u32),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to inject an extint interrupt")]
    ExtintInterrupt(#[source] kvm::Error),
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
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

    /// # Safety
    ///
    /// `data..data+size` must be and remain an allocated VA range until the
    /// partition is destroyed or the region is unmapped.
    unsafe fn map_region(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        readonly: bool,
    ) -> anyhow::Result<()> {
        let mut state = self.memory.lock();

        // Memory slots cannot be resized but can be moved within the guest
        // address space. Find the existing slot if there is one.
        let mut slot_to_use = None;
        for (slot, range) in state.ranges.iter_mut().enumerate() {
            match range {
                Some(range) if range.host_addr == data => {
                    slot_to_use = Some(slot);
                    break;
                }
                Some(_) => (),
                None => slot_to_use = Some(slot),
            }
        }
        if slot_to_use.is_none() {
            slot_to_use = Some(state.ranges.len());
            state.ranges.push(None);
        }
        let slot_to_use = slot_to_use.unwrap();
        unsafe {
            self.kvm
                .set_user_memory_region(slot_to_use as u32, data, size, addr, readonly)?
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range: MemoryRange::new(addr..addr + size as u64),
        });
        Ok(())
    }
}

impl virt::PartitionMemoryMapper for KvmPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.inner.clone()
    }
}

// TODO: figure out a better abstraction that works for both KVM and WHP.
impl virt::PartitionMemoryMap for KvmPartitionInner {
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        _exec: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: guaranteed by caller.
        unsafe { self.map_region(data, size, addr, !writable) }
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size);
        let mut state = self.memory.lock();
        for (slot, entry) in state.ranges.iter_mut().enumerate() {
            let Some(kvm_range) = entry else { continue };
            if range.contains(&kvm_range.range) {
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                unsafe {
                    self.kvm.set_user_memory_region(
                        slot as u32,
                        std::ptr::null_mut(),
                        0,
                        0,
                        false,
                    )?;
                }
                *entry = None;
            } else {
                assert!(
                    !range.overlaps(&kvm_range.range),
                    "can only unmap existing ranges of exact size"
                );
            }
        }
        Ok(())
    }
}
