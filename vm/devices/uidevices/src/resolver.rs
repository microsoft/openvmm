// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for UI devices.

use crate::keyboard::Keyboard;
use crate::mouse::Mouse;
use crate::video::Video;
use async_trait::async_trait;
use thiserror::Error;
use uidevices_resources::SynthKeyboardHandle;
use uidevices_resources::SynthMouseHandle;
use uidevices_resources::SynthVideoHandle;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;

/// A resolver for [`SynthVideoHandle`], [`SynthKeyboardHandle`], and
/// [`SynthMouseHandle`].
pub struct VmbusUiResolver;

declare_static_async_resolver! {
    VmbusUiResolver,
    (VmbusDeviceHandleKind, SynthVideoHandle),
    (VmbusDeviceHandleKind, SynthKeyboardHandle),
    (VmbusDeviceHandleKind, SynthMouseHandle),
}

/// Error returned when resolving video device handles.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum VideoError {
    #[error("failed to create video device")]
    Video(#[source] anyhow::Error),
    #[error("failed to resolve framebuffer")]
    Framebuffer(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, SynthVideoHandle> for VmbusUiResolver {
    type Output = ResolvedVmbusDevice;
    type Error = VideoError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SynthVideoHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let framebuffer = resolver
            .resolve(resource.framebuffer, ())
            .await
            .map_err(VideoError::Framebuffer)?;
        let device = SimpleDeviceWrapper::new(
            input.driver_source.simple(),
            Video::new(framebuffer.0).map_err(VideoError::Video)?,
        );
        Ok(device.into())
    }
}

/// Error returned when resolving input device handles.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum InputError {
    #[error("failed to resolve input source")]
    InputSource(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, SynthKeyboardHandle> for VmbusUiResolver {
    type Output = ResolvedVmbusDevice;
    type Error = InputError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SynthKeyboardHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let source = resolver
            .resolve(resource.source, "synthkbd")
            .await
            .map_err(InputError::InputSource)?;
        let device =
            SimpleDeviceWrapper::new(input.driver_source.simple(), Keyboard::new(source.0));
        Ok(device.into())
    }
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, SynthMouseHandle> for VmbusUiResolver {
    type Output = ResolvedVmbusDevice;
    type Error = InputError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SynthMouseHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let source = resolver
            .resolve(resource.source, "synthmouse")
            .await
            .map_err(InputError::InputSource)?;
        let device = SimpleDeviceWrapper::new(input.driver_source.simple(), Mouse::new(source.0));
        Ok(device.into())
    }
}
