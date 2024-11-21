// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition abstraction allowing different virtualization backends within
//! OpenHCL.
//!
//! This abstraction is similar to `HvlitePartition`, and should be merged with
//! it at some point. Right now, there are enough differences in requirements
//! that this is not practical.

#![warn(missing_docs)]

use core::ops::RangeInclusive;
use inspect::Inspect;
use inspect::InspectMut;
use std::sync::Arc;
use virt::Partition;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;

/// The VM partition.
pub trait OpenhclPartition: Send + Sync + Inspect {
    /// The current paravisor reference time.
    ///
    /// This is only here because we use an `Hcl` method to get it today. It
    /// should be moved elsewhere in the future, since this isn't really a
    /// partition concept.
    fn reference_time(&self) -> u64;

    /// The current VTL0 guest OS ID.
    fn vtl0_guest_os_id(&self) -> hvdef::hypercall::HvGuestOsId;

    /// Registers the range to be intercepted by the host directly, without the
    /// exit flowing through the paravisor.
    ///
    /// This is best effort. Exits for this range may still flow through the paravisor.
    fn register_host_io_port_fast_path(&self, range: RangeInclusive<u16>) -> Box<dyn Send>;

    /// Revokes support for guest VSM (i.e., VTL1) after the guest has started
    /// running.
    fn revoke_guest_vsm(&self) -> anyhow::Result<()>;

    /// Requests an MSI be delivered to the guest interrupt controller.
    fn request_msi(&self, vtl: hvdef::Vtl, request: virt::irqcon::MsiRequest);

    /// Returns the partition's capabilities.
    fn caps(&self) -> &virt::PartitionCapabilities;

    /// Returns the trait object for accessing the synic.
    fn into_synic(self: Arc<Self>) -> Arc<dyn virt::Synic>;

    /// Gets a line set target to trigger local APIC LINTs.
    ///
    /// The line number is the VP index times 2, plus the LINT number (0 or 1).
    #[cfg(guest_arch = "x86_64")]
    fn into_lint_target(
        self: Arc<Self>,
        vtl: hvdef::Vtl,
    ) -> Arc<dyn vmcore::line_interrupt::LineSetTarget>;

    /// Returns the interface for IO APIC routing.
    #[cfg(guest_arch = "x86_64")]
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting>;

    /// Returns the interface for VTL memory protection changes.
    fn into_vtl_memory_protection(
        self: Arc<Self>,
    ) -> Arc<dyn virt::VtlMemoryProtection + Send + Sync>;

    /// Sets the port to use for the PM timer assist. Reads of this port will be
    /// implemented by the hypervisor, using the reference time scaled to the
    /// appropriate frequency.
    ///
    /// This is best effort. Exits for reads of this port may still flow through
    /// the paravisor.
    fn set_pm_timer_assist(&self, port: Option<u16>) -> anyhow::Result<()>;

    /// Reads from an MMIO address by calling into the host.
    ///
    /// FUTURE: remove from the partition interface
    fn host_mmio_read(&self, addr: u64, data: &mut [u8]);

    /// Writes to an MMIO address by calling into the host.
    ///
    /// FUTURE: remove from the partition interface
    fn host_mmio_write(&self, addr: u64, data: &[u8]);

    /// Gets an interface for cancelling VPs.
    fn into_request_yield(self: Arc<Self>) -> Arc<dyn vmm_core::partition_unit::RequestYield>;
}

impl OpenhclPartition for virt_mshv_vtl::UhPartition {
    fn reference_time(&self) -> u64 {
        self.reference_time()
    }

    fn vtl0_guest_os_id(&self) -> hvdef::hypercall::HvGuestOsId {
        self.vtl0_guest_os_id()
    }

    fn register_host_io_port_fast_path(&self, range: RangeInclusive<u16>) -> Box<dyn Send> {
        Box::new(self.register_host_io_port_fast_path(range))
    }

    fn revoke_guest_vsm(&self) -> anyhow::Result<()> {
        self.revoke_guest_vsm()?;
        Ok(())
    }

    fn request_msi(&self, vtl: hvdef::Vtl, request: virt::irqcon::MsiRequest) {
        Partition::request_msi(self, vtl, request)
    }

    fn caps(&self) -> &virt::PartitionCapabilities {
        Partition::caps(self)
    }

    fn into_synic(self: Arc<Self>) -> Arc<dyn virt::Synic> {
        self
    }

