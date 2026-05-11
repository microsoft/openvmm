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
/// the MSI configured via [`enable`](IrqFdRoute::enable) into the guest.
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
    /// `address` and `data` are the MSI address and data values that the
    /// hypervisor will use when injecting the interrupt into the guest.
    fn enable(&self, address: u64, data: u32);

    /// Disables the MSI routing for this irqfd's GSI.
    ///
    /// Disarms the irqfd so that signaling the event no longer injects an
    /// interrupt. Interrupts that arrive while disabled remain pending on
    /// the event and will be delivered when [`enable`](IrqFdRoute::enable)
    /// is called, or can be drained by waiting on the event directly.
    fn disable(&self);
}
