// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Object-safe traits for interacting with the hypervisor.
//!
//! These traits wrap non-object-safe traits provided by the [`virt`] crate.
//! Although the `virt` traits could be made object safe, having this
//! abstraction between the consumer and producer side may make it easier to
//! refactor and share code between the hypervisors without having to change all
//! the client code, which may be beneficial.
//!
//! If this ends up not being true, then this layer should probably be removed.

use anyhow::Context as _;
use async_trait::async_trait;
use guestmem::DoorbellRegistration;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use pci_core::msi::SignalMsi;
use std::convert::Infallible;
use std::sync::Arc;
#[cfg(guest_arch = "aarch64")]
use virt::Aarch64Partition as ArchPartition;
use virt::PageVisibility;
use virt::Partition;
use virt::PartitionAccessState;
use virt::PartitionCapabilities;
use virt::PartitionMemoryMap;
use virt::PartitionMemoryMapper;
use virt::Processor;
use virt::StopVp;
use virt::Synic;
use virt::VpHaltReason;
#[cfg(guest_arch = "x86_64")]
use virt::X86Partition as ArchPartition;
use virt::io::CpuIo;
#[cfg(guest_arch = "x86_64")]
use virt::irqcon::MsiRequest;
use virt::vm::AccessVmState;
use virt::vm::VmSavedState;
use virt::vp::AccessVpState;
use virt::vp::VpSavedState;
#[cfg(guest_arch = "x86_64")]
use vmcore::line_interrupt::LineSetTarget;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmm_core::partition_unit::RequestYield;
use vmm_core::partition_unit::RunCancelled;
use vmm_core::partition_unit::VmPartition;
use vmm_core::partition_unit::VpRunner;

/// A base partition, with methods needed at rutnime along with methods to initialize the vm.
pub trait HvlitePartition: Inspect + Send + Sync + RequestYield + Synic {
    /// Gets a line set target to trigger local APIC LINTs.
    ///
    /// The line number is the VP index times 2, plus the LINT number (0 or 1).
    #[cfg(guest_arch = "x86_64")]
    fn into_lint_target(self: Arc<Self>, vtl: Vtl) -> Arc<dyn LineSetTarget>;

    /// Gets the partition capabilities.
    fn caps(&self) -> &PartitionCapabilities;

    /// Returns a structure that implements the [`VmPartition`] trait.
    fn into_vm_partition(self: Arc<Self>) -> WrappedPartition;

    /// Gets the [`PartitionMemoryMap`] interface for `vtl`.
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn PartitionMemoryMap>;

    /// Requests an MSI be delivered to `vtl`.
    #[cfg(guest_arch = "x86_64")]
    fn request_msi(&self, vtl: Vtl, request: MsiRequest);

    /// Gets the [`virt::irqcon::IoApicRouting`] interface.
    #[cfg(guest_arch = "x86_64")]
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting>;

    /// Gets the [`virt::irqcon::ControlGic`] interface.
    #[cfg(guest_arch = "aarch64")]
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic>;

    /// Gets the [`DoorbellRegistration`] interface for a particular VTL.
    fn into_doorbell_registration(
        self: Arc<Self>,
        minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>>;

    /// Gets the [`SignalMsi`] interface for a particular VTL.
    fn into_signal_msi(self: Arc<Self>, minimum_vtl: Vtl) -> Option<Arc<dyn SignalMsi>>;

    /// Returns whether virtual devices are supported.
    fn supports_virtual_devices(&self) -> bool;

    /// Creates a new VPCI virtual device.
    fn new_virtual_device(&self, vtl: Vtl, device_id: u64) -> anyhow::Result<Arc<dyn VpciDevice>>;

    /// Returns whether partition reset is supported.
    fn supports_reset(&self) -> bool;

    /// Returns the reference time source.
    fn reference_time_source(&self) -> Option<ReferenceTimeSource>;

    /// Gets an interface to support downcasting to specific partition types.
    ///
    /// TODO: remove this.
    #[cfg(all(windows, feature = "virt_whp"))]
    fn as_any(&self) -> &dyn std::any::Any;
}

pub trait BasicPartitionStateAccess: 'static + Send + Sync + Inspect {
    fn save(&self) -> anyhow::Result<VmSavedState>;
    fn restore(&self, state: VmSavedState) -> anyhow::Result<()>;
    fn reset(&self) -> anyhow::Result<()>;
    fn scrub_vtl(&self, vtl: Vtl) -> anyhow::Result<()>;
    fn accept_initial_pages(&self, pages: Vec<(MemoryRange, PageVisibility)>)
    -> anyhow::Result<()>;
}

