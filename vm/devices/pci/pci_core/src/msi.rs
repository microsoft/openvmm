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
    /// default `devfn` is bypassed.
    ///
    /// The bus portion of `rid` is validated against the route's
    /// assigned bus range; if it falls outside the range the route
    /// is left disabled and a ratelimited warning is emitted.
    pub fn enable_with_rid(&self, rid: u16, address: u64, data: u32) {
        let bus = (rid >> 8) as u8;
        if !self.default_bdf.bus_range.contains_bus(bus) {
            let (secondary, subordinate) = self.default_bdf.bus_range.bus_range();
            tracelimit::warn_ratelimited!(
                rid,
                secondary,
                subordinate,
                "refusing to enable MSI route: rid bus outside assigned bus range"
            );
            self.inner.disable();
            return;
        }
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
    /// default `devfn` is bypassed.
    ///
    /// The bus portion of `rid` is validated against this target's
    /// assigned bus range; if it falls outside the range the MSI is
    /// dropped and a ratelimited warning is emitted.
    pub fn signal_msi_with_rid(&self, rid: u16, address: u64, data: u32) {
        let bus = (rid >> 8) as u8;
        if !self.default_bdf.bus_range.contains_bus(bus) {
            let (secondary, subordinate) = self.default_bdf.bus_range.bus_range();
            tracelimit::warn_ratelimited!(
                rid,
                secondary,
                subordinate,
                "dropping MSI: rid bus outside assigned bus range"
            );
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus_range::AssignedBusRange;
    use pal_event::Event;
    use parking_lot::Mutex;
    use std::collections::VecDeque;

    /// A [`SignalMsi`] mock that records `(devid, address, data)`.
    struct RecordingSignalMsi {
        calls: Mutex<VecDeque<(Option<u32>, u64, u32)>>,
    }

    impl RecordingSignalMsi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(VecDeque::new()),
            })
        }

        fn pop(&self) -> Option<(Option<u32>, u64, u32)> {
            self.calls.lock().pop_front()
        }
    }

    impl SignalMsi for RecordingSignalMsi {
        fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
            self.calls.lock().push_back((devid, address, data));
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum RouteCall {
        Enable {
            address: u64,
            data: u32,
            devid: Option<u32>,
        },
        Disable,
    }

    struct MockIrqFdRoute {
        event: Event,
        calls: Arc<Mutex<Vec<RouteCall>>>,
    }

    impl IrqFdRoute for MockIrqFdRoute {
        fn event(&self) -> &Event {
            &self.event
        }

        fn enable(&self, address: u64, data: u32, devid: Option<u32>) {
            self.calls.lock().push(RouteCall::Enable {
                address,
                data,
                devid,
            });
        }

        fn disable(&self) {
            self.calls.lock().push(RouteCall::Disable);
        }
    }

    fn mock_irqfd(count: usize) -> (Arc<dyn IrqFd>, Vec<Arc<Mutex<Vec<RouteCall>>>>) {
        let mut call_logs = Vec::new();
        let route_params = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..count {
            let calls = Arc::new(Mutex::new(Vec::new()));
            call_logs.push(calls.clone());
            route_params.lock().push(calls);
        }

        struct MockIrqFd {
            routes: Mutex<Vec<Arc<Mutex<Vec<RouteCall>>>>>,
        }
        impl IrqFd for MockIrqFd {
            fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>> {
                let calls = self.routes.lock().remove(0);
                Ok(Box::new(MockIrqFdRoute {
                    event: Event::new(),
                    calls,
                }))
            }
        }

        (
            Arc::new(MockIrqFd {
                routes: Mutex::new(call_logs.clone()),
            }),
            call_logs,
        )
    }

    #[test]
    fn signal_msi_resolves_default_bdf() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0x18); // devfn = dev 3, fn 0
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        msi_conn.target().signal_msi(0xFEE0_0000, 42);

        let (devid, addr, data) = recorder.pop().unwrap();
        assert_eq!(devid, Some((5 << 8) | 0x18));
        assert_eq!(addr, 0xFEE0_0000);
        assert_eq!(data, 42);
    }

    #[test]
    fn signal_msi_with_rid_accepts_bus_in_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // RID with bus=7, devfn=0x0A → within [5, 10]
        let rid: u16 = (7 << 8) | 0x0A;
        msi_conn.target().signal_msi_with_rid(rid, 0xABCD, 99);

        let (devid, addr, data) = recorder.pop().unwrap();
        assert_eq!(devid, Some(rid as u32));
        assert_eq!(addr, 0xABCD);
        assert_eq!(data, 99);
    }

    #[test]
    fn signal_msi_with_rid_drops_bus_outside_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // bus=11, above subordinate=10 → dropped
        let rid_above: u16 = 11 << 8;
        msi_conn.target().signal_msi_with_rid(rid_above, 0xABCD, 1);
        assert!(recorder.pop().is_none());

        // bus=4, below secondary=5 → dropped
        let rid_below: u16 = 4 << 8;
        msi_conn.target().signal_msi_with_rid(rid_below, 0xABCD, 2);
        assert!(recorder.pop().is_none());
    }

    #[test]
    fn signal_msi_with_rid_accepts_boundary_buses() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // Exactly at secondary bus (5)
        msi_conn.target().signal_msi_with_rid(5 << 8, 0x1000, 10);
        assert!(recorder.pop().is_some());

        // Exactly at subordinate bus (10)
        msi_conn.target().signal_msi_with_rid(10 << 8, 0x2000, 20);
        assert!(recorder.pop().is_some());
    }

    #[test]
    fn route_enable_resolves_default_bdf() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(3, 8);
        let (irqfd, calls) = mock_irqfd(1);
        let msi_conn = MsiConnection::new(bus_range, 0x10); // devfn = dev 2, fn 0
        msi_conn.connect_irqfd(irqfd);

        let route = msi_conn.target().new_route().unwrap().unwrap();
        route.enable(0xFEE0_0000, 55);

        let log = calls[0].lock();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0],
            RouteCall::Enable {
                address: 0xFEE0_0000,
                data: 55,
                devid: Some((3 << 8) | 0x10),
            }
        );
    }

    #[test]
    fn route_enable_with_rid_accepts_bus_in_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let (irqfd, calls) = mock_irqfd(1);
        let msi_conn = MsiConnection::new(bus_range, 0);
        msi_conn.connect_irqfd(irqfd);

        let route = msi_conn.target().new_route().unwrap().unwrap();
        let rid: u16 = (7 << 8) | 0x0A;
        route.enable_with_rid(rid, 0xBEEF, 77);

        let log = calls[0].lock();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0],
            RouteCall::Enable {
                address: 0xBEEF,
                data: 77,
                devid: Some(rid as u32),
            }
        );
    }

    #[test]
    fn route_enable_with_rid_disables_when_bus_outside_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let (irqfd, calls) = mock_irqfd(1);
        let msi_conn = MsiConnection::new(bus_range, 0);
        msi_conn.connect_irqfd(irqfd);

        let route = msi_conn.target().new_route().unwrap().unwrap();
        // bus=11, above subordinate → should disable
        let rid: u16 = 11 << 8;
        route.enable_with_rid(rid, 0xBEEF, 77);

        let log = calls[0].lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], RouteCall::Disable);
    }

    #[test]
    fn with_devfn_derives_target_with_new_devfn() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(2, 5);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let derived = msi_conn.target().with_devfn(0x18); // dev 3, fn 0
        derived.signal_msi(0x1000, 1);

        let (devid, _, _) = recorder.pop().unwrap();
        assert_eq!(devid, Some((2 << 8) | 0x18));
    }

    #[test]
    fn with_bus_range_derives_target_with_new_range() {
        let parent_range = AssignedBusRange::new();
        parent_range.set_bus_range(1, 20);
        let msi_conn = MsiConnection::new(parent_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let child_range = AssignedBusRange::new();
        child_range.set_bus_range(10, 15);
        let derived = msi_conn.target().with_bus_range(child_range, 0x08);
        derived.signal_msi(0x2000, 2);

        let (devid, _, _) = recorder.pop().unwrap();
        // secondary=10, devfn=0x08 → BDF = (10 << 8) | 0x08
        assert_eq!(devid, Some((10 << 8) | 0x08));

        // Validation uses the child range, not the parent
        derived.signal_msi_with_rid(16 << 8, 0x3000, 3);
        assert!(recorder.pop().is_none()); // bus 16 > subordinate 15
    }
}
