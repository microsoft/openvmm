// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use crate::bus_range::AssignedBusRange;
pub use chipset_device::msi::SignalMsi;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::sync::Arc;
use vmcore::irqfd::KernelMsiBinding;

/// A kernel-mediated MSI interrupt route for a single vector.
///
/// Each route owns an [`Event`] (fd). Signaling the event delivers the MSI —
/// either directly in the hypervisor (when the guest-programmed address decodes
/// to a kernel-serviceable sink) or in usermode (when it decodes to an emulated
/// controller). The route owns the fd rather than the backend minting it, so a
/// single fd — the one handed to VFIO — can be rebound across backends as the
/// guest reprograms the MSI address. Exactly one consumer reads the fd at a
/// time: the kernel binding, or the usermode fallback task.
///
/// The route does not hold a fixed backend: on each [`enable`](Self::enable) it
/// binds through the connection's router ([`SignalMsi::bind_msi`]), which
/// decodes the guest-programmed address to a sink afresh. A reprogram that
/// keeps the same sink updates the binding in place
/// ([`KernelMsiBinding::update`]) with no fd churn; a reprogram that changes
/// the sink drops the old binding (unbinding the fd) before binding the new
/// one.
pub struct MsiRoute {
    default_rid: DefaultRid,
    /// The shared backend slot — read live for the router (`signal_msi` /
    /// `bind_msi`) and the driver that runs the usermode fallback task.
    connection: Arc<RwLock<MsiTargetInner>>,
    /// The fd both VFIO and the usermode fallback consume — one at a time.
    event: Event,
    /// The current kernel binding, when the guest-programmed address decodes to
    /// a kernel-serviceable sink. `None` while delivering in usermode (or
    /// disabled). Dropping it unbinds the fd from the kernel.
    binding: Mutex<Option<Box<dyn KernelMsiBinding>>>,
    /// The usermode fallback task. Present only while the address decodes to a
    /// sink the kernel cannot service; dropping it cancels the poll and
    /// releases the fd.
    usermode: Mutex<Option<Task<()>>>,
}