impl<T: Partition + PartitionAccessState> BasicPartitionStateAccess for T {
    fn save(&self) -> anyhow::Result<VmSavedState> {
        let vm = self
            .access_state(Vtl::Vtl0)
            .save_all()
            .context("saving vm state")?;

        Ok(vm)
    }

    fn restore(&self, state: VmSavedState) -> anyhow::Result<()> {
        self.access_state(Vtl::Vtl0)
            .restore_all(&state)
            .context("restoring vm state")?;

        Ok(())
    }

    fn reset(&self) -> anyhow::Result<()> {
        self.supports_reset()
            .context("reset not supported")?
            .reset()?;
        Ok(())
    }

    fn scrub_vtl(&self, vtl: Vtl) -> anyhow::Result<()> {
        self.supports_vtl_scrub()
            .context("scrub vtl not supported")?
            .scrub(vtl)?;
        Ok(())
    }

    fn accept_initial_pages(
        &self,
        pages: Vec<(MemoryRange, PageVisibility)>,
    ) -> anyhow::Result<()> {
        self.supports_initial_accept_pages()
            .context("accept pages not supported")?
            .accept_initial_pages(&pages)?;
        Ok(())
    }
}

impl<T> HvlitePartition for T
where
    T: BasicPartitionStateAccess + ArchPartition + PartitionMemoryMapper + Synic,
{
    #[cfg(guest_arch = "x86_64")]
    fn into_lint_target(self: Arc<Self>, vtl: Vtl) -> Arc<dyn LineSetTarget> {
        Arc::new(vmm_core::emuplat::apic::ApicLintLineTarget::new(self, vtl))
    }

    fn caps(&self) -> &PartitionCapabilities {
        self.caps()
    }

    fn into_vm_partition(self: Arc<Self>) -> WrappedPartition {
        WrappedPartition(self)
    }

    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn PartitionMemoryMap> {
        self.memory_mapper(vtl)
    }

    #[cfg(guest_arch = "x86_64")]
    fn request_msi(&self, vtl: Vtl, request: MsiRequest) {
        self.request_msi(vtl, request)
    }

    #[cfg(guest_arch = "x86_64")]
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        self.ioapic_routing()
    }

    #[cfg(guest_arch = "aarch64")]
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        self.control_gic(vtl)
    }

    fn into_doorbell_registration(
        self: Arc<Self>,
        minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        self.doorbell_registration(minimum_vtl)
    }

    fn into_signal_msi(self: Arc<Self>, minimum_vtl: Vtl) -> Option<Arc<dyn SignalMsi>> {
        self.as_signal_msi(minimum_vtl)
    }

    fn supports_virtual_devices(&self) -> bool {
        self.new_virtual_device().is_some()
    }

    fn new_virtual_device(&self, vtl: Vtl, device_id: u64) -> anyhow::Result<Arc<dyn VpciDevice>> {
        Ok(Arc::new(
            self.new_virtual_device()
                .context("virtual devices not supported by this VM")?
                .build(vtl, device_id)?,
        ))
    }

    fn supports_reset(&self) -> bool {
        self.supports_reset().is_some()
    }

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        self.reference_time_source()
    }

    #[cfg(all(windows, feature = "virt_whp"))]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Wrapper struct that implements [`VmPartition`].
