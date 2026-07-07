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
    default_rid: DefaultRid,
}

impl MsiRoute {
    /// Returns the event that triggers interrupt injection when signaled.
    ///
    /// Pass this to VFIO `map_msix` or any other interrupt source.
    pub fn event(&self) -> &Event {
        self.inner.event()
    }

    /// Configures the MSI address and data for this route, using the route's
    /// default requester ID `(secondary_bus << 8) + rid_offset`.
    ///
    /// If the resolved bus falls outside the assigned bus range, the route is
    /// left disabled and a ratelimited warning is emitted.
    pub fn enable(&self, address: u64, data: u32) {
        // `resolve_default_rid` emits the ratelimited warning when the
        // resolved bus is out of range; just leave the route disabled here.
        let Some(resolved) = resolve_default_rid(&self.default_rid) else {
            self.inner.disable();
            return;
        };
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
        if !self.default_rid.bus_range.contains_bus(bus) {
            let (secondary, subordinate) = self.default_rid.bus_range.bus_range();
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

/// Default requester-ID source for MSI device identification.
///
/// [`MsiTarget::signal_msi`] composes the requester ID at signal time as
/// `(secondary_bus << 8) + rid_offset`, reading the secondary bus from the
/// live [`AssignedBusRange`]. For a single-function device `rid_offset` is
/// just its devfn; for SR-IOV VFs it may carry into the bus byte to address
/// functions on higher buses within the assigned range.
#[derive(Clone, Debug)]
struct DefaultRid {
    bus_range: AssignedBusRange,
    rid_offset: u16,
}

/// Resolves a requester ID from a [`DefaultRid`] source, composing it as
/// `(secondary_bus << 8) + rid_offset` against the live bus range.
///
/// Returns `None` when the resulting bus falls outside the assigned range
/// (the offset reaches past the subordinate bus), in which case a ratelimited
/// warning is emitted and the caller should drop the MSI / disable the route.
/// The offset is non-negative, so the bus is always at least the secondary
/// bus; only the upper bound can be exceeded.
fn resolve_default_rid(default: &DefaultRid) -> Option<u32> {
    let (secondary, subordinate) = default.bus_range.bus_range();
    let rid = ((secondary as u32) << 8) + default.rid_offset as u32;
    if rid >> 8 > subordinate as u32 {
        tracelimit::warn_ratelimited!(
            rid,
            secondary,
            subordinate,
            "dropping MSI: rid bus outside assigned bus range"
        );
        return None;
    }
    Some(rid)
}

/// A late-bound MSI backend slot.
///
/// A connection carries no device identity — it is purely the backend that
/// MSIs are delivered to, filled in after construction via [`connect`].
/// Identity is supplied when a target is derived via
/// [`msi_target`](Self::msi_target), or when a
/// [`DmaTarget`](crate::dma::DmaTarget) is built from it.
///
/// [`connect`]: Self::connect
#[derive(Debug)]
pub struct MsiConnection {
    inner: Arc<RwLock<MsiTargetInner>>,
}

/// An MSI target that can be used to signal MSI interrupts.
#[derive(Clone)]
pub struct MsiTarget {
    inner: Arc<RwLock<MsiTargetInner>>,
    default_rid: DefaultRid,
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
            default_rid: DefaultRid {
                bus_range: AssignedBusRange::new(),
                rid_offset: 0,
            },
        }
    }
}

impl std::fmt::Debug for MsiTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiTarget")
            .field("default_rid", &self.default_rid)
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
    /// Creates a new disconnected MSI connection.
    ///
    /// The connection is purely the late-bound MSI backend slot; it carries
    /// no device identity. Callers stamp identity when they derive a target
    /// via [`msi_target`](Self::msi_target), or by building a
    /// [`DmaTarget`](crate::dma::DmaTarget) from it.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MsiTargetInner {
                signal_msi: Arc::new(DisconnectedMsiTarget),
                irqfd: None,
            })),
        }
    }

    /// Updates the MSI target to which this connection signals interrupts.
    pub fn connect(&self, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.inner.write();
        inner.signal_msi = signal_msi;
    }

    /// Sets the [`IrqFd`] for kernel-mediated MSI route allocation.
    ///
    /// When present, [`MsiTarget::new_route`] can create [`MsiRoute`]
    /// instances for direct interrupt delivery.
    pub fn connect_irqfd(&self, irqfd: Arc<dyn IrqFd>) {
        let mut inner = self.inner.write();
        inner.irqfd = Some(irqfd);
    }

    /// Derives an MSI target with the given identity, sharing this
    /// connection's (late-bound) backend slot.
    pub fn msi_target(&self, bus_range: AssignedBusRange, devfn: u8) -> MsiTarget {
        MsiTarget {
            inner: self.inner.clone(),
            default_rid: DefaultRid {
                bus_range,
                rid_offset: devfn as u16,
            },
        }
    }

    /// Derives an MSI target with no device identity (an empty bus range).
    ///
    /// Use for MSI emitters that don't need a meaningful requester ID, or
    /// that re-anchor identity themselves (e.g. PCIe switches).
    pub fn target(&self) -> MsiTarget {
        self.msi_target(AssignedBusRange::new(), 0)
    }
}

