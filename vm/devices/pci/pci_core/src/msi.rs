// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use crate::bus_range::AssignedBusRange;
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
pub struct MsiRoute {
    inner: Box<dyn IrqFdRoute>,
    default_bdf: DefaultBdf,
}

impl MsiRoute {
    /// Returns the event that triggers interrupt injection when signaled.
    ///
    /// Pass this to VFIO `map_msix` or any other interrupt source.
    pub fn event(&self) -> &Event {
        self.inner.event()
    }

    /// Configures the MSI address and data for this route, using
    /// the route's default BDF as the requester ID.
    pub fn enable(&self, address: u64, data: u32) {
        let resolved = resolve_default_bdf(&self.default_bdf);
        self.inner.enable(address, data, Some(resolved))
    }

    /// Configures the MSI address and data for this route, using
    /// an explicit segment-local BDF (`rid`) as the requester ID.
    ///
    /// Use this for multi-function devices whose functions span
    /// multiple buses: the caller composes the full `(bus << 8) | devfn`
    /// itself from whatever bus range it owns. The route's own
    /// default BDF is bypassed entirely.
    pub fn enable_with_rid(&self, rid: u16, address: u64, data: u32) {
        self.inner.enable(address, data, Some(rid.into()))
    }

    /// Disables the MSI route. Interrupts that arrive while disabled
    /// remain pending on the event and will be delivered when
    /// [`enable`](Self::enable) is called, or can be drained via
    /// [`consume_pending`](Self::consume_pending).
    pub fn disable(&self) {
        self.inner.disable()
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

/// Default BDF source for MSI device identification.
///
/// [`MsiTarget::signal_msi`] uses this to compose the requester ID
/// from the port's secondary bus combined with the configured `devfn`.
#[derive(Clone, Debug)]
struct DefaultBdf {
    bus_range: AssignedBusRange,
    devfn: u8,
}

/// Resolves a BDF from a [`DefaultBdf`] source.
fn resolve_default_bdf(default: &DefaultBdf) -> u32 {
    let (secondary, _) = default.bus_range.bus_range();
    (secondary as u32) << 8 | default.devfn as u32
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
    default_bdf: DefaultBdf,
}

impl MsiTarget {
    /// Returns a disconnected MSI target with a dummy BDF.
    ///
    /// Useful in tests and contexts where MSI delivery is not needed.
    pub fn disconnected() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MsiTargetInner {
                signal_msi: Arc::new(DisconnectedMsiTarget),
                irqfd: None,
            })),
            default_bdf: DefaultBdf {
                bus_range: AssignedBusRange::new(),
                devfn: 0,
            },
        }
    }
}

impl std::fmt::Debug for MsiTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiTarget")
            .field("default_bdf", &self.default_bdf)
            .finish()
    }
}

struct MsiTargetInner {
    signal_msi: Arc<dyn SignalMsi>,
    irqfd: Option<Arc<dyn IrqFd>>,
}

impl std::fmt::Debug for MsiTargetInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            signal_msi: _,
            irqfd,
        } = self;
        f.debug_struct("MsiTargetInner")
            .field("has_irqfd", &irqfd.is_some())
            .finish()
    }
}

impl MsiConnection {
    /// Creates a new disconnected MSI target connection.
    ///
    /// `bus_range` and `devfn` configure the default BDF identity
    /// for this connection. When a device signals an MSI via
    /// [`MsiTarget::signal_msi`], the BDF is resolved from the bus
    /// range's secondary bus and the given `devfn`.
    pub fn new(bus_range: AssignedBusRange, devfn: u8) -> Self {
        Self {
            target: MsiTarget {
                inner: Arc::new(RwLock::new(MsiTargetInner {
                    signal_msi: Arc::new(DisconnectedMsiTarget),
                    irqfd: None,
                })),
                default_bdf: DefaultBdf { bus_range, devfn },
            },
        }
    }

    /// Updates the MSI target to which this connection signals interrupts.
    pub fn connect(&self, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.target.inner.write();
        inner.signal_msi = signal_msi;
    }

    /// Sets the [`IrqFd`] for kernel-mediated MSI route allocation.
    ///
    /// When present, [`MsiTarget::new_route`] can create [`MsiRoute`]
    /// instances for direct interrupt delivery.
    pub fn connect_irqfd(&self, irqfd: Arc<dyn IrqFd>) {
        let mut inner = self.target.inner.write();
        inner.irqfd = Some(irqfd);
    }

    /// Returns the MSI target for this connection.
    pub fn target(&self) -> &MsiTarget {
        &self.target
    }
}

impl MsiTarget {
    /// Returns a new `MsiTarget` sharing the same connection and bus
    /// range but with the given `devfn` in the default BDF.
    ///
    /// Use this to derive per-port targets: create one target per
    /// bus range, then call `with_devfn(port_number)` to get a
    /// target that resolves to `(bus << 8) | devfn`.
    pub fn with_devfn(&self, devfn: u8) -> MsiTarget {
        MsiTarget {
            inner: self.inner.clone(),
            default_bdf: DefaultBdf {
                bus_range: self.default_bdf.bus_range.clone(),
                devfn,
            },
        }
    }

    /// Returns a new `MsiTarget` sharing the same connection but with
    /// a different bus range and devfn.
    ///
    /// Use this when a component (e.g. a PCIe switch) needs to derive
    /// targets using a bus range it owns rather than the parent's.
    pub fn with_bus_range(&self, bus_range: AssignedBusRange, devfn: u8) -> MsiTarget {
        MsiTarget {
            inner: self.inner.clone(),
            default_bdf: DefaultBdf { bus_range, devfn },
        }
    }

    /// Signals an MSI interrupt to this target, using this target's
    /// default BDF as the requester ID.
    pub fn signal_msi(&self, address: u64, data: u32) {
        let resolved = resolve_default_bdf(&self.default_bdf);
        let inner = self.inner.read();
        inner.signal_msi.signal_msi(Some(resolved), address, data);
    }

    /// Signals an MSI interrupt to this target, using an explicit
    /// segment-local BDF (`rid`) as the requester ID.
    ///
    /// Use this for multi-function devices whose functions span
    /// multiple buses: the caller composes the full `(bus << 8) | devfn`
    /// itself from whatever bus range it owns. This target's own
    /// default BDF is bypassed entirely.
    pub fn signal_msi_with_rid(&self, rid: u16, address: u64, data: u32) {
        let inner = self.inner.read();
        inner.signal_msi.signal_msi(Some(rid.into()), address, data);
    }

    /// Creates a new kernel-mediated MSI route for direct interrupt
    /// delivery.
    ///
    /// The route inherits this target's default BDF source so that
    /// [`MsiRoute::enable`] resolves the BDF the same way
    /// [`signal_msi`](Self::signal_msi) does.
    ///
    /// Returns `None` if no [`IrqFd`] has been connected.
    pub fn new_route(&self) -> Option<anyhow::Result<MsiRoute>> {
        let inner = self.inner.read();
        inner.irqfd.as_ref().map(|fd| {
            Ok(MsiRoute {
                inner: fd.new_irqfd_route()?,
                default_bdf: self.default_bdf.clone(),
            })
        })
    }

    /// Returns the default BDF that will be used by
    /// [`signal_msi`](Self::signal_msi) and [`MsiRoute::enable`].
    pub fn default_bdf(&self) -> u32 {
        resolve_default_bdf(&self.default_bdf)
    }

    /// Returns whether this target supports direct MSI routes.
    pub fn supports_direct_msi(&self) -> bool {
        let inner = self.inner.read();
        inner.irqfd.is_some()
    }
}
