// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common video device-related definitions.

#![forbid(unsafe_code)]

use inspect::Inspect;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use vm_resource::CanResolveTo;
use vm_resource::ResourceId;
use vm_resource::kind::FramebufferHandleKind;

impl CanResolveTo<ResolvedFramebuffer> for FramebufferHandleKind {
    type Input<'a> = ();
}

/// A resolved framebuffer.
pub struct ResolvedFramebuffer(pub Box<dyn FramebufferControl>);

impl<T: 'static + FramebufferControl> From<T> for ResolvedFramebuffer {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

/// A handle to the global shared framebuffer that has been mapped with the VM.
#[derive(MeshPayload)]
pub struct SharedFramebufferHandle;

impl ResourceId<FramebufferHandleKind> for SharedFramebufferHandle {
    const ID: &'static str = "shared";
}

/// The framebuffer memory format.
#[derive(Debug, Copy, Clone, Protobuf, PartialEq, Eq, Inspect)]
#[mesh(package = "framebuffer")]
pub struct FramebufferFormat {
    /// Width in pixels.
    #[mesh(1)]
    pub width: usize,
    /// Height in pixels.
    #[mesh(2)]
    pub height: usize,
    /// Bytes per scanline.
    #[mesh(3)]
    pub bytes_per_line: usize,
    /// Starting offset.
    #[mesh(4)]
    pub offset: usize,
}

/// A dirty rectangle in pixel coordinates, reported by the guest video driver.
/// Used to communicate changed screen regions from the synthetic video device
/// to the VNC worker.
#[derive(Copy, Clone, Debug, MeshPayload)]
pub struct DirtyRect {
    /// Left edge (inclusive).
    pub left: i32,
    /// Top edge (inclusive).
    pub top: i32,
    /// Right edge (exclusive).
    pub right: i32,
    /// Bottom edge (exclusive).
    pub bottom: i32,
}

/// Functions necessary to control the framebuffer from a video device.
///
/// This trait needs to be async so that an implementation of these functions can be async.
///
/// For example, the GET request needed to map the framebuffer from Underhill is async since
/// the video device needs to wait for a response from the host to send an ack to the guest.
#[async_trait::async_trait]
pub trait FramebufferControl: Send {
    /// Maps the framebuffer to the guest at the specified GPA.
    async fn map(&mut self, gpa: u64);
    /// Unmaps the framebuffer from the guest.
    async fn unmap(&mut self);
    /// Updates the framebuffer format.
    async fn set_format(&mut self, format: FramebufferFormat);
}