impl Default for MsiConnection {
    fn default() -> Self {
        Self::new()
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
        self.with_rid_offset(devfn as u16)
    }

    /// Returns a new `MsiTarget` sharing the same connection but with
    /// a different bus range and devfn.
    ///
    /// Use this when a component (e.g. a PCIe switch) needs to derive
    /// targets using a bus range it owns rather than the parent's.
    pub fn with_bus_range(&self, bus_range: AssignedBusRange, devfn: u8) -> MsiTarget {
        MsiTarget {
            inner: self.inner.clone(),
            default_rid: DefaultRid {
                bus_range,
                rid_offset: devfn as u16,
            },
        }
    }

    /// Returns a new `MsiTarget` sharing the same connection and bus range
    /// but with the requester-ID offset set so the target resolves to the
    /// given absolute `rid`.
    ///
    /// The offset is computed against the *current* secondary bus, so call
    /// this only once the bus range is assigned. For targets derived before
    /// the bus is programmed (e.g. SR-IOV VFs), use
    /// [`with_rid_offset`](Self::with_rid_offset) instead.
    ///
    /// The resulting bus is validated against the assigned bus range when an
    /// MSI is signaled (see [`signal_msi`](Self::signal_msi)), not here.
    pub fn with_rid(&self, rid: u16) -> MsiTarget {
        let (secondary, _) = self.default_rid.bus_range.bus_range();
        self.with_rid_offset(rid.wrapping_sub((secondary as u16) << 8))
    }

    /// Returns a new `MsiTarget` sharing the same connection and bus range
    /// but with the requester-ID offset set to `rid_offset`.
    ///
    /// The RID is resolved at signal time as `(secondary_bus << 8) +
    /// rid_offset`, so the target tracks the live bus assignment. This is the
    /// primitive for SR-IOV VFs, which are constructed before the PF's bus is
    /// programmed: pass the VF's RID offset (e.g. VF Offset + index × VF
    /// Stride) and it resolves correctly once the bus range is assigned.
    pub fn with_rid_offset(&self, rid_offset: u16) -> MsiTarget {
        MsiTarget {
            inner: self.inner.clone(),
            default_rid: DefaultRid {
                bus_range: self.default_rid.bus_range.clone(),
                rid_offset,
            },
        }
    }

