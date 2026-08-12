// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for UI devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::FramebufferHandleKind;
use vm_resource::kind::KeyboardInputHandleKind;
use vm_resource::kind::MouseInputHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;

/// Handle for a synthetic keyboard device.
#[derive(MeshPayload)]
pub struct SynthKeyboardHandle {
    /// The source of keyboard input.
    pub source: Resource<KeyboardInputHandleKind>,
}

impl ResourceId<VmbusDeviceHandleKind> for SynthKeyboardHandle {
    const ID: &'static str = "keyboard";
}

/// Handle for a synthetic mouse device.
#[derive(MeshPayload)]
pub struct SynthMouseHandle {
    /// The source of mouse moves and clicks.
    pub source: Resource<MouseInputHandleKind>,
}

impl ResourceId<VmbusDeviceHandleKind> for SynthMouseHandle {
    const ID: &'static str = "mouse";
}

/// The pair of channels linking the synth video device to its consumer (the
/// VNC worker).
#[derive(Debug, MeshPayload)]
pub struct SynthVideoDeviceChannels {
    /// Channel for the device to forward dirty-rectangle hints to the consumer.
    pub dirty_send: mesh::Sender<Vec<video_core::DirtyRect>>,
    /// Channel by which the consumer tells the device whether the guest's
    /// screen/pointer updates are currently needed: `true` when at least one
    /// consumer (e.g. a connected VNC client) is watching, `false` when none
    /// are. The device relays this to the guest via a synthvid `FeatureChange`.
    pub updates_needed_recv: mesh::Receiver<bool>,
}

/// Handle for a synthetic video device.
#[derive(MeshPayload)]
pub struct SynthVideoHandle {
    /// The framebuffer memory to map into the guest for rendering.
    pub framebuffer: Resource<FramebufferHandleKind>,
    /// Channels to the consumer (the VNC worker), or `None` when no consumer is
    /// wired up; the device then leaves the guest at its default (everything
    /// enabled) and never sends a `FeatureChange`.
    pub channels: Option<SynthVideoDeviceChannels>,
}

impl ResourceId<VmbusDeviceHandleKind> for SynthVideoHandle {
    const ID: &'static str = "video";
}
