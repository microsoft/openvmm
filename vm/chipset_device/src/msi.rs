// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for signaling MSI interrupts.
//!
//! An MSI is, in hardware, a bus-mastered memory write a device performs to a
//! platform-defined address. [`SignalMsi`] models the platform routing that
//! delivers that write, either immediately in user mode or by pre-registering
//! a caller-owned event for kernel-mediated delivery.

use pal_event::Event;
use vmcore::irqfd::KernelMsiBinding;

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
    /// unbinds on drop), or `None` if it does not — in which case the caller
    /// should deliver via [`signal_msi`](Self::signal_msi) in usermode instead.
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