    /// Signals an MSI interrupt to this target, using this target's
    /// default BDF as the requester ID.
    pub fn signal_msi(&self, address: u64, data: u32) {
        let Some(resolved) = resolve_default_rid(&self.default_rid) else {
            return;
        };
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
        if !self.default_rid.bus_range.contains_bus(bus) {
            let (secondary, subordinate) = self.default_rid.bus_range.bus_range();
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
                default_rid: self.default_rid.clone(),
            })
        })
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
    fn signal_msi_resolves_default_rid() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        msi_conn
            .msi_target(bus_range, 0x18)
            .signal_msi(0xFEE0_0000, 42);

        let (devid, addr, data) = recorder.pop().unwrap();
        assert_eq!(devid, Some((5 << 8) | 0x18));
        assert_eq!(addr, 0xFEE0_0000);
        assert_eq!(data, 42);
    }

    #[test]
    fn signal_msi_with_rid_accepts_bus_in_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // RID with bus=7, devfn=0x0A → within [5, 10]
        let rid: u16 = (7 << 8) | 0x0A;
        msi_conn
            .msi_target(bus_range, 0)
            .signal_msi_with_rid(rid, 0xABCD, 99);

        let (devid, addr, data) = recorder.pop().unwrap();
        assert_eq!(devid, Some(rid as u32));
        assert_eq!(addr, 0xABCD);
        assert_eq!(data, 99);
    }

    #[test]
    fn signal_msi_with_rid_drops_bus_outside_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // bus=11, above subordinate=10 → dropped
        let rid_above: u16 = 11 << 8;
        msi_conn
            .msi_target(bus_range.clone(), 0)
            .signal_msi_with_rid(rid_above, 0xABCD, 1);
        assert!(recorder.pop().is_none());

        // bus=4, below secondary=5 → dropped
        let rid_below: u16 = 4 << 8;
        msi_conn
            .msi_target(bus_range, 0)
            .signal_msi_with_rid(rid_below, 0xABCD, 2);
        assert!(recorder.pop().is_none());
    }

    #[test]
    fn signal_msi_with_rid_accepts_boundary_buses() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // Exactly at secondary bus (5)
        msi_conn
            .msi_target(bus_range.clone(), 0)
            .signal_msi_with_rid(5 << 8, 0x1000, 10);
        assert!(recorder.pop().is_some());

        // Exactly at subordinate bus (10)
        msi_conn
            .msi_target(bus_range, 0)
            .signal_msi_with_rid(10 << 8, 0x2000, 20);
        assert!(recorder.pop().is_some());
    }

    #[test]
    fn route_enable_resolves_default_rid() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(3, 8);
        let (irqfd, calls) = mock_irqfd(1);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);

        let route = msi_conn
            .msi_target(bus_range, 0x10)
            .new_route()
            .unwrap()
            .unwrap();
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
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);

        let route = msi_conn
            .msi_target(bus_range, 0)
            .new_route()
            .unwrap()
            .unwrap();
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
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);

        let route = msi_conn
            .msi_target(bus_range, 0)
            .new_route()
            .unwrap()
            .unwrap();
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
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let derived = msi_conn.msi_target(bus_range, 0).with_devfn(0x18); // dev 3, fn 0
        derived.signal_msi(0x1000, 1);

        let (devid, _, _) = recorder.pop().unwrap();
        assert_eq!(devid, Some((2 << 8) | 0x18));
    }

    #[test]
    fn with_bus_range_derives_target_with_new_range() {
        let parent_range = AssignedBusRange::new();
        parent_range.set_bus_range(1, 20);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let child_range = AssignedBusRange::new();
        child_range.set_bus_range(10, 15);
        let derived = msi_conn
            .msi_target(parent_range, 0)
            .with_bus_range(child_range, 0x08);
        derived.signal_msi(0x2000, 2);

        let (devid, _, _) = recorder.pop().unwrap();
        // secondary=10, devfn=0x08 → BDF = (10 << 8) | 0x08
        assert_eq!(devid, Some((10 << 8) | 0x08));

        // Validation uses the child range, not the parent
        derived.signal_msi_with_rid(16 << 8, 0x3000, 3);
        assert!(recorder.pop().is_none()); // bus 16 > subordinate 15
    }

    #[test]
    fn with_rid_signal_msi_accepts_bus_in_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // RID with bus=7 (within [5, 10]), devfn=0x0A
        let rid: u16 = (7 << 8) | 0x0A;
        let derived = msi_conn.msi_target(bus_range, 0).with_rid(rid);
        derived.signal_msi(0x1000, 7);

        let (devid, addr, data) = recorder.pop().unwrap();
        assert_eq!(devid, Some(rid as u32));
        assert_eq!(addr, 0x1000);
        assert_eq!(data, 7);
    }

    #[test]
    fn with_rid_signal_msi_drops_bus_outside_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        // bus=11 > subordinate=10 → dropped
        let derived_above = msi_conn.msi_target(bus_range.clone(), 0).with_rid(11 << 8);
        derived_above.signal_msi(0x1000, 1);
        assert!(recorder.pop().is_none());

        // bus=4 < secondary=5 → dropped
        let derived_below = msi_conn.msi_target(bus_range, 0).with_rid(4 << 8);
        derived_below.signal_msi(0x2000, 2);
        assert!(recorder.pop().is_none());
    }

    #[test]
    fn with_rid_route_enable_disables_when_bus_outside_range() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let (irqfd, calls) = mock_irqfd(1);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);

        // Derive a target whose override bus (11) is outside [5, 10], then
        // enable a route from it: the route must be disabled, not enabled.
        let derived = msi_conn.msi_target(bus_range, 0).with_rid(11 << 8);
        let route = derived.new_route().unwrap().unwrap();
        route.enable(0xBEEF, 77);

        let log = calls[0].lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], RouteCall::Disable);
    }
}
