// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for implementing simple vmbus devices with no subchannels.
//!
//! This is a simpler abstraction than [`VmbusDevice`], and it is appropriate
//! under these conditions:
//!
//! * The VMBus device has a single channel.
//!
//! * The VMBus device needs to spawn an asynchronous task to handle a channel
//!   when the channel is opened.

use crate::RawAsyncChannel;
use crate::bus::OfferParams;
use crate::bus::OpenRequest;
use crate::bus::ParentBus;
use crate::channel::ChannelHandle;
use crate::channel::ChannelOpenError;
use crate::channel::DeviceResources;
use crate::channel::RestoreControl;
use crate::channel::SaveRestoreVmbusDevice;
use crate::channel::VmbusDevice;
use crate::channel::offer_channel;
use crate::gpadl_ring::GpadlRingMem;
use crate::gpadl_ring::gpadl_channel;
use async_trait::async_trait;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::payload::Protobuf;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use vmbus_ring::RingMem;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::save_restore::SavedStateRoot;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// A trait implemented by a simple vmbus device with no subchannels.
#[async_trait]
pub trait SimpleVmbusDevice<M: RingMem = GpadlRingMem>: 'static + Send {
    /// The saved state type.
    type SavedState: SavedStateRoot + Send;

    /// The type used to run an open channel.
    type Runner: 'static + Send;

    /// The channel offer parameters.
    fn offer(&self) -> OfferParams;

    /// Inspects a channel.
    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>);

    /// Opens the channel, returning the runner to use to run the channel.
    ///
    /// When the channel is closed, the runner will be dropped.
    fn open(
        &mut self,
        channel: RawAsyncChannel<M>,
        guest_memory: GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError>;

    /// Runs an open channel until `stop` is signaled.
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled>;

    /// Closes the channel after the runner has been dropped.
    async fn close(&mut self) {}

    /// Returns a trait used to save/restore the device.
    ///
    /// Returns `None` if the device should be revoked and reoffered on restore.
    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    >;
}

/// Trait implemented by simple vmbus devices that support save/restore.
///
/// If you implement this, make sure to return `Some(self)` from [`SimpleVmbusDevice::supports_save_restore`].
pub trait SaveRestoreSimpleVmbusDevice<M: RingMem = GpadlRingMem>: SimpleVmbusDevice {
    /// Saves the channel.
    ///
    /// Will only be called if the channel is open. If there is state to save on
    /// a closed channel, implement [`VmbusDevice`] instead.
    fn save_open(&mut self, runner: &Self::Runner) -> Self::SavedState;

    /// Restores the channel.
    ///
    /// Will only be called if the channel was saved open. If there is state to
    /// save on a closed channel, implement [`VmbusDevice`] instead.
    fn restore_open(
        &mut self,
        state: Self::SavedState,
        channel: RawAsyncChannel<M>,
    ) -> Result<Self::Runner, ChannelOpenError>;
}

/// The saved state for a simple device.
#[derive(Debug, Protobuf, SavedStateRoot)]
#[mesh(package = "vmbus")]
struct SimpleSavedState {
    /// The open channel saved state.
    ///
    /// If `None`, then the channel was closed during save.
    #[mesh(1)]
    channel: Option<SavedStateBlob>,
}

/// A wrapper around [`SimpleVmbusDevice`] that implements [`VmbusDevice`].
pub struct SimpleDeviceWrapper<T: SimpleVmbusDevice> {
    driver: VmTaskDriver,
    offer: OfferParams,
    resources: DeviceResources,
    device: TaskControl<DeviceTask<T>, T::Runner>,
    running: bool,
}

struct DeviceTask<T>(T);

impl<T: SimpleVmbusDevice> AsyncRun<T::Runner> for DeviceTask<T> {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut T::Runner,
    ) -> Result<(), Cancelled> {
        self.0.run(stop, runner).await
    }
}

impl<T: SimpleVmbusDevice> InspectTaskMut<T::Runner> for DeviceTask<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, runner: Option<&mut T::Runner>) {
        self.0.inspect(req, runner)
    }
}

impl<T: SimpleVmbusDevice> SimpleDeviceWrapper<T> {
    /// Creates a new wrapper, spawning tasks on `driver_source`.
    pub fn new(driver: VmTaskDriver, device: T) -> Self {
        let offer = device.offer();
        Self {
            running: false,
            driver,
            offer,
            resources: Default::default(),
            device: TaskControl::new(DeviceTask(device)),
        }
    }

    /// Gets the inner device back out.
    pub fn into_inner(self) -> T {
        let (task, _) = self.device.into_inner();
        task.0
    }

    fn insert_runner(&mut self, runner: T::Runner) {
        self.device.insert(
            &self.driver,
            format!("{}-{}", self.offer.interface_name, self.offer.instance_id),
            runner,
        );
    }

    fn save(&mut self) -> SimpleSavedState {
        assert!(!self.running);
        let device = if let (state, Some(runner)) = self.device.get_mut() {
            let sr = state.0.supports_save_restore().unwrap();
            Some(SavedStateBlob::new(sr.save_open(runner)))
        } else {
            None
        };
        SimpleSavedState { channel: device }
    }

