// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Watchdog platform capability resources.

use crate::platform::WatchdogPlatform;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::Arc;
use vm_resource::CanResolveTo;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vm_resource::ResourceKind;

/// Resource kind for obtaining a guest-watchdog platform capability.
///
/// This is primarily used with [`PlatformResource`].
pub enum WatchdogPlatformHandleKind {}

impl ResourceKind for WatchdogPlatformHandleKind {
    const NAME: &'static str = "watchdog_platform";
}

impl CanResolveTo<ResolvedWatchdogPlatform> for WatchdogPlatformHandleKind {
    type Input<'a> = ();
}

/// A one-shot watchdog platform capability.
///
/// The underlying platform object is consumed once via [`Self::take`].
#[derive(Clone)]
pub struct ResolvedWatchdogPlatform(Arc<Mutex<Option<Box<dyn WatchdogPlatform>>>>);

impl ResolvedWatchdogPlatform {
    /// Creates a new one-shot platform capability around `platform`.
    pub fn new(platform: Box<dyn WatchdogPlatform>) -> Self {
        Self(Arc::new(Mutex::new(Some(platform))))
    }

    /// Takes ownership of the platform object.
    ///
    /// Returns `None` if it has already been taken.
    pub fn take(&self) -> Option<Box<dyn WatchdogPlatform>> {
        let mut guard = self.0.lock();
        guard.take()
    }
}

/// A static platform resolver that serves a pre-built watchdog platform.
pub struct StaticWatchdogPlatformResolver(pub ResolvedWatchdogPlatform);

impl ResolveResource<WatchdogPlatformHandleKind, PlatformResource>
    for StaticWatchdogPlatformResolver
{
    type Output = ResolvedWatchdogPlatform;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.0.clone())
    }
}
