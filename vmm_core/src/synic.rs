// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
use virt::Synic;
use virt::VpIndex;
use vmcore::synic::EventPort;
use vmcore::synic::GuestEventPort;
use vmcore::synic::GuestMessagePort;
use vmcore::synic::MessagePort;
use vmcore::synic::MonitorInfo;
use vmcore::synic::MonitorPageGpas;
use vmcore::synic::SynicMonitorAccess;
use vmcore::synic::SynicPortAccess;

pub struct SynicPorts {
    partition: Arc<dyn Synic>,
    ports: Arc<PortMap>,
}

type PortMap = Mutex<HashMap<u32, Port>>;

impl SynicPorts {
    pub fn new(partition: Arc<dyn Synic>) -> Self {
        Self {
            partition,
            ports: Default::default(),
        }
    }

    pub fn on_post_message(
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

    pub fn on_signal_event(&self, vtl: Vtl, connection_id: u32, flag_number: u16) -> HvResult<()> {
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
}

impl SynicPortAccess for SynicPorts {
    fn add_message_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn MessagePort>,
    ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
        match self.ports.lock().entry(connection_id) {
            hash_map::Entry::Occupied(_) => {
                return Err(vmcore::synic::Error::ConnectionIdInUse(connection_id));
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(Port {
                    port_type: PortType::Message(port),
                    minimum_vtl,
                });
            }
        }
        Ok(Box::new(PortHandle {
            ports: Arc::downgrade(&self.ports),
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
            self.partition
                .new_host_event_port(connection_id, minimum_vtl, event)?
        } else {
            None
        };

        match self.ports.lock().entry(connection_id) {
            hash_map::Entry::Occupied(_) => {
                return Err(vmcore::synic::Error::ConnectionIdInUse(connection_id));
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(Port {
                    port_type: PortType::Event(port),
                    minimum_vtl,
                });
            }
        }

        let monitor = monitor_info.as_ref().and_then(|info| {
            self.partition
                .monitor_support()
                .map(|monitor| monitor.register_monitor(info.monitor_id, connection_id))
        });

        Ok(Box::new(PortHandle {
            ports: Arc::downgrade(&self.ports),
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
            partition: Arc::clone(&self.partition),
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
        Ok(self.partition.new_guest_event_port(vtl, vp, sint, flag))
    }

    fn prefer_os_events(&self) -> bool {
        self.partition.prefer_os_events()
    }

    fn monitor_support(&self) -> Option<&dyn SynicMonitorAccess> {
        self.partition.monitor_support().and(Some(self))
    }
}

impl SynicMonitorAccess for SynicPorts {
    fn set_monitor_page(&self, vtl: Vtl, gpa: Option<MonitorPageGpas>) -> anyhow::Result<()> {
        self.partition
            .monitor_support()
            .unwrap()
            .set_monitor_page(vtl, gpa.map(|mp| mp.child_to_parent))
    }

    fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<MonitorPageGpas>> {
        self.partition
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

struct PortHandle {
    ports: Weak<PortMap>,
    connection_id: u32,
    _inner_handle: Option<Box<dyn Sync + Send>>,
    _monitor: Option<Box<dyn Sync + Send>>,
}

impl Drop for PortHandle {
    fn drop(&mut self) {
        if let Some(ports) = self.ports.upgrade() {
            let entry = ports.lock().remove(&self.connection_id);
            entry.expect("port was previously added");
        }
    }
}

#[derive(Debug, Clone)]
struct Port {
    port_type: PortType,
    minimum_vtl: Vtl,
}

#[derive(Clone)]
enum PortType {
    Message(Arc<dyn MessagePort>),
    Event(Arc<dyn EventPort>),
}

impl Debug for PortType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Self::Message(_) => "Port::Message",
            Self::Event(_) => "Port::Event",
        })
    }
}

struct DirectGuestMessagePort {
    partition: Arc<dyn Synic>,
    vtl: Vtl,
    vp: VpIndex,
    sint: u8,
}

impl GuestMessagePort for DirectGuestMessagePort {
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

impl Inspect for DirectGuestMessagePort {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond().field("message_port_vp", self.vp.index());
    }
}
