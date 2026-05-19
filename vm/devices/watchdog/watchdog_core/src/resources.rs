// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Watchdog platform capability resources.

use crate::platform::WatchdogPlatform;
use parking_lot::Mutex;
use thiserror::Error;
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

/// An owned watchdog platform capability consumed at resolve-time.
pub struct ResolvedWatchdogPlatform(Box<dyn WatchdogPlatform>);

impl ResolvedWatchdogPlatform {
    pub fn into_inner(self) -> Box<dyn WatchdogPlatform> {
        self.0
    }
}

#[derive(Debug, Error)]
pub enum ResolveWatchdogPlatformError {
    #[error("watchdog platform capability has already been consumed")]
    AlreadyConsumed,
}

/// A static platform resolver that serves a pre-built watchdog platform.
pub struct StaticWatchdogPlatformResolver(Mutex<Option<Box<dyn WatchdogPlatform>>>);

impl StaticWatchdogPlatformResolver {
    pub fn new(platform: Box<dyn WatchdogPlatform>) -> Self {
        Self(Mutex::new(Some(platform)))
    }
}

impl ResolveResource<WatchdogPlatformHandleKind, PlatformResource>
    for StaticWatchdogPlatformResolver
{
    type Output = ResolvedWatchdogPlatform;
    type Error = ResolveWatchdogPlatformError;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        let mut guard = self.0.lock();
        guard
            .take()
            .map(ResolvedWatchdogPlatform)
            .ok_or(ResolveWatchdogPlatformError::AlreadyConsumed)
    }
}
