// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh::MeshPayload;
use std::convert::Infallible;
use video_core::FramebufferControl;
use video_core::FramebufferFormat;
use video_core::ResolvedFramebuffer;
use video_core::SharedFramebufferHandle;
use vm_resource::ResolveResource;
use vm_resource::kind::FramebufferHandleKind;

#[derive(Clone, MeshPayload)]
pub struct FramebufferRemoteControl {
    pub get: guest_emulation_transport::GuestEmulationTransportClient,
    pub format_send: mesh::Sender<FramebufferFormat>,
}

#[async_trait::async_trait]
impl FramebufferControl for FramebufferRemoteControl {
    async fn map(&mut self, gpa: u64) {
        tracing::trace!("sending map framebuffer request via GET");
        if self.get.map_framebuffer(gpa).await.is_ok() {
            tracing::debug!("successfully mapped framebuffer at {:#x}", gpa);
        } else {
            tracing::warn!("failed to map framebuffer at {:#x}", gpa);
        }
    }

    async fn unmap(&mut self) {
        tracing::trace!("sending unmap framebuffer request via GET");
        if self.get.unmap_framebuffer().await.is_ok() {
            tracing::debug!("successfully unmapped framebuffer");
        } else {
            tracing::warn!("failed to unmap framebuffer");
        }
    }

    async fn set_format(&mut self, format: FramebufferFormat) {
        self.format_send.send(format);
    }
}

impl ResolveResource<FramebufferHandleKind, SharedFramebufferHandle> for FramebufferRemoteControl {
    type Output = ResolvedFramebuffer;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: SharedFramebufferHandle,
        (): (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.clone().into())
    }
}
