// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`SynicPortAccess`] implementation for backends that intercept
//! `HvPostMessage` / `HvSignalEvent` hypercalls in user mode.
//!
//! Backends where the hypervisor handles synic in-kernel should implement
//! [`SynicPortAccess`] directly.

use hvdef::HvError;
use hvdef::HvResult;
use hvdef::Vtl;
use inspect::Inspect;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::hash_map;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;
use vm_topology::processor::VpIndex;
use vmcore::monitor::MonitorId;
use vmcore::synic::EventPort;
use vmcore::synic::GuestEventPort;
use vmcore::synic::GuestMessagePort;
use vmcore::synic::MessagePort;
use vmcore::synic::MonitorInfo;
use vmcore::synic::MonitorPageGpas;
use vmcore::synic::SynicMonitorAccess;
use vmcore::synic::SynicPortAccess;

/// Registry of message and event ports, keyed by connection ID.
///
/// This is the shared state that backs [`SynicPorts`]. Store an instance
/// on the partition inner struct so that both `SynicPortAccess` consumers
/// (VMBus, etc.) and hypercall handlers share the same port map.
///
/// Hypercall handlers should call [`SynicPortMap::handle_post_message`] /
/// [`SynicPortMap::handle_signal_event`] to dispatch guest hypercalls.
#[derive(Inspect, Debug, Default)]
pub struct SynicPortMap {
    #[inspect(with = "|x| inspect::adhoc(|req| inspect::iter_by_key(&*x.lock()).inspect(req))")]
    ports: Mutex<HashMap<u32, Port>>,
}

impl SynicPortMap {
    /// Dispatches a guest `HvPostMessage` hypercall to the registered port.
    pub fn handle_post_message(
        &self,
        vtl: Vtl,
        connection_id: u32,
        secure: bool,
        message: &[u8],
    ) -> HvResult<()> {
        let port = self.ports.lock().get(&connection_id).cloned();
        if let Some(Port {
            port_type: PortType::Message(port),
            minimum_vtl,
        }) = port
        {
            if vtl < minimum_vtl {
                Err(HvError::OperationDenied)
            } else if port.poll_handle_message(
                &mut Context::from_waker(std::task::Waker::noop()),
                message,
                secure,
            ) == Poll::Ready(())
            {
                Ok(())
            } else {
                // TODO: VMBus sometimes (in Azure?) returns HV_STATUS_TIMEOUT
                //       here instead to force the guest to retry. Should we do
                //       the same? Perhaps only for Linux VMs?
                Err(HvError::InsufficientBuffers)
            }
        } else {
            Err(HvError::InvalidConnectionId)
        }
    }

    /// Dispatches a guest `HvSignalEvent` hypercall to the registered port.
    pub fn handle_signal_event(
        &self,
        vtl: Vtl,
        connection_id: u32,
        flag_number: u16,
    ) -> HvResult<()> {
        let port = self.ports.lock().get(&connection_id).cloned();
        if let Some(Port {
            port_type: PortType::Event(port),
            minimum_vtl,
        }) = port
        {
            if vtl < minimum_vtl {
                Err(HvError::OperationDenied)
            } else {
                port.handle_event(flag_number);
                Ok(())
            }
        } else {
            Err(HvError::InvalidConnectionId)
        }
    }

    fn add_message_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn MessagePort>,
    ) -> Result<(), vmcore::synic::Error> {
        match self.ports.lock().entry(connection_id) {
            hash_map::Entry::Occupied(_) => {
                Err(vmcore::synic::Error::ConnectionIdInUse(connection_id))
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(Port {
                    port_type: PortType::Message(port),
                    minimum_vtl,
                });
                Ok(())
            }
        }
    }

    fn add_event_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn EventPort>,
    ) -> Result<(), vmcore::synic::Error> {
        match self.ports.lock().entry(connection_id) {
            hash_map::Entry::Occupied(_) => {
                Err(vmcore::synic::Error::ConnectionIdInUse(connection_id))
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(Port {
                    port_type: PortType::Event(port),
                    minimum_vtl,
                });
                Ok(())
            }
        }
    }
}