#[derive(InspectMut)]
#[inspect(transparent)]
pub struct WrappedPartition(Arc<dyn BasicPartitionStateAccess>);

#[async_trait]
impl VmPartition for WrappedPartition {
    fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset()
    }

    fn scrub_vtl(&mut self, vtl: Vtl) -> anyhow::Result<()> {
        self.0.scrub_vtl(vtl)
    }

    fn accept_initial_pages(
        &mut self,
        pages: Vec<(MemoryRange, PageVisibility)>,
    ) -> anyhow::Result<()> {
        self.0.accept_initial_pages(pages)
    }
}

impl SaveRestore for WrappedPartition {
    type SavedState = VmSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        self.0.save().map_err(SaveError::Other)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        self.0.restore(state).map_err(RestoreError::Other)
    }
}

/// The hypervisor portion of a VPCI device.
pub trait VpciDevice {
    /// Gets the [`VpciInterruptMapper`] interface to create interrupt mapping
    /// table entries.
    fn interrupt_mapper(self: Arc<Self>) -> VpciInterruptMapper;

    /// Gets the [`SignalMsi`] interface to signal interrupts.
    fn target(self: Arc<Self>) -> Arc<dyn SignalMsi>;
}

impl<T: 'static + MapVpciInterrupt + SignalMsi> VpciDevice for T {
    fn interrupt_mapper(self: Arc<Self>) -> VpciInterruptMapper {
        VpciInterruptMapper::new(self)
    }

    fn target(self: Arc<Self>) -> Arc<dyn SignalMsi> {
        self
    }
}

struct WrappedVp<'a, T>(&'a mut T);

impl<T: InspectMut> InspectMut for WrappedVp<'_, T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.0.inspect_mut(req)
    }
}

impl<T: Processor> Processor for WrappedVp<'_, T> {
    type StateAccess<'a>
        = T::StateAccess<'a>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        vtl: Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <T::StateAccess<'_> as AccessVpState>::Error> {
        self.0.set_debug_state(vtl, state)
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        self.0.run_vp(stop, dev).await
    }

    fn flush_async_requests(&mut self) {
        self.0.flush_async_requests()
    }

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        self.0.access_state(vtl)
    }

    fn vtl_inspectable(&self, vtl: Vtl) -> bool {
        self.0.vtl_inspectable(vtl)
    }
}

impl<T: Processor> SaveRestore for WrappedVp<'_, T> {
    type SavedState = VpSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        // Ensure all async requests are reflected in the saved state.
        self.0.flush_async_requests();

        self.0
            .access_state(Vtl::Vtl0)
            .save_all()
            .map_err(|err| SaveError::Other(err.into()))
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        self.0
            .access_state(Vtl::Vtl0)
            .restore_all(&state)
            .map_err(|err| RestoreError::Other(err.into()))
    }
}

pub trait BindHvliteVp: Send {
    fn bind<'a>(&'a mut self) -> anyhow::Result<Box<dyn 'a + HvliteVp>>;
}

impl<T: virt::BindProcessor + Send> BindHvliteVp for T {
    fn bind<'a>(&'a mut self) -> anyhow::Result<Box<dyn 'a + HvliteVp>> {
        Ok(Box::new(virt::BindProcessor::bind(self)?))
    }
}

#[async_trait(?Send)]
pub trait HvliteVp {
    async fn run(
        &mut self,
        runner: VpRunner,
        chipset: &vmm_core::vmotherboard_adapter::ChipsetPlusSynic,
    );
}

#[async_trait(?Send)]
impl<T: Processor> HvliteVp for T {
    async fn run(
        &mut self,
        mut runner: VpRunner,
        chipset: &vmm_core::vmotherboard_adapter::ChipsetPlusSynic,
    ) {
        while let Err(RunCancelled { .. }) = runner.run(&mut WrappedVp(self), chipset).await {}
    }
}