impl MsiRoute {
    /// Returns the event that triggers interrupt injection when signaled.
    ///
    /// Pass this to VFIO `map_msix` or any other interrupt source.
    pub fn event(&self) -> &Event {
        &self.event
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
            self.disable();
            return;
        };
        self.set(address, data, resolved);
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
            self.disable();
            return;
        }
        self.set(address, data, rid.into());
    }

    /// Programs the route for the given address/data with the resolved
    /// requester ID: bind the kernel route when the address decodes to a
    /// kernel sink, otherwise fall back to usermode delivery.
    fn set(&self, address: u64, data: u32, rid: u32) {
        // Fast path: if the address still resolves to the sink we're already
        // bound to, re-point the routing entry in place without touching the fd
        // assignment (the common case: the guest reprogrammed only the vector).
        if let Some(binding) = &*self.binding.lock() {
            if binding.update(Some(rid), address, data) {
                return;
            }
        }

        // Slow path: the backend changed (or there was no binding). Tear down
        // the old consumer of the fd before establishing the new one — an
        // eventfd has exactly one consumer, so the kernel binding must be
        // dropped (unbinding the fd) and the usermode poll stopped before we
        // bind or poll again. A device signal that lands in the gap is not
        // lost: the fd stays readable and the new consumer picks it up.
        self.stop_usermode();
        *self.binding.lock() = None;

        let router = self.connection.read().signal_msi.clone();
        match router.bind_msi(&self.event, Some(rid), address, data) {
            Some(binding) => *self.binding.lock() = Some(binding),
            None => {
                // The address does not decode to a kernel-serviceable sink (an
                // emulated controller, or — pending the per-segment guard — a
                // foreign sink). Deliver in usermode by polling our fd and
                // dispatching through the connection's router.
                self.start_usermode(Some(rid), address, data);
            }
        }
    }

    /// Disables the MSI route. Interrupts that arrive while disabled
    /// remain pending on the event and will be delivered when
    /// [`enable`](Self::enable) is called, or can be drained via
    /// [`consume_pending`](Self::consume_pending).
    pub fn disable(&self) {
        self.stop_usermode();
        // Dropping the binding unbinds the fd from the kernel.
        *self.binding.lock() = None;
    }

    /// Drains pending interrupt state and returns whether an interrupt
    /// was pending while the route was masked.
    pub fn consume_pending(&self) -> bool {
        self.event.try_wait()
    }

    /// Stops the usermode fallback task, if running, releasing the fd.
    fn stop_usermode(&self) {
        // Dropping the task cancels the poll loop.
        *self.usermode.lock() = None;
    }

    /// Starts the usermode fallback: a task that waits on our fd and, on each
    /// signal, dispatches through the connection's router (the address decodes
    /// to the emulated sink there). No-ops with a warning if no driver was
    /// supplied (e.g. in tests), matching the prior drop-on-miss behavior.
    fn start_usermode(&self, devid: Option<u32>, address: u64, data: u32) {
        let driver = self.connection.read().driver.clone();
        let Some(driver) = driver else {
            tracelimit::warn_ratelimited!(
                address,
                data,
                "MSI route not kernel-serviceable and no driver for usermode fallback; dropping"
            );
            return;
        };
        let wait = match PolledWait::new(&*driver, self.event.clone()) {
            Ok(wait) => wait,
            Err(e) => {
                tracelimit::error_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    "failed to create usermode MSI wait"
                );
                return;
            }
        };
        let connection = self.connection.clone();
        let task = driver.spawn("msi-usermode-route", async move {
            let mut wait = wait;
            loop {
                wait.wait().await.expect("wait should not fail");
                // Read the router live so a reconnect is picked up.
                let signal = connection.read().signal_msi.clone();
                signal.signal_msi(devid, address, data);
            }
        });
        *self.usermode.lock() = Some(task);
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
                driver: None,
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
    /// Driver used to run [`MsiRoute`]s. Its presence also gates whether direct
    /// (fd-based / passthrough) routes are supported: it is set by the wiring
    /// only on platforms that support kernel-mediated routes. A route binds its
    /// fd through the connection's router ([`SignalMsi::bind_msi`]); when the
    /// guest-programmed address does not decode to a kernel sink, the route
    /// polls the fd on this driver and delivers via `signal_msi`.
    driver: Option<Arc<dyn SpawnDriver>>,
}

