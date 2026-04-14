// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for irqfd-based interrupt delivery.
//!
//! irqfd allows a hypervisor to directly inject an MSI into a guest when an
//! event is signaled, without involving userspace in the interrupt delivery
//! path. This is used for device passthrough (e.g., VFIO) where the physical
//! device signals an event and the hypervisor injects the corresponding MSI
//! into the guest VM.

use pal_event::Event;

/// Trait for partitions that support irqfd-based interrupt delivery.
///
/// An irqfd associates an event with a GSI (Global System Interrupt), and a
/// GSI routing table maps GSIs to MSI addresses and data values. When the
/// event is signaled, the kernel looks up the GSI routing and injects the
/// configured MSI into the guest without a usermode transition.
pub trait IrqFd: Send + Sync {
    /// Creates a new irqfd route.
    ///
    /// Allocates a GSI, creates an event, and registers the event with the
    /// hypervisor so that signaling it injects the configured MSI into the
    /// guest.
    ///
    /// The caller retrieves the event via [`IrqFdRoute::event`] to pass to
    /// VFIO or other interrupt sources.
    ///
    /// When the route is dropped, the irqfd is unregistered and the GSI is
    /// freed.
    fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>>;
}

/// A handle to a registered irqfd route.
///
/// Each route represents a single GSI with an associated event. When the
/// event is signaled (e.g., by VFIO on a device interrupt), the kernel injects
/// the MSI configured via [`set_msi`](IrqFdRoute::set_msi) into the guest.
///
/// Dropping this handle unregisters the irqfd and frees the GSI.
pub trait IrqFdRoute: Send + Sync {
    /// Returns the event that triggers interrupt injection when signaled.
    ///
    /// Pass this to VFIO `map_msix` or any other interrupt source. On Linux,
    /// this is an eventfd created by the implementation. On WHP (future), this
    /// is the event handle returned by `WHvCreateTrigger`.
    fn event(&self) -> &Event;

    /// Sets the MSI routing for this irqfd's GSI.
    ///
    /// `address` and `data` are the x86 MSI address and data values that the
    /// kernel will use when injecting the interrupt into the guest.
    fn set_msi(&self, address: u64, data: u32) -> anyhow::Result<()>;

    /// Clears the MSI routing for this irqfd's GSI.
    ///
    /// The irqfd remains registered but interrupt delivery is disabled until
    /// a new route is configured via [`set_msi`](IrqFdRoute::set_msi).
    fn clear_msi(&self) -> anyhow::Result<()>;

    /// Masks the route.
    ///
    /// While masked, interrupts arriving on the event are not injected into
    /// the guest. The caller should use [`consume_pending`](IrqFdRoute::consume_pending)
    /// to check whether an interrupt arrived while masked and store the
    /// result in the MSI-X PBA. On unmask, the caller should deliver any
    /// pending interrupt from the PBA before re-enabling the route.
    fn mask(&self) -> anyhow::Result<()>;

    /// Unmasks the route and re-enables interrupt injection.
    fn unmask(&self) -> anyhow::Result<()>;

    /// Drains the pending interrupt state and returns whether an interrupt
    /// was pending.
    ///
    /// This atomically reads and clears the event's counter. The caller
    /// should store the result in the MSI-X PBA (Pending Bit Array).
    /// Repeated calls after the first drain will return `false` until a
    /// new interrupt arrives, so the caller must persist the pending state
    /// externally (e.g., in the MSI-X emulator's PBA bits).
    fn consume_pending(&self) -> bool {
        self.event().try_wait()
    }
}
