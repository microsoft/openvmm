// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for signaling MSI interrupts.
//!
//! An MSI is, in hardware, a bus-mastered memory write a device performs to a
//! platform-defined address. [`SignalMsi`] models the platform routing that
//! delivers that write, either immediately in user mode or by pre-registering
//! a caller-owned event for kernel-mediated delivery.

use pal_event::Event;
use std::sync::Arc;
use vmcore::irqfd::KernelMsiBinding;
use vmcore::irqfd::RetranslateInterrupts;

/// A resolved MSI destination, valid until its registered consumer is invalidated.
#[derive(Clone)]
pub struct ResolvedMsi {
    /// Destination after address and identity translation.
    pub target: Arc<dyn SignalMsi>,
    /// Translated device identity.
    pub devid: Option<u32>,
    /// Translated destination address.
    pub address: u64,
    /// Translated interrupt data.
    pub data: u32,
}

/// Result of preparing an MSI destination without delivering a transaction.
pub enum MsiResolution {
    /// Use the original destination and parameters without translation.
    Direct,
    /// Cache this translated destination until invalidation.
    Resolved(ResolvedMsi),
    /// No destination could be prepared. No guest fault has been reported.
    /// Actual software delivery must retry through `signal_msi`.
    Failed,
}

/// An object that can signal MSI interrupts.
///
/// This is the fabric's outbound-MSI decode seam. It has two forms of the same
/// operation, distinguished only by *when* the target is resolved:
///
/// - [`signal_msi`](Self::signal_msi) delivers a message *now*, in usermode.
/// - [`bind_msi`](Self::bind_msi) pre-registers a caller-owned fd so the
///   *kernel* injects the same message when the fd is later signaled, with no
///   usermode transition (device passthrough, e.g. VFIO).
///
/// Both decode the address the same way; `bind_msi` is just the
/// kernel-accelerated binding time of `signal_msi`. Emulated sinks implement
/// only `signal_msi` and inherit the default `bind_msi` (which returns `None`,
/// so callers fall back to `signal_msi`).
pub trait SignalMsi: Send + Sync {
    /// Whether an event-backed source can notify userspace without a kernel
    /// binding. This describes the source transport, not software injection.
    ///
    /// Translating wrappers must preserve this capability. Kernel-only sources
    /// must remain unbound, rather than be polled, if preparation fails.
    fn supports_eventfd_fallback(&self) -> bool {
        true
    }

    /// Registers a cached consumer before its first lookup.
    ///
    /// Translating wrappers must forward this to any inner translating layer.
    /// Configuration changes must invalidate consumers after publishing the new
    /// state, without holding locks needed by resolution or delivery.
    fn register_msi_route(&self, route: &Arc<dyn RetranslateInterrupts>) {
        let _ = route;
    }

    /// Prepares a destination for cached delivery and optional kernel binding.
    ///
    /// Must not deliver an interrupt or report guest-visible faults: this runs
    /// on route programming and invalidation, even if no interrupt ever fires.
    /// A successful result must preserve userspace and kernel delivery semantics.
    fn resolve_msi(&self, devid: Option<u32>, address: u64, data: u32) -> MsiResolution {
        let _ = (devid, address, data);
        MsiResolution::Direct
    }

    /// Signals a message-signaled interrupt at the specified address with the specified data.
    ///
    /// `devid` is an optional device identity. Its meaning is layer-dependent:
    /// at the device layer it is a BDF for multi-function devices (`None` for
    /// single-function); at the ITS wrapper layer it is the fully composed ITS
    /// device ID; backends that don't need it ignore it.
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32);

    /// Pre-registers `fd` so that the hypervisor injects this MSI directly when
    /// the fd is signaled, without a usermode transition.
    ///
    /// Returns `Some(binding)` if the address decodes to a kernel-serviceable
    /// sink (the returned [`KernelMsiBinding`] owns the kernel registration and
    /// unbinds on drop), or `None` if it does not. Userspace event delivery is
    /// possible only when [`supports_eventfd_fallback`](Self::supports_eventfd_fallback)
    /// permits it.
    ///
    /// The default returns `None`: emulated sinks are not kernel-serviceable.
    fn bind_msi(
        &self,
        fd: &Event,
        devid: Option<u32>,
        address: u64,
        data: u32,
    ) -> Option<Box<dyn KernelMsiBinding>> {
        let _ = (fd, devid, address, data);
        None
    }
}