pub trait Synic: 'static + Send + Sync {
    /// Returns the port map for this partition, which can be used to register
    /// ports.
    fn port_map(&self) -> &SynicPortMap;

    /// Adds a fast path to signal `event` when the guest signals
    /// `connection_id` from VTL >= `minimum_vtl`.
    ///
    /// Returns Ok(None) if this acceleration is not supported.
    fn new_host_event_port(
        self: Arc<Self>,
        connection_id: u32,
        minimum_vtl: Vtl,
        event: &pal_event::Event,
    ) -> Result<Option<Box<dyn Sync + Send>>, vmcore::synic::Error> {
        let _ = (connection_id, minimum_vtl, event);
        Ok(None)
    }

    /// Posts a message to the guest.
    fn post_message(&self, vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]);

    /// Creates a [`GuestEventPort`] for signaling VMBus channels in the guest.
    fn new_guest_event_port(
        self: Arc<Self>,
        vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort>;

    /// Returns whether callers should pass an OS event when creating event
    /// ports, as opposed to passing a function to call.
    ///
    /// This is true when the hypervisor can more quickly dispatch an OS event
    /// and resume the VP than it can take an intercept into user mode and call
    /// a function.
    fn prefer_os_events(&self) -> bool;

    /// Returns an object for manipulating the monitor page, or None if monitor pages aren't
    /// supported.
    fn monitor_support(&self) -> Option<&dyn SynicMonitor> {
        None
    }
}

/// Provides monitor page functionality for a `Synic` implementation.
pub trait SynicMonitor: Synic {
    /// Registers a monitored interrupt. The returned struct will unregister the ID when dropped.
    ///
    /// # Panics
    ///
    /// Panics if monitor_id is already in use.
    fn register_monitor(&self, monitor_id: MonitorId, connection_id: u32) -> Box<dyn Sync + Send>;

    /// Sets the GPA of the monitor page currently in use.
    fn set_monitor_page(&self, vtl: Vtl, gpa: Option<u64>) -> anyhow::Result<()>;

    /// Allocates a monitor page and sets it as the monitor page currently in use. If allocating
    /// monitor pages is not supported, returns `Ok(None)`.
    ///
    /// The page will be deallocated if the monitor page is subsequently changed or cleared using
    /// [`SynicMonitor::set_monitor_page`].
    fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<u64>> {
        let _ = vtl;
        Ok(None)
    }
}

/// Adapts a [`Synic`] implementation to [`SynicPortAccess`].
///
/// Wraps a shared [`SynicPortMap`] (stored on the partition inner struct)
/// with the [`Synic`] trait methods needed for port registration.
#[derive(Debug)]
pub struct SynicPorts<T> {
    synic: Arc<T>,
}

impl<T: Synic> SynicPorts<T> {
    /// Creates a new `SynicPorts` backed by the given partition.
    pub fn new(synic: Arc<T>) -> Self {
        Self { synic }
    }
}

impl<T: Synic> SynicPortAccess for SynicPorts<T> {
    fn add_message_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn MessagePort>,
    ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
        self.synic
            .port_map()
            .add_message_port(connection_id, minimum_vtl, port)?;
        Ok(Box::new(PortHandle {
            synic: Arc::downgrade(&self.synic),
            connection_id,
            _inner_handle: None,
            _monitor: None,
        }))
    }

    fn add_event_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn EventPort>,
        monitor_info: Option<MonitorInfo>,
    ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
        // Create a direct port mapping in the hypervisor if an event was provided.
        let inner_handle = if let Some(event) = port.os_event() {
            self.synic
                .clone()
                .new_host_event_port(connection_id, minimum_vtl, event)?
        } else {
            None
        };

        self.synic
            .port_map()
            .add_event_port(connection_id, minimum_vtl, port)?;

        let monitor = monitor_info.as_ref().and_then(|info| {
            self.synic
                .monitor_support()
                .map(|monitor| monitor.register_monitor(info.monitor_id, connection_id))
        });

        Ok(Box::new(PortHandle {
            synic: Arc::downgrade(&self.synic),
            connection_id,
            _inner_handle: inner_handle,
            _monitor: monitor,
        }))
    }

    fn new_guest_message_port(
        &self,
        vtl: Vtl,
        vp: u32,
        sint: u8,
    ) -> Result<Box<dyn GuestMessagePort>, vmcore::synic::HypervisorError> {
        Ok(Box::new(DirectGuestMessagePort {
            partition: Arc::clone(&self.synic),
            vtl,
            vp: VpIndex::new(vp),
            sint,
        }))
    }

    fn new_guest_event_port(
        &self,
        _port_id: u32,
        vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
        _monitor_info: Option<MonitorInfo>,
    ) -> Result<Box<dyn GuestEventPort>, vmcore::synic::HypervisorError> {
        Ok(self.synic.clone().new_guest_event_port(vtl, vp, sint, flag))
    }

    fn prefer_os_events(&self) -> bool {
        self.synic.prefer_os_events()
    }

    fn monitor_support(&self) -> Option<&dyn SynicMonitorAccess> {
        self.synic.monitor_support().and(Some(self))
    }
}

