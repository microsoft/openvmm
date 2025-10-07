// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Power request resources, for powering off, restarting, and hibernating VMs.

#![forbid(unsafe_code)]

use mesh::payload::Protobuf;
use std::sync::Arc;
use vm_resource::CanResolveTo;
use vm_resource::ResourceKind;

/// Resource kind for power requests.
pub enum PowerRequestHandleKind {}

impl ResourceKind for PowerRequestHandleKind {
    const NAME: &'static str = "power_request";
}

impl CanResolveTo<PowerRequestClient> for PowerRequestHandleKind {
    type Input<'a> = ();
}

/// Type erased object for requesting power state changes.
#[derive(Clone)]
pub struct PowerRequestClient(Arc<dyn Fn(PowerRequest) + Send + Sync>);

impl PowerRequestClient {
    /// Issues a power request.
    pub fn power_request(&self, request: PowerRequest) {
        (self.0)(request)
    }
}

impl<T: 'static + Fn(PowerRequest) + Send + Sync> From<T> for PowerRequestClient {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

/// A VM power request.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf)]
pub enum PowerRequest {
    /// Power off the VM.
    PowerOff,
    /// Restart the VM.
    Reset,
    /// Hibernate the VM.
    Hibernate,
    /// Triple fault the VM.
    TripleFault {
        /// The VP that caused the triple fault.
        vp: u32,
    },
}
