// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common input device-related definitions.

#![forbid(unsafe_code)]

pub mod mesh_input;

use mesh::MeshPayload;
use std::pin::Pin;
use vm_resource::CanResolveTo;
use vm_resource::ResourceId;
use vm_resource::kind::KeyboardInputHandleKind;
use vm_resource::kind::MouseInputHandleKind;

/// Keyboard or mouse input data.
#[derive(Debug, Copy, Clone, MeshPayload)]
pub enum InputData {
    /// A keystoke.
    Keyboard(KeyboardData),
    /// A mouse move or click.
    Mouse(MouseData),
}

/// A mouse input event.
#[derive(Debug, Copy, Clone, MeshPayload)]
pub struct MouseData {
    /// A bitmask of the buttons that are pressed.
    pub button_mask: u8,
    /// The absolute X location.
    pub x: u16,
    /// The absolute Y location.
    pub y: u16,
}

/// A keyboard input event.
#[derive(Debug, Copy, Clone, MeshPayload)]
pub struct KeyboardData {
    /// Keyboard code.
    pub code: u16,
    /// True if this is a "make", false if it is a "break".
    pub make: bool,
}

/// Trait implemented by input sources.
pub trait InputSource<T>: futures::Stream<Item = T> + Unpin + Send {
    /// Sets this input source active, so that the sending side can choose which
    /// device to send input to.
    fn set_active(&mut self, active: bool) -> Pin<Box<dyn '_ + Future<Output = ()> + Send>>;
}

/// A resolved [`InputSource`].
pub struct ResolvedInputSource<T>(pub Box<dyn InputSource<T>>);

impl<T: 'static + InputSource<KeyboardData>> From<T> for ResolvedInputSource<KeyboardData> {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

impl<T: 'static + InputSource<MouseData>> From<T> for ResolvedInputSource<MouseData> {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

impl CanResolveTo<ResolvedInputSource<KeyboardData>> for KeyboardInputHandleKind {
    type Input<'a> = &'a str;
}

impl CanResolveTo<ResolvedInputSource<MouseData>> for MouseInputHandleKind {
    type Input<'a> = &'a str;
}

/// An input handle for input multiplexed over an input channel serving multiple
/// devices.
#[derive(MeshPayload)]
pub struct MultiplexedInputHandle {
    /// The elevation of this device on the input stack. The active device with
    /// the highest elevation will receive the input.
    ///
    /// Each device must have a distinct elevation.
    pub elevation: usize,
}

impl ResourceId<KeyboardInputHandleKind> for MultiplexedInputHandle {
    const ID: &'static str = "keyboard";
}

impl ResourceId<MouseInputHandleKind> for MultiplexedInputHandle {
    const ID: &'static str = "mouse";
}
