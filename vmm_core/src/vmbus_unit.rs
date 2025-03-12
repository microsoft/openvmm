// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! State unit definitions for vmbus components.

#![warn(missing_docs)]

use inspect::Inspect;
use pal_async::task::Spawn;
use state_unit::NameInUse;
use state_unit::SpawnedUnit;
use state_unit::StateUnit;
use state_unit::StateUnits;
use state_unit::UnitBuilder;
use state_unit::UnitHandle;
use state_unit::run_async_unit;
use std::sync::Arc;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::channel::ChannelHandle;
use vmbus_channel::channel::VmbusDevice;
use vmbus_channel::channel::offer_channel;
use vmbus_channel::channel::offer_generic_channel;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::simple::SimpleDeviceHandle;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_channel::simple::offer_simple_device;
use vmbus_server::VmbusServer;
use vmbus_server::VmbusServerControl;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::VmTaskDriverSource;

/// A handle to a vmbus server that is registered as a state unit.
///
/// FUTURE: incorporate the state unit handling directly into `VmbusServer`.
pub struct VmbusServerHandle {
    unit: SpawnedUnit<VmbusServerUnit>,
    control: Arc<VmbusServerControl>,
}

impl VmbusServerHandle {
    /// Makes a new handle, registering the server via `builder`.
    pub fn new(
        spawner: &impl Spawn,
        builder: UnitBuilder<'_>,
        server: VmbusServer,
    ) -> Result<Self, NameInUse> {
        let control = server.control();
        let unit = builder.spawn(spawner, |recv| {
            run_async_unit(VmbusServerUnit(server), recv)
        })?;
        Ok(Self { unit, control })
    }

    /// Gets the vmbus control interface.
    pub fn control(&self) -> &Arc<VmbusServerControl> {
        &self.control
    }

    /// Gets the vmbus unit handle.
    pub fn unit_handle(&self) -> &UnitHandle {
        self.unit.handle()
    }

    /// Removes the server.
    pub async fn remove(self) -> VmbusServer {
        self.unit.remove().await.0
    }
}

/// A newtype over `VmbusServer` implementing [`StateUnit`].
#[derive(Inspect)]
#[inspect(transparent)]
struct VmbusServerUnit(VmbusServer);

impl StateUnit for &'_ VmbusServerUnit {
    async fn start(&mut self) {
        self.0.start();
    }

    async fn stop(&mut self) {
        self.0.stop().await;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset().await;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(Some(SavedStateBlob::new(self.0.save().await)))
    }

    async fn restore(&mut self, buffer: SavedStateBlob) -> Result<(), RestoreError> {
        self.0
            .restore(buffer.parse()?)
            .await
            .map_err(|err| RestoreError::Other(err.into()))
    }

    async fn post_restore(&mut self) -> anyhow::Result<()> {
        self.0.post_restore().await?;
        Ok(())
    }
}

/// A type wrapping a [`ChannelHandle`] and implementing [`StateUnit`].
#[must_use]
#[derive(Debug, Inspect)]
#[inspect(transparent)]
pub struct ChannelUnit<T: ?Sized>(ChannelHandle<T>);

/// Offers a channel, creates a unit for it, and adds it to `state_units`.
pub async fn offer_channel_unit<T: 'static + VmbusDevice>(
    driver: &impl Spawn,
    state_units: &StateUnits,
    vmbus: &VmbusServerHandle,
    channel: T,
) -> anyhow::Result<SpawnedUnit<ChannelUnit<T>>> {
    let offer = channel.offer();
    let name = format!("{}:{}", offer.interface_name, offer.instance_id);
    let handle = offer_channel(driver, vmbus.control.as_ref(), channel).await?;
    let unit = state_units
        .add(name)
        .depends_on(vmbus.unit.handle())
        .spawn(driver, |recv| run_async_unit(ChannelUnit(handle), recv))?;
    Ok(unit)
}

impl<T: 'static + VmbusDevice> ChannelUnit<T> {
    /// Revokes a channel.
    pub async fn revoke(self) -> T {
        self.0.revoke().await.unwrap()
    }
}

impl<T: 'static + VmbusDevice + ?Sized> StateUnit for &'_ ChannelUnit<T> {
    async fn start(&mut self) {
        self.0.start();
    }

    async fn stop(&mut self) {
        self.0.stop().await;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset().await;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        let state = self.0.save().await.map_err(SaveError::Other)?;
        Ok(state)
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.0.restore(state).await.map_err(RestoreError::Other)
    }
}

/// A type wrapping a [`ChannelHandle`] and implementing [`StateUnit`].
#[must_use]
#[derive(Debug)]
pub struct SimpleChannelUnit<T: SimpleVmbusDevice>(SimpleDeviceHandle<T>);

/// Offers a simple vmbus device, creates a unit for it, and adds it to `state_units`.
pub async fn offer_simple_device_unit<T: SimpleVmbusDevice>(
    driver_source: &VmTaskDriverSource,
    state_units: &StateUnits,
    vmbus: &VmbusServerHandle,
    device: T,
) -> anyhow::Result<SpawnedUnit<SimpleChannelUnit<T>>> {
    let offer = device.offer();
    let name = format!("{}:{}", offer.interface_name, offer.instance_id);
    let handle = offer_simple_device(driver_source, vmbus.control.as_ref(), device).await?;
    let unit = state_units
        .add(name)
        .depends_on(vmbus.unit.handle())
        .spawn(driver_source.simple(), |recv| {
            run_async_unit(SimpleChannelUnit(handle), recv)
        })?;
    Ok(unit)
}

impl<T: SimpleVmbusDevice> SimpleChannelUnit<T> {
    /// Revokes the channel and returns it.
    pub async fn revoke(self) -> T {
        self.0.revoke().await.unwrap()
    }
}

impl<T: SimpleVmbusDevice> Inspect for SimpleChannelUnit<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.0.inspect(req);
    }
}

impl<T: SimpleVmbusDevice> StateUnit for &'_ SimpleChannelUnit<T> {
    async fn start(&mut self) {
        self.0.start();
    }

    async fn stop(&mut self) {
        self.0.stop().await;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset().await;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        let state = self.0.save().await.map_err(SaveError::Other)?;
        Ok(state)
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.0.restore(state).await.map_err(RestoreError::Other)
    }
}

/// Offers a channel, creates a unit for it, and adds it to `state_units`.
pub async fn offer_vmbus_device_handle_unit(
    driver_source: &VmTaskDriverSource,
    state_units: &StateUnits,
    vmbus: &VmbusServerHandle,
    resolver: &ResourceResolver,
    resource: Resource<VmbusDeviceHandleKind>,
) -> anyhow::Result<SpawnedUnit<ChannelUnit<dyn VmbusDevice>>> {
    let channel = resolver
        .resolve(resource, ResolveVmbusDeviceHandleParams { driver_source })
        .await?;
    let offer = channel.0.offer();
    let name = format!("{}:{}", offer.interface_name, offer.instance_id);
    let handle =
        offer_generic_channel(&driver_source.simple(), vmbus.control.as_ref(), channel.0).await?;
    let unit = state_units
        .add(name)
        .depends_on(vmbus.unit.handle())
        .spawn(driver_source.simple(), |recv| {
            run_async_unit(ChannelUnit(handle), recv)
        })?;
    Ok(unit)
}