impl<T: Synic> SynicMonitorAccess for SynicPorts<T> {
    fn set_monitor_page(&self, vtl: Vtl, gpa: Option<MonitorPageGpas>) -> anyhow::Result<()> {
        self.synic
            .monitor_support()
            .unwrap()
            .set_monitor_page(vtl, gpa.map(|mp| mp.child_to_parent))
    }

    fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<MonitorPageGpas>> {
        self.synic
            .monitor_support()
            .unwrap()
            .allocate_monitor_page(vtl)
            .map(|gpa| {
                gpa.map(|child_to_parent| MonitorPageGpas {
                    parent_to_child: 0,
                    child_to_parent,
                })
            })
    }
}

struct PortHandle<T: Synic> {
    synic: Weak<T>,
    connection_id: u32,
    _inner_handle: Option<Box<dyn Sync + Send>>,
    _monitor: Option<Box<dyn Sync + Send>>,
}

impl<T: Synic> Drop for PortHandle<T> {
    fn drop(&mut self) {
        if let Some(synic) = self.synic.upgrade() {
            let entry = synic.port_map().ports.lock().remove(&self.connection_id);
            entry.expect("port was previously added");
        }
    }
}

#[derive(Debug, Clone, Inspect)]
struct Port {
    port_type: PortType,
    #[inspect(with = "|&x| x as u8")]
    minimum_vtl: Vtl,
}

#[derive(Clone, Inspect)]
#[inspect(external_tag)]
enum PortType {
    Message(#[inspect(skip)] Arc<dyn MessagePort>),
    Event(#[inspect(skip)] Arc<dyn EventPort>),
}

impl Debug for PortType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Self::Message(_) => "Port::Message",
            Self::Event(_) => "Port::Event",
        })
    }
}

#[derive(Inspect)]
#[inspect(bound = "")]
struct DirectGuestMessagePort<T> {
    #[inspect(skip)]
    partition: Arc<T>,
    #[inspect(with = "|&x| x as u8")]
    vtl: Vtl,
    vp: VpIndex,
    sint: u8,
}

impl<T: Synic> GuestMessagePort for DirectGuestMessagePort<T> {
    fn poll_post_message(&mut self, _cx: &mut Context<'_>, typ: u32, payload: &[u8]) -> Poll<()> {
        self.partition
            .post_message(self.vtl, self.vp, self.sint, typ, payload);
        Poll::Ready(())
    }

