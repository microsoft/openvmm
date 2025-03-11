// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for UI devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::kind::FramebufferHandleKind;
use vm_resource::kind::KeyboardInputHandleKind;
use vm_resource::kind::MouseInputHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

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

/// Handle for a synthetic video device.
#[derive(MeshPayload)]
pub struct SynthVideoHandle {
    /// The framebuffer memory to map into the guest for rendering.
    pub framebuffer: Resource<FramebufferHandleKind>,
}

impl ResourceId<VmbusDeviceHandleKind> for SynthVideoHandle {
    const ID: &'static str = "video";
}
