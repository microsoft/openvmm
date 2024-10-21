// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VPCI bus control for Underhill.
//!
//! TODO: move to a different crate.

// TODO: should this be in this crate? nothing in this crate actually ends up
// using this code...

use anyhow::Result;
use async_trait::async_trait;
use mesh::payload::Protobuf;
use mesh::Receiver;

/// Events signaled on a Virtual PCI bus.
#[derive(Debug, Protobuf)]
pub enum VpciBusEvent {
    /// Device has been enumerated by the bus.
    DeviceEnumerated,
    /// Device is about to be detached from the bus.
    PrepareForRemoval,
}

/// A trait used to control a Virtual PCI bus.
#[async_trait]
pub trait VpciBusControl {
    /// Offers the bus and its attached device to the target partition.
    /// The bus and device must have been pre-configured (through some other mechanism,
    /// not provided by this trait) before they are offered.
    async fn offer_device(&self) -> Result<()>;

    /// Revokes the bus and its attached device from the target partition.
    async fn revoke_device(&self) -> Result<()>;

    /// Returns a reference to an object used to receive bus events.
    fn notifier(&mut self) -> &mut Receiver<VpciBusEvent>;
}