impl std::fmt::Debug for MsiTargetInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            signal_msi: _,
            driver,
        } = self;
        f.debug_struct("MsiTargetInner")
            .field("has_driver", &driver.is_some())
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
                driver: None,
            })),
        }
    }

    /// Updates the MSI target to which this connection signals interrupts.
    pub fn connect(&self, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.inner.write();
        inner.signal_msi = signal_msi;
    }

    /// Enables kernel-mediated (fd-based / passthrough) MSI routes on this
    /// connection, supplying the `driver` used to run them.
    ///
    /// The wiring calls this only on platforms that support kernel-mediated
    /// routes. After it is called, [`MsiTarget::new_route`] can create
    /// [`MsiRoute`] instances: each binds its fd through the connection's
    /// router ([`SignalMsi::bind_msi`]), and falls back to a usermode poll on
    /// this driver when the guest-programmed address does not decode to a
    /// kernel sink.
    pub fn enable_msi_routes(&self, driver: Arc<dyn SpawnDriver>) {
        let mut inner = self.inner.write();
        inner.driver = Some(driver);
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
    /// [`signal_msi`](Self::signal_msi) does. It owns a fresh fd and binds it,
    /// on each [`enable`](MsiRoute::enable), through the connection's router
    /// ([`SignalMsi::bind_msi`]) — so the fd (the one handed to VFIO) follows
    /// the guest across sinks as it reprograms the MSI address.
    ///
    /// Returns `None` if kernel-mediated routes have not been enabled on the
    /// connection (see [`MsiConnection::enable_msi_routes`]).
    pub fn new_route(&self) -> Option<anyhow::Result<MsiRoute>> {
        let inner = self.inner.read();
        // Routes are supported iff the wiring enabled them (which also supplies
        // the driver for the usermode fallback leg).
        inner.driver.as_ref()?;
        Some(Ok(MsiRoute {
            default_rid: self.default_rid.clone(),
            connection: self.inner.clone(),
            event: Event::new(),
            binding: Mutex::new(None),
            usermode: Mutex::new(None),
        }))
    }

    /// Returns whether this target supports direct MSI routes.
    pub fn supports_direct_msi(&self) -> bool {
        let inner = self.inner.read();
        inner.driver.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus_range::AssignedBusRange;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
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

    /// A recording MSI router whose `bind_msi` logs each bind and hands back a
    /// binding that always requests a rebind (so every route `enable` re-binds
    /// and is captured). Models a kernel-serviceable sink for route tests.
    struct RecordingRouter {
        binds: Arc<Mutex<Vec<(Option<u32>, u64, u32)>>>,
    }

    impl SignalMsi for RecordingRouter {
        fn signal_msi(&self, _devid: Option<u32>, _address: u64, _data: u32) {}

        fn bind_msi(
            &self,
            _fd: &Event,
            devid: Option<u32>,
            address: u64,
            data: u32,
        ) -> Option<Box<dyn KernelMsiBinding>> {
            self.binds.lock().push((devid, address, data));
            Some(Box::new(RecordingBinding))
        }
    }

    struct RecordingBinding;

    impl KernelMsiBinding for RecordingBinding {
        fn update(&self, _devid: Option<u32>, _address: u64, _data: u32) -> bool {
            // Force a full rebind on every reprogram so each `enable` is logged.
            false
        }
    }

    /// Builds a connection with routes enabled (via `driver`) and a recording
    /// router, returning the connection and the shared bind log.
    fn route_conn(
        driver: Arc<dyn SpawnDriver>,
    ) -> (MsiConnection, Arc<Mutex<Vec<(Option<u32>, u64, u32)>>>) {
        let binds = Arc::new(Mutex::new(Vec::new()));
        let msi_conn = MsiConnection::new();
        msi_conn.connect(Arc::new(RecordingRouter {
            binds: binds.clone(),
        }));
        msi_conn.enable_msi_routes(driver);
        (msi_conn, binds)
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

    #[async_test]
    async fn route_enable_resolves_default_rid(driver: DefaultDriver) {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(3, 8);
        let (msi_conn, binds) = route_conn(Arc::new(driver));

        let route = msi_conn
            .msi_target(bus_range, 0x10)
            .new_route()
            .unwrap()
            .unwrap();
        route.enable(0xFEE0_0000, 55);

        let log = binds.lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], (Some((3 << 8) | 0x10), 0xFEE0_0000, 55));
    }

    #[async_test]
    async fn route_enable_with_rid_accepts_bus_in_range(driver: DefaultDriver) {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let (msi_conn, binds) = route_conn(Arc::new(driver));

        let route = msi_conn
            .msi_target(bus_range, 0)
            .new_route()
            .unwrap()
            .unwrap();
        let rid: u16 = (7 << 8) | 0x0A;
        route.enable_with_rid(rid, 0xBEEF, 77);

        let log = binds.lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], (Some(rid as u32), 0xBEEF, 77));
    }

    #[async_test]
    async fn route_enable_with_rid_disables_when_bus_outside_range(driver: DefaultDriver) {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let (msi_conn, binds) = route_conn(Arc::new(driver));

        let route = msi_conn
            .msi_target(bus_range, 0)
            .new_route()
            .unwrap()
            .unwrap();
        // bus=11, above subordinate → route left disabled, no bind
        let rid: u16 = 11 << 8;
        route.enable_with_rid(rid, 0xBEEF, 77);

        assert!(binds.lock().is_empty());
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

    #[async_test]
    async fn with_rid_route_enable_disables_when_bus_outside_range(driver: DefaultDriver) {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let (msi_conn, binds) = route_conn(Arc::new(driver));

        // Derive a target whose override bus (11) is outside [5, 10], then
        // enable a route from it: the route must be left disabled, not bound.
        let derived = msi_conn.msi_target(bus_range, 0).with_rid(11 << 8);
        let route = derived.new_route().unwrap().unwrap();
        route.enable(0xBEEF, 77);

        assert!(binds.lock().is_empty());
    }
}
