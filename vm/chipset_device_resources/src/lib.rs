// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for chipset devices.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use guestmem::GuestMemory;
use inspect::InspectMut;
use std::ops::RangeInclusive;
use vm_resource::CanResolveTo;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::ProtobufSaveRestore;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vmtime::VmTimeSource;

/// A unique identifier for a line set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineSetId(&'static str);

impl LineSetId {
    /// Returns the name of the line set.
    pub fn name(&self) -> &str {
        self.0
    }
}

/// Line set for device interrupts connected to the platform interrupt
/// controller.
pub const IRQ_LINE_SET: LineSetId = LineSetId("irq");
/// Line set for ACPI general purpose events.
pub const GPE0_LINE_SET: LineSetId = LineSetId("gpe0");
/// Line set for the BSP's local interrupts (LINT0/1) on x86.
pub const BSP_LINT_LINE_SET: LineSetId = LineSetId("bsp_lint");

impl CanResolveTo<ResolvedChipsetDevice> for ChipsetDeviceHandleKind {
    type Input<'a> = ResolveChipsetDeviceHandleParams<'a>;
}

/// Parameters used when resolving a resource with kind
/// [`ChipsetDeviceHandleKind`].
pub struct ResolveChipsetDeviceHandleParams<'a> {
    /// The name of the device.
    pub device_name: &'a str,
    /// Guest memory for device DMA operations for untrusted devices.
    pub guest_memory: &'a GuestMemory,
    /// Guest memory for device DMA operations for trusted devices, which can
    /// access encrypted memory on CVMs.
    ///
    /// For non-CVMs, this is the same as `guest_memory`.
    pub encrypted_guest_memory: &'a GuestMemory,
    /// The VM time source.
    pub vmtime: &'a VmTimeSource,
    /// Whether the VM is restoring from a saved state.
    ///
    /// FUTURE: remove this once devices have a state transition for "first
    /// boot". Device authors: try to avoid taking a dependency on this. If
    /// possible, delay any "first boot" initialization until it's really
    /// needed.
    pub is_restoring: bool,
    /// An object to confiure the chipset device's connection to the platform.
    pub configure: &'a mut dyn ConfigureChipsetDevice,
    /// The task driver source for the VM.
    pub task_driver_source: &'a VmTaskDriverSource,
    /// Object to register for MMIO intercepts.
    pub register_mmio: &'a mut (dyn chipset_device::mmio::RegisterMmioIntercept + Send),
    /// Object to register for PIO intercepts.
    pub register_pio: &'a mut (dyn chipset_device::pio::RegisterPortIoIntercept + Send),
}

/// A trait for configuring a chipset device's connection to the platform.
pub trait ConfigureChipsetDevice: Send {
    /// Creates a new line interrupt.
    fn new_line(&mut self, id: LineSetId, name: &str, vector: u32) -> LineInterrupt;

    /// Adds this device as a target for a range of line interrupts.
    fn add_line_target(
        &mut self,
        id: LineSetId,
        source_range: RangeInclusive<u32>,
        target_start: u32,
    );

    /// Tags this device so that its save/restore routines will not be called.
    fn omit_saved_state(&mut self);
}

#[async_trait]
trait DynChipsetDevice: ChipsetDevice + ProtobufSaveRestore + InspectMut {
    fn start(&mut self);
    async fn stop(&mut self);
    async fn reset(&mut self);
}

#[async_trait]
impl<T: ChangeDeviceState + ChipsetDevice + ProtobufSaveRestore + InspectMut> DynChipsetDevice
    for T
{
    fn start(&mut self) {
        self.start()
    }
    async fn stop(&mut self) {
        self.stop().await
    }
    async fn reset(&mut self) {
        self.reset().await
    }
}

/// A resolved chipset device resource.
pub struct ResolvedChipsetDevice(pub ErasedChipsetDevice);

/// A type-erased [`ChipsetDevice`].
#[derive(InspectMut)]
#[inspect(transparent(mut))]
pub struct ErasedChipsetDevice(Box<dyn DynChipsetDevice>);

impl<T: ChangeDeviceState + ChipsetDevice + ProtobufSaveRestore + InspectMut> From<T>
    for ResolvedChipsetDevice
{
    fn from(value: T) -> Self {
        Self(ErasedChipsetDevice(Box::new(value)))
    }
}

impl ChangeDeviceState for ErasedChipsetDevice {
    fn start(&mut self) {
        self.0.start()
    }

    async fn stop(&mut self) {
        self.0.stop().await
    }

    async fn reset(&mut self) {
        self.0.reset().await
    }
}

impl ChipsetDevice for ErasedChipsetDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn chipset_device::pio::PortIoIntercept> {
        self.0.supports_pio()
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn chipset_device::mmio::MmioIntercept> {
        self.0.supports_mmio()
    }

    fn supports_pci(&mut self) -> Option<&mut dyn chipset_device::pci::PciConfigSpace> {
        self.0.supports_pci()
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn chipset_device::poll_device::PollDevice> {
        self.0.supports_poll_device()
    }

    fn supports_line_interrupt_target(
        &mut self,
    ) -> Option<&mut dyn chipset_device::interrupt::LineInterruptTarget> {
        self.0.supports_line_interrupt_target()
    }

    fn supports_handle_eoi(&mut self) -> Option<&mut dyn chipset_device::interrupt::HandleEoi> {
        self.0.supports_handle_eoi()
    }

    fn supports_acknowledge_pic_interrupt(
        &mut self,
    ) -> Option<&mut dyn chipset_device::interrupt::AcknowledgePicInterrupt> {
        self.0.supports_acknowledge_pic_interrupt()
    }

    fn supports_tdisp(&mut self) -> Option<&mut dyn tdisp::TdispHostDeviceTarget> {
        self.0.supports_tdisp()
    }
}

impl ProtobufSaveRestore for ErasedChipsetDevice {
    fn save(
        &mut self,
    ) -> Result<vmcore::save_restore::SavedStateBlob, vmcore::save_restore::SaveError> {
        self.0.save()
    }

    fn restore(
        &mut self,
        state: vmcore::save_restore::SavedStateBlob,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        self.0.restore(state)
    }
}
