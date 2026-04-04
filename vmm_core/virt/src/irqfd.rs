// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for irqfd-based interrupt delivery.
//!
//! irqfd allows a hypervisor to directly inject an MSI into a guest when an
//! eventfd is signaled, without involving userspace in the interrupt delivery
//! path. This is used for device passthrough (e.g., VFIO) where the physical
//! device signals an eventfd and the hypervisor injects the corresponding MSI
//! into the guest VM.

use pal_event::Event;

/// Trait for partitions that support irqfd-based interrupt delivery.
///
/// An irqfd associates an eventfd with a GSI (Global System Interrupt), and a
/// GSI routing table maps GSIs to MSI addresses and data values. When the
/// eventfd is signaled, the kernel looks up the GSI routing and injects the
/// configured MSI into the guest without a userspace transition.
pub trait IrqFd: Send + Sync {
    /// Creates a new irqfd route for the given event.
    ///
    /// This allocates a GSI and registers the event's underlying file descriptor
    /// as an irqfd with the hypervisor kernel module. The returned route handle
    /// can be used to set or update the MSI routing for this GSI.
    ///
    /// When the route is dropped, the irqfd is unregistered and the GSI is freed.
    fn new_irqfd_route(&self, event: &Event) -> anyhow::Result<Box<dyn IrqFdRoute>>;
}

/// A handle to a registered irqfd route.
///
/// Each route represents a single GSI with an associated eventfd. When the
/// eventfd is signaled (e.g., by VFIO on a device interrupt), the kernel injects
/// the MSI configured via [`set_msi`](IrqFdRoute::set_msi) into the guest.
///
/// Dropping this handle unregisters the irqfd and frees the GSI.
pub trait IrqFdRoute: Send + Sync {
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
}