    fn set_target_vp(&mut self, vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        self.vp = VpIndex::new(vp);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    /// Records messages, completing them only while `ready` is set.
    struct TestMessagePort {
        ready: bool,
        messages: Mutex<Vec<(Vec<u8>, bool)>>,
    }

    impl TestMessagePort {
        fn new(ready: bool) -> Arc<Self> {
            Arc::new(Self {
                ready,
                messages: Mutex::new(Vec::new()),
            })
        }
    }

    impl MessagePort for TestMessagePort {
        fn poll_handle_message(
            &self,
            _cx: &mut Context<'_>,
            msg: &[u8],
            trusted: bool,
        ) -> Poll<()> {
            if !self.ready {
                return Poll::Pending;
            }
            self.messages.lock().push((msg.to_vec(), trusted));
            Poll::Ready(())
        }
    }

    #[derive(Default)]
    struct TestEventPort {
        flags: Mutex<Vec<u16>>,
        /// If set, the port asks for a hypervisor fast path.
        os_event: Option<pal_event::Event>,
    }

    impl TestEventPort {
        fn with_os_event() -> Arc<Self> {
            Arc::new(Self {
                flags: Mutex::new(Vec::new()),
                os_event: Some(pal_event::Event::new()),
            })
        }
    }

    impl EventPort for TestEventPort {
        fn handle_event(&self, flag: u16) {
            self.flags.lock().push(flag);
        }

        fn os_event(&self) -> Option<&pal_event::Event> {
            self.os_event.as_ref()
        }
    }

    /// Records the hypervisor fast paths that are currently installed.
    #[derive(Default)]
    struct TestSynic {
        port_map: SynicPortMap,
        next_fast_path: AtomicU64,
        fast_paths: Mutex<Vec<(u64, u32)>>,
    }

    /// Removes its fast path from the synic when dropped.
    struct FastPathHandle {
        synic: Weak<TestSynic>,
        id: u64,
    }

    impl Drop for FastPathHandle {
        fn drop(&mut self) {
            if let Some(synic) = self.synic.upgrade() {
                synic.fast_paths.lock().retain(|&(id, _)| id != self.id);
            }
        }
    }

    impl Synic for TestSynic {
        fn port_map(&self) -> &SynicPortMap {
            &self.port_map
        }

        fn new_host_event_port(
            self: Arc<Self>,
            connection_id: u32,
            _minimum_vtl: Vtl,
            _event: &pal_event::Event,
        ) -> Result<Option<Box<dyn Sync + Send>>, vmcore::synic::Error> {
            let id = self.next_fast_path.fetch_add(1, Ordering::Relaxed);
            self.fast_paths.lock().push((id, connection_id));
            Ok(Some(Box::new(FastPathHandle {
                synic: Arc::downgrade(&self),
                id,
            })))
        }

        fn post_message(&self, _vtl: Vtl, _vp: VpIndex, _sint: u8, _typ: u32, _payload: &[u8]) {
            unreachable!()
        }

        fn new_guest_event_port(
            self: Arc<Self>,
            _vtl: Vtl,
            _vp: u32,
            _sint: u8,
            _flag: u16,
        ) -> Box<dyn GuestEventPort> {
            unreachable!()
        }

        fn prefer_os_events(&self) -> bool {
            false
        }
    }

    // Test: Post a guest message and signal a guest event to registered ports
    //       of the matching type.
    // Expected: The message, with its trusted flag, reaches the message port,
    //       and the event flag reaches the event port.
    #[test]
    fn hypercalls_dispatch_to_registered_ports() {
        let synic = Arc::new(TestSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let map = synic.port_map();
        let message_port = TestMessagePort::new(true);
        let event_port = Arc::new(TestEventPort::default());
        let _message_handle = ports
            .add_message_port(10, Vtl::Vtl0, message_port.clone())
            .unwrap();
        let _event_handle = ports
            .add_event_port(20, Vtl::Vtl0, event_port.clone(), None)
            .unwrap();

        map.handle_post_message(Vtl::Vtl0, 10, true, b"hello")
            .unwrap();
        map.handle_signal_event(Vtl::Vtl0, 20, 7).unwrap();

        assert_eq!(*message_port.messages.lock(), [(b"hello".to_vec(), true)]);
        assert_eq!(*event_port.flags.lock(), [7]);
    }

    // Test: Post to an unregistered connection ID, and use each registered
    //       connection ID with the hypercall for the other port type.
    // Expected: Each is rejected with InvalidConnectionId and no port is
    //       invoked.
    #[test]
    fn hypercalls_reject_unknown_or_mismatched_connection_ids() {
        let synic = Arc::new(TestSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let map = synic.port_map();
        let message_port = TestMessagePort::new(true);
        let event_port = Arc::new(TestEventPort::default());
        let _message_handle = ports
            .add_message_port(10, Vtl::Vtl0, message_port.clone())
            .unwrap();
        let _event_handle = ports
            .add_event_port(20, Vtl::Vtl0, event_port.clone(), None)
            .unwrap();

        assert_eq!(
            map.handle_post_message(Vtl::Vtl0, 99, false, b"x"),
            Err(HvError::InvalidConnectionId)
        );
        assert_eq!(
            map.handle_signal_event(Vtl::Vtl0, 99, 0),
            Err(HvError::InvalidConnectionId)
        );
        assert_eq!(
            map.handle_post_message(Vtl::Vtl0, 20, false, b"x"),
            Err(HvError::InvalidConnectionId)
        );
        assert_eq!(
            map.handle_signal_event(Vtl::Vtl0, 10, 0),
            Err(HvError::InvalidConnectionId)
        );

        assert!(message_port.messages.lock().is_empty());
        assert!(event_port.flags.lock().is_empty());
    }

    // Test: Post messages and signal events from VTLs below and at each
    //       port's minimum VTL.
    // Expected: Callers below the minimum VTL get OperationDenied and the
    //       port is not invoked; callers at or above it are dispatched.
    #[test]
    fn hypercalls_enforce_minimum_vtl() {
        let synic = Arc::new(TestSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let map = synic.port_map();
        let message_port = TestMessagePort::new(true);
        let event_port = Arc::new(TestEventPort::default());
        let _message_handle = ports
            .add_message_port(10, Vtl::Vtl2, message_port.clone())
            .unwrap();
        let _event_handle = ports
            .add_event_port(20, Vtl::Vtl1, event_port.clone(), None)
            .unwrap();

        for vtl in [Vtl::Vtl0, Vtl::Vtl1] {
            assert_eq!(
                map.handle_post_message(vtl, 10, false, b"low"),
                Err(HvError::OperationDenied),
                "{vtl:?}"
            );
        }
        assert_eq!(
            map.handle_signal_event(Vtl::Vtl0, 20, 1),
            Err(HvError::OperationDenied)
        );
        assert!(message_port.messages.lock().is_empty());
        assert!(event_port.flags.lock().is_empty());

        map.handle_post_message(Vtl::Vtl2, 10, false, b"high")
            .unwrap();
        map.handle_signal_event(Vtl::Vtl1, 20, 2).unwrap();
        map.handle_signal_event(Vtl::Vtl2, 20, 3).unwrap();
        assert_eq!(*message_port.messages.lock(), [(b"high".to_vec(), false)]);
        assert_eq!(*event_port.flags.lock(), [2, 3]);
    }

    // Test: Post a guest message to a message port that cannot accept it yet.
    // Expected: The hypercall fails with InsufficientBuffers so the guest
    //       retries, rather than reporting success for a dropped message.
    #[test]
    fn busy_message_port_reports_insufficient_buffers() {
        let synic = Arc::new(TestSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let map = synic.port_map();
        let message_port = TestMessagePort::new(false);
        let _message_handle = ports
            .add_message_port(10, Vtl::Vtl0, message_port.clone())
            .unwrap();

        assert_eq!(
            map.handle_post_message(Vtl::Vtl0, 10, false, b"x"),
            Err(HvError::InsufficientBuffers)
        );
        assert!(message_port.messages.lock().is_empty());
    }

    // Test: Register a connection ID twice (as either port type), then drop
    //       the first registration's handle and register the ID again.
    // Expected: The duplicate is rejected with ConnectionIdInUse; dropping the
    //       handle unregisters the port so guest hypercalls to it fail and the
    //       ID can be reused.
    #[test]
    fn port_handle_owns_connection_id() {
        let synic = Arc::new(TestSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let message_port = TestMessagePort::new(true);

        let handle = ports
            .add_message_port(10, Vtl::Vtl0, message_port.clone())
            .unwrap();
        assert!(matches!(
            ports.add_message_port(10, Vtl::Vtl0, TestMessagePort::new(true)),
            Err(vmcore::synic::Error::ConnectionIdInUse(10))
        ));
        assert!(matches!(
            ports.add_event_port(10, Vtl::Vtl0, Arc::new(TestEventPort::default()), None),
            Err(vmcore::synic::Error::ConnectionIdInUse(10))
        ));
        synic
            .port_map()
            .handle_post_message(Vtl::Vtl0, 10, false, b"before")
            .unwrap();

        drop(handle);
        assert_eq!(
            synic
                .port_map()
                .handle_post_message(Vtl::Vtl0, 10, false, b"after"),
            Err(HvError::InvalidConnectionId)
        );
        assert_eq!(*message_port.messages.lock(), [(b"before".to_vec(), false)]);

        let event_port = Arc::new(TestEventPort::default());
        let _handle = ports
            .add_event_port(10, Vtl::Vtl0, event_port.clone(), None)
            .unwrap();
        synic
            .port_map()
            .handle_signal_event(Vtl::Vtl0, 10, 4)
            .unwrap();
        assert_eq!(*event_port.flags.lock(), [4]);
    }

    // Test: Drop a port handle after the synic that owns the port map has
    //       already been torn down.
    // Expected: The drop does nothing and does not panic, and the port itself
    //       was released along with the map.
    #[test]
    fn port_handle_drop_after_synic_teardown_is_a_no_op() {
        let synic = Arc::new(TestSynic::default());
        let weak_synic = Arc::downgrade(&synic);
        let ports = SynicPorts::new(synic);
        let message_port = TestMessagePort::new(true);
        let handle = ports
            .add_message_port(10, Vtl::Vtl0, message_port.clone())
            .unwrap();

        drop(ports);
        assert!(weak_synic.upgrade().is_none(), "synic still alive");
        assert_eq!(Arc::strong_count(&message_port), 1);

        drop(handle);
    }

    // Test: Register an event port that requests a hypervisor fast path, then
    //       try to register a second such port on the same connection ID, and
    //       finally drop the first port's handle.
    // Expected: The duplicate is rejected with ConnectionIdInUse and its fast
    //       path is torn down, leaving only the original's fast path and
    //       routing guest events to the original port. Dropping the original
    //       handle removes its fast path too.
    #[test]
    fn duplicate_event_port_does_not_leave_a_fast_path() {
        let synic = Arc::new(TestSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let original = TestEventPort::with_os_event();

        let handle = ports
            .add_event_port(10, Vtl::Vtl0, original.clone(), None)
            .unwrap();
        let original_fast_paths = synic.fast_paths.lock().clone();
        assert_eq!(original_fast_paths.len(), 1);
        assert_eq!(original_fast_paths[0].1, 10);

        let duplicate = TestEventPort::with_os_event();
        assert!(matches!(
            ports.add_event_port(10, Vtl::Vtl0, duplicate.clone(), None),
            Err(vmcore::synic::Error::ConnectionIdInUse(10))
        ));
        assert_eq!(
            *synic.fast_paths.lock(),
            original_fast_paths,
            "duplicate registration changed the installed fast paths"
        );

        synic
            .port_map()
            .handle_signal_event(Vtl::Vtl0, 10, 5)
            .unwrap();
        assert_eq!(*original.flags.lock(), [5]);
        assert!(duplicate.flags.lock().is_empty());

        drop(handle);
        assert!(synic.fast_paths.lock().is_empty());
    }

    type Post = (Vtl, VpIndex, u8, u32, Vec<u8>);

    /// A synic that records guest messages, guest event ports, and monitor
    /// page activity, and that supports monitor pages.
    #[derive(Default)]
    struct RecordingSynic {
        port_map: SynicPortMap,
        posts: Mutex<Vec<Post>>,
        event_ports: Mutex<Vec<(Vtl, u32, u8, u16)>>,
        monitors: Arc<Mutex<Vec<(MonitorId, u32)>>>,
        monitor_pages: Mutex<Vec<(Vtl, Option<u64>)>>,
    }

    struct NullGuestEventPort;

    impl GuestEventPort for NullGuestEventPort {
        fn interrupt(&self) -> vmcore::interrupt::Interrupt {
            vmcore::interrupt::Interrupt::null()
        }

        fn set_target_vp(&mut self, _vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
            Ok(())
        }
    }

    /// Unregisters its monitor when dropped.
    struct MonitorHandle {
        monitors: Arc<Mutex<Vec<(MonitorId, u32)>>>,
        connection_id: u32,
    }

    impl Drop for MonitorHandle {
        fn drop(&mut self) {
            self.monitors
                .lock()
                .retain(|&(_, id)| id != self.connection_id);
        }
    }

    impl Synic for RecordingSynic {
        fn port_map(&self) -> &SynicPortMap {
            &self.port_map
        }

        fn post_message(&self, vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
            self.posts
                .lock()
                .push((vtl, vp, sint, typ, payload.to_vec()));
        }

        fn new_guest_event_port(
            self: Arc<Self>,
            vtl: Vtl,
            vp: u32,
            sint: u8,
            flag: u16,
        ) -> Box<dyn GuestEventPort> {
            self.event_ports.lock().push((vtl, vp, sint, flag));
            Box::new(NullGuestEventPort)
        }

        fn prefer_os_events(&self) -> bool {
            true
        }

        fn monitor_support(&self) -> Option<&dyn SynicMonitor> {
            Some(self)
        }
    }

    impl SynicMonitor for RecordingSynic {
        fn register_monitor(
            &self,
            monitor_id: MonitorId,
            connection_id: u32,
        ) -> Box<dyn Sync + Send> {
            self.monitors.lock().push((monitor_id, connection_id));
            Box::new(MonitorHandle {
                monitors: self.monitors.clone(),
                connection_id,
            })
        }

        fn set_monitor_page(&self, vtl: Vtl, gpa: Option<u64>) -> anyhow::Result<()> {
            self.monitor_pages.lock().push((vtl, gpa));
            Ok(())
        }

        fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<u64>> {
            self.monitor_pages.lock().push((vtl, Some(0x5000)));
            Ok(Some(0x5000))
        }
    }

    // Test: Create a guest message port for VTL1, VP 2, SINT 3, post a
    //       message, retarget the port to VP 5, and post again.
    // Expected: Each message reaches the synic for the port's VTL and SINT
    //       with its type and payload, first on VP 2 and then on VP 5.
    #[test]
    fn guest_message_port_posts_to_its_target_vp() {
        let synic = Arc::new(RecordingSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let mut port = ports.new_guest_message_port(Vtl::Vtl1, 2, 3).unwrap();
        let mut cx = Context::from_waker(std::task::Waker::noop());

        assert!(port.poll_post_message(&mut cx, 0x10, b"one").is_ready());
        port.set_target_vp(5).unwrap();
        assert!(port.poll_post_message(&mut cx, 0x11, b"two").is_ready());

        assert_eq!(
            *synic.posts.lock(),
            [
                (Vtl::Vtl1, VpIndex::new(2), 3, 0x10, b"one".to_vec()),
                (Vtl::Vtl1, VpIndex::new(5), 3, 0x11, b"two".to_vec()),
            ]
        );
    }

    // Test: Create a guest event port and ask whether OS events are preferred
    //       on a synic that prefers them.
    // Expected: The event port is created by the synic for the requested VTL,
    //       VP, SINT, and flag, and the preference is passed through.
    #[test]
    fn guest_event_port_and_os_event_preference_pass_through() {
        let synic = Arc::new(RecordingSynic::default());
        let ports = SynicPorts::new(synic.clone());

        let _port = ports
            .new_guest_event_port(99, Vtl::Vtl2, 4, 6, 12, None)
            .unwrap();

        assert_eq!(*synic.event_ports.lock(), [(Vtl::Vtl2, 4, 6, 12)]);
        assert!(ports.prefer_os_events());
    }

    // Test: On a synic with monitor pages, add an event port with monitor ID
    //       9, set the monitor page, allocate one, and drop the port; then add
    //       a monitored event port on a synic without monitor pages.
    // Expected: The monitor is registered for the port's connection ID and
    //       unregistered when the port is dropped; setting the page passes the
    //       outgoing (child-to-parent) GPA; an allocated page is reported as
    //       the outgoing GPA. Without monitor support, no monitor access is
    //       offered and the port is still added.
    #[test]
    fn monitor_pages_and_monitored_ports() {
        let synic = Arc::new(RecordingSynic::default());
        let ports = SynicPorts::new(synic.clone());
        let monitor_info = MonitorInfo {
            monitor_id: MonitorId(9),
            latency: std::time::Duration::from_millis(1),
        };

        let handle = ports
            .add_event_port(
                30,
                Vtl::Vtl0,
                Arc::new(TestEventPort::default()),
                Some(monitor_info),
            )
            .unwrap();
        assert_eq!(*synic.monitors.lock(), [(MonitorId(9), 30)]);

        let monitor = ports.monitor_support().unwrap();
        monitor
            .set_monitor_page(
                Vtl::Vtl0,
                Some(MonitorPageGpas {
                    parent_to_child: 0x3000,
                    child_to_parent: 0x4000,
                }),
            )
            .unwrap();
        monitor.set_monitor_page(Vtl::Vtl0, None).unwrap();
        let allocated = monitor.allocate_monitor_page(Vtl::Vtl1).unwrap().unwrap();
        assert_eq!(
            (allocated.parent_to_child, allocated.child_to_parent),
            (0, 0x5000)
        );
        assert_eq!(
            *synic.monitor_pages.lock(),
            [
                (Vtl::Vtl0, Some(0x4000)),
                (Vtl::Vtl0, None),
                (Vtl::Vtl1, Some(0x5000))
            ]
        );

        drop(handle);
        assert!(synic.monitors.lock().is_empty());

        let plain = Arc::new(TestSynic::default());
        let plain_ports = SynicPorts::new(plain.clone());
        assert!(plain_ports.monitor_support().is_none());
        let _handle = plain_ports
            .add_event_port(
                31,
                Vtl::Vtl0,
                Arc::new(TestEventPort::default()),
                Some(monitor_info),
            )
            .unwrap();
        plain
            .port_map()
            .handle_signal_event(Vtl::Vtl0, 31, 0)
            .unwrap();
    }
}