    fn restore(
        &mut self,
        open_request: Option<&OpenRequest>,
        state: SimpleSavedState,
    ) -> anyhow::Result<()> {
        assert!(!self.running);
        if let Some(device) = state.channel {
            let device = device.parse()?;
            let open_request = open_request.expect("open state mismatch");
            let channel = self.build_channel(open_request)?;
            let sr = self.device.task_mut().0.supports_save_restore().unwrap();
            let task = sr.restore_open(device, channel)?;
            self.insert_runner(task);
        }
        Ok(())
    }

    fn build_channel(
        &mut self,
        open_request: &OpenRequest,
    ) -> anyhow::Result<RawAsyncChannel<GpadlRingMem>> {
        self.driver.retarget_vp(open_request.open_data.target_vp);
        let channel = gpadl_channel(&self.driver, &self.resources, open_request, 0)?;
        Ok(channel)
    }
}

#[async_trait]
impl<T: SimpleVmbusDevice> VmbusDevice for SimpleDeviceWrapper<T> {
    fn offer(&self) -> OfferParams {
        self.offer.clone()
    }

    fn install(&mut self, resources: DeviceResources) {
        self.resources = resources;
    }

    async fn open(
        &mut self,
        _channel_idx: u16,
        open_request: &OpenRequest,
    ) -> Result<(), anyhow::Error> {
        assert!(self.running);
        let channel = self.build_channel(open_request)?;
        let gm = self
            .resources
            .offer_resources
            .guest_memory(open_request)
            .clone();
        let runner = self.device.task_mut().0.open(channel, gm)?;

        self.insert_runner(runner);
        self.device.start();
        Ok(())
    }

    async fn close(&mut self, _channel_idx: u16) {
        self.device.stop().await;
        self.device.remove();
        self.device.task_mut().0.close().await;
    }

    async fn retarget_vp(&mut self, _channel_idx: u16, target_vp: u32) {
        self.driver.retarget_vp(target_vp);
    }

    fn start(&mut self) {
        assert!(!self.running);
        self.device.start();
        self.running = true;
    }

    async fn stop(&mut self) {
        assert!(self.running);
        self.device.stop().await;
        self.running = false;
    }

    fn supports_save_restore(&mut self) -> Option<&mut dyn SaveRestoreVmbusDevice> {
        assert!(!self.running);
        let _ = self.device.task_mut().0.supports_save_restore()?;
        Some(self)
    }
}

#[async_trait]
impl<T: SimpleVmbusDevice> SaveRestoreVmbusDevice for SimpleDeviceWrapper<T> {
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        Ok(SavedStateBlob::new(self.save()))
    }

    async fn restore(
        &mut self,
        mut control: RestoreControl<'_>,
        state: SavedStateBlob,
    ) -> Result<(), RestoreError> {
        let state: SimpleSavedState = state.parse()?;
        let is_open = state.channel.is_some();
        let open_request = control.restore(&[is_open]).await?;
        self.restore(open_request[0].as_ref(), state)
            .map_err(RestoreError::Other)?;
        Ok(())
    }
}

impl<T: SimpleVmbusDevice> InspectMut for SimpleDeviceWrapper<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .field("driver", &self.driver)
            .merge(&mut self.device);
    }
}

/// Offers a new channel, returning a typed handle to get back the original
/// channel when it's revoked.
pub async fn offer_simple_device<T: 'static + SimpleVmbusDevice>(
    driver_source: &VmTaskDriverSource,
    bus: &(impl ParentBus + ?Sized),
    device: T,
) -> anyhow::Result<SimpleDeviceHandle<T>> {
    let driver = driver_source.builder().target_vp(0).build("simple-vmbus");
    let channel = SimpleDeviceWrapper::new(driver, device);
    Ok(SimpleDeviceHandle(
        offer_channel(&driver_source.simple(), bus, channel).await?,
    ))
}

/// A handle to an offered simple vmbus device.
#[must_use]
#[derive(Debug, Inspect)]
#[inspect(transparent)]
pub struct SimpleDeviceHandle<T: SimpleVmbusDevice>(ChannelHandle<SimpleDeviceWrapper<T>>);

impl<T: SimpleVmbusDevice> SimpleDeviceHandle<T> {
    /// Revokes the device, returning it if the VMBus server is still running.
    pub async fn revoke(self) -> Option<T> {
        self.0.revoke().await.map(|x| x.into_inner())
    }

    /// Starts the device.
    pub fn start(&self) {
        self.0.start()
    }

    /// Stops the device.
    pub async fn stop(&self) {
        self.0.stop().await
    }

    /// Resets a stopped device.
    pub async fn reset(&self) {
        self.0.reset().await
    }

    /// Saves a stopped device.
    pub async fn save(&self) -> anyhow::Result<Option<SavedStateBlob>> {
        self.0.save().await
    }

    /// Restores a stopped device.
    pub async fn restore(&self, state: SavedStateBlob) -> anyhow::Result<()> {
        self.0.restore(state).await
    }
}
