// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use pal_event::Event;
use parking_lot::RwLock;
use std::sync::Arc;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;

/// An object that can signal MSI interrupts.
pub trait SignalMsi: Send + Sync {
    /// Signals a message-signaled interrupt at the specified address with the specified data.
    ///
    /// `devid` is an optional device identity. Its meaning is layer-dependent:
    /// at the device layer it is a BDF for multi-function devices (`None` for
    /// single-function); at the ITS wrapper layer it is the fully composed ITS
    /// device ID; backends that don't need it ignore it.
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32);
}

/// A kernel-mediated MSI interrupt route for a single vector.
///
/// Each route has an associated event. Signaling the event causes the
/// hypervisor to inject the configured MSI into the guest without a
/// userspace transition. This is used for device passthrough (VFIO)
/// where the physical device signals the event on interrupt.
pub struct MsiRoute(Box<dyn IrqFdRoute>);

impl MsiRoute {
    /// Wraps a boxed [`IrqFdRoute`] into a concrete route.
    pub fn new(backing: Box<dyn IrqFdRoute>) -> Self {
        Self(backing)
    }

    /// Returns the event that triggers interrupt injection when signaled.
    ///
    /// Pass this to VFIO `map_msix` or any other interrupt source.
    pub fn event(&self) -> &Event {
        self.0.event()
    }

    /// Configures the MSI address and data for this route.
    ///
    /// `address` and `data` are the MSI address and data values that
    /// the hypervisor will use when injecting the interrupt.
    pub fn enable(&self, address: u64, data: u32) {
        self.0.enable(address, data, None)
    }

    /// Configures the MSI address and data for this route.
    ///
    /// `rid` is the PCIe requester ID (RID) of the device that will signal the
    /// interrupt. `address` and `data` are the MSI address and data values that
    /// the hypervisor will use when injecting the interrupt.
    pub fn enable_with_rid(&self, address: u64, data: u32, rid: u16) {
        self.0.enable(address, data, Some(rid.into()))
    }

    /// Disables the MSI route. Interrupts that arrive while disabled
    /// remain pending on the event and will be delivered when
    /// [`enable`](Self::enable) is called, or can be drained via
    /// [`consume_pending`](Self::consume_pending).
    pub fn disable(&self) {
        self.0.disable()
    }

    /// Drains pending interrupt state and returns whether an interrupt
    /// was pending while the route was masked.
    pub fn consume_pending(&self) -> bool {
        self.event().try_wait()
    }
}

struct DisconnectedMsiTarget;

impl SignalMsi for DisconnectedMsiTarget {
    fn signal_msi(&self, _devid: Option<u32>, _address: u64, _data: u32) {
        tracelimit::warn_ratelimited!("dropped MSI interrupt to disconnected target");
    }
}

/// A connection between a device and an MSI target.
#[derive(Debug)]
pub struct MsiConnection {
    target: MsiTarget,
}

/// An MSI target that can be used to signal MSI interrupts.
#[derive(Clone)]
pub struct MsiTarget {
    inner: Arc<RwLock<MsiTargetInner>>,
    irqfd: Option<Arc<dyn IrqFd>>,
}

impl std::fmt::Debug for MsiTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiTarget")
            .field("has_irqfd", &self.irqfd.is_some())
            .finish()
    }
}

struct MsiTargetInner {
    signal_msi: Arc<dyn SignalMsi>,
}

impl std::fmt::Debug for MsiTargetInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { signal_msi: _ } = self;
        f.debug_struct("MsiTargetInner").finish()
    }
}

impl MsiConnection {
    /// Creates a new disconnected MSI target connection.
    pub fn new() -> Self {
        Self {
            target: MsiTarget {
                inner: Arc::new(RwLock::new(MsiTargetInner {
                    signal_msi: Arc::new(DisconnectedMsiTarget),
                })),
                irqfd: None,
            },
        }
    }

    /// Creates a new disconnected MSI target connection with an
    /// [`IrqFd`] for kernel-mediated MSI route allocation.
    ///
    /// When present, [`MsiTarget::new_route`] can create [`MsiRoute`]
    /// instances for direct interrupt delivery.
    pub fn with_irqfd(irqfd: Arc<dyn IrqFd>) -> Self {
        Self {
            target: MsiTarget {
                inner: Arc::new(RwLock::new(MsiTargetInner {
                    signal_msi: Arc::new(DisconnectedMsiTarget),
                })),
                irqfd: Some(irqfd),
            },
        }
    }

    /// Updates the MSI target to which this connection signals interrupts.
    pub fn connect(&self, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.target.inner.write();
        inner.signal_msi = signal_msi;
    }

    /// Returns the MSI target for this connection.
    pub fn target(&self) -> &MsiTarget {
        &self.target
    }
}

impl MsiTarget {
    /// Signals an MSI interrupt to this target.
    pub fn signal_msi(&self, address: u64, data: u32) {
        let inner = self.inner.read();
        inner.signal_msi.signal_msi(None, address, data);
    }

    /// Signals an MSI interrupt to this target from a specific RID.
    pub fn signal_msi_with_rid(&self, rid: u16, address: u64, data: u32) {
        let inner = self.inner.read();
        inner.signal_msi.signal_msi(Some(rid.into()), address, data);
    }

    /// Creates a new kernel-mediated MSI route for direct interrupt
    /// delivery.
    ///
    /// Returns `None` if this target was not configured with an
    /// [`IrqFd`].
    pub fn new_route(&self) -> Option<anyhow::Result<MsiRoute>> {
        self.irqfd
            .as_ref()
            .map(|fd| Ok(MsiRoute::new(fd.new_irqfd_route()?)))
    }

    /// Returns whether this target supports direct MSI routes.
    pub fn supports_direct_msi(&self) -> bool {
        self.irqfd.is_some()
    }
}
