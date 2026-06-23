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