    fn into_lint_target(
        self: Arc<Self>,
        vtl: hvdef::Vtl,
    ) -> Arc<dyn vmcore::line_interrupt::LineSetTarget> {
        Arc::new(virt::irqcon::ApicLintLineTarget::new(self, vtl))
    }

    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        virt::X86Partition::ioapic_routing(self)
    }

    fn into_vtl_memory_protection(
        self: Arc<Self>,
    ) -> Arc<dyn virt::VtlMemoryProtection + Send + Sync> {
        self
    }

    fn set_pm_timer_assist(&self, port: Option<u16>) -> anyhow::Result<()> {
        self.set_pm_timer_assist(port)?;
        Ok(())
    }

    fn host_mmio_read(&self, addr: u64, data: &mut [u8]) {
        self.host_mmio_read(addr, data);
    }

    fn host_mmio_write(&self, addr: u64, data: &[u8]) {
        self.host_mmio_write(addr, data);
    }

    fn into_request_yield(self: Arc<Self>) -> Arc<dyn vmm_core::partition_unit::RequestYield> {
        self
    }
}

impl OpenhclPartition for virt_kvm::KvmPartition {
    fn reference_time(&self) -> u64 {
        // TODO. This is just used for logging, so it's not critical.
        0
    }

    fn vtl0_guest_os_id(&self) -> hvdef::hypercall::HvGuestOsId {
        hvdef::hypercall::HvGuestOsId::new()
    }

    fn register_host_io_port_fast_path(&self, _range: RangeInclusive<u16>) -> Box<dyn Send> {
        Box::new(())
    }

    fn revoke_guest_vsm(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn request_msi(&self, vtl: hvdef::Vtl, request: virt::irqcon::MsiRequest) {
        Partition::request_msi(self, vtl, request)
    }

    fn caps(&self) -> &virt::PartitionCapabilities {
        Partition::caps(self)
    }

    fn into_synic(self: Arc<Self>) -> Arc<dyn virt::Synic> {
        self
    }

    fn into_lint_target(
        self: Arc<Self>,
        vtl: hvdef::Vtl,
    ) -> Arc<dyn vmcore::line_interrupt::LineSetTarget> {
        Arc::new(virt::irqcon::ApicLintLineTarget::new(self, vtl))
    }

    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        virt::X86Partition::ioapic_routing(self)
    }

    fn into_vtl_memory_protection(
        self: Arc<Self>,
    ) -> Arc<dyn virt::VtlMemoryProtection + Send + Sync> {
        Arc::new(IgnoreProtectionChanges)
    }

    fn set_pm_timer_assist(&self, _port: Option<u16>) -> anyhow::Result<()> {
        Ok(())
    }

    fn host_mmio_read(&self, _addr: u64, _data: &mut [u8]) {
        unimplemented!()
    }

    fn host_mmio_write(&self, _addr: u64, _data: &[u8]) {
        unimplemented!()
    }

    fn into_request_yield(self: Arc<Self>) -> Arc<dyn vmm_core::partition_unit::RequestYield> {
        self
    }
}

struct IgnoreProtectionChanges;

impl virt::VtlMemoryProtection for IgnoreProtectionChanges {
    fn modify_vtl_page_setting(
        &self,
        _pfn: u64,
        _flags: hvdef::HvMapGpaFlags,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A wrapper around a `virt::Processor` that does not support save/restore.
#[derive(InspectMut)]
#[inspect(transparent, bound = "T: InspectMut")]
pub struct NoSaveVp<T>(#[inspect(mut)] pub T);

impl<T: virt::Processor> virt::Processor for NoSaveVp<T> {
    type Error = T::Error;
    type RunVpError = T::RunVpError;

    type StateAccess<'a> = T::StateAccess<'a>
    where
        Self: 'a
    ;

    fn set_debug_state(
        &mut self,
        vtl: hvdef::Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> Result<(), Self::Error> {
        self.0.set_debug_state(vtl, state)
    }

    async fn run_vp(
        &mut self,
        stop: virt::StopVp<'_>,
        dev: &impl virt::io::CpuIo,
    ) -> Result<std::convert::Infallible, virt::VpHaltReason<Self::RunVpError>> {
        self.0.run_vp(stop, dev).await
    }

    fn flush_async_requests(&mut self) -> Result<(), Self::RunVpError> {
        self.0.flush_async_requests()
    }

    fn access_state(&mut self, vtl: hvdef::Vtl) -> Self::StateAccess<'_> {
        self.0.access_state(vtl)
    }
}

impl<T> SaveRestore for NoSaveVp<T> {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
        Err(vmcore::save_restore::SaveError::NotSupported)
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        match state {}
    }
}
