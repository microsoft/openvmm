// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synic interface definitions used by VmBus.

use crate::interrupt::Interrupt;
use crate::monitor::MonitorId;
use hvdef::Vtl;
use inspect::Inspect;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

pub trait MessagePort: Send + Sync {
    /// Handles a message received on the message port.
    ///
    /// A message is `trusted` if it was was received from the guest without using host-visible
    /// mechanisms on a hardware-isolated VM. The `trusted` parameter is always `false` if not
    /// running in the paravisor of a hardware-isolated VM.
    fn poll_handle_message(&self, cx: &mut Context<'_>, msg: &[u8], trusted: bool) -> Poll<()>;
}

pub trait EventPort: Send + Sync {
    fn handle_event(&self, flag: u16);
    fn os_event(&self) -> Option<&pal_event::Event> {
        None
    }
}

#[derive(Debug, Error)]
#[error("hypervisor error")]
pub struct HypervisorError(#[from] pub Box<dyn std::error::Error + Send + Sync>);

#[derive(Debug, Error)]
pub enum Error {
    #[error("connection ID in use: {0}")]
    ConnectionIdInUse(u32),
    #[error(transparent)]
    Hypervisor(HypervisorError),
}

/// Trait for accessing partition's synic ports.
pub trait SynicPortAccess: Send + Sync {
    /// Adds a host message port, which gets notified when the guest calls
    /// `HvPostMessage`.
    fn add_message_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn MessagePort>,
    ) -> Result<Box<dyn Sync + Send>, Error>;

    /// Adds a host event port, which gets notified when the guest calls
    /// `HvSignalEvent`.
    fn add_event_port(
        &self,
        connection_id: u32,
        minimum_vtl: Vtl,
        port: Arc<dyn EventPort>,
    ) -> Result<Box<dyn Sync + Send>, Error>;

    /// Creates a [`GuestMessagePort`] for posting messages to the guest.
    fn new_guest_message_port(
        &self,
        vtl: Vtl,
        vp: u32,
        sint: u8,
    ) -> Result<Box<dyn GuestMessagePort>, HypervisorError>;

    /// Creates a [`GuestEventPort`] for signaling VMBus channels in the guest.
    fn new_guest_event_port(&self) -> Result<Box<dyn GuestEventPort>, HypervisorError>;

    /// Returns whether callers should pass an OS event when creating event
    /// ports, as opposed to passing a function to call.
    ///
    /// This is true when the hypervisor can more quickly dispatch an OS event
    /// and resume the VP than it can take an intercept into user mode and call
    /// a function.
    fn prefer_os_events(&self) -> bool;

    /// Returns an object for manipulating the monitor page, or None if monitor pages aren't
    /// supported.
    fn monitor_support(&self) -> Option<&dyn SynicMonitorAccess> {
        None
    }
}

/// Provides monitor page functionality for a `SynicPortAccess` implementation.
pub trait SynicMonitorAccess: SynicPortAccess {
    /// Registers a monitored interrupt. The returned struct will unregister the ID when dropped.
    ///
    /// # Panics
    ///
    /// Panics if monitor_id is already in use.
    fn register_monitor(&self, monitor_id: MonitorId, connection_id: u32) -> Box<dyn Send>;

    /// Sets the GPA of the monitor page currently in use.
    fn set_monitor_page(&self, gpa: Option<u64>) -> anyhow::Result<()>;
}

/// A guest event port, created by [`SynicPortAccess::new_guest_event_port`].
pub trait GuestEventPort: Send + Sync {
    /// Returns an interrupt object used to signal the guest.
    fn interrupt(&self) -> Interrupt;

    /// Clears the event port state so that the interrupt does nothing.
    fn clear(&mut self);

    /// Updates the parameters for the event port.
    fn set(&mut self, vtl: Vtl, vp: u32, sint: u8, flag: u16) -> Result<(), HypervisorError>;
}

/// A guest message port, created by [`SynicPortAccess::new_guest_message_port`].
pub trait GuestMessagePort: Send + Sync + Inspect {
    /// Posts a message to the guest.
    ///
    /// It is the caller's responsibility to not queue too many messages. There
    /// is no backpressure mechanism at the transport layer.
    ///
    /// FUTURE: add backpressure.
    fn post_message(&mut self, typ: u32, payload: &[u8]);

    /// Changes the virtual processor to which messages are sent.
    fn set_target_vp(&mut self, vp: u32) -> Result<(), HypervisorError>;
}
