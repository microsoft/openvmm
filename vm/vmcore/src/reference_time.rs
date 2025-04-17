// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for hypervisor reference time sources.

use inspect::Inspect;
use std::convert::Infallible;
use std::sync::Arc;
use vm_resource::CanResolveTo;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vm_resource::ResourceKind;

/// Trait for getting the reference time.
pub trait GetReferenceTime: Send + Sync {
    /// Returns the current time.
    fn now(&self) -> ReferenceTimeResult;
}

/// The result of a call to [`GetReferenceTime::now()`].
pub struct ReferenceTimeResult {
    /// The reference time in 100ns units.
    pub ref_time: u64,
    /// Optionally, the system time (in UTC, without leap seconds) at the time
    /// `ref_time` was snapped.
    ///
    /// This is returned if the time source is able to cheaply provide a value
    /// synchronized with `ref_time`. Otherwise, it is `None`.
    pub system_time: Option<jiff::Timestamp>,
}

/// A resource kind for accessing the partition's reference time.
///
/// Only the platform resource makes sense for this resource kind,
/// since there is only one reference time for a partition.
pub enum ReferenceTimeSourceKind {}

impl ResourceKind for ReferenceTimeSourceKind {
    const NAME: &'static str = "ref_time";
}

impl CanResolveTo<ReferenceTimeSource> for ReferenceTimeSourceKind {
    type Input<'a> = ();
}

/// A time source that can be used to get the current VM reference time in 100ns
/// units.
#[derive(Clone)]
pub struct ReferenceTimeSource(Arc<dyn GetReferenceTime>);

impl ReferenceTimeSource {
    /// Creates a new reference time source.
    pub fn new<T: GetReferenceTime + 'static>(time_source: T) -> Self {
        Self(Arc::new(time_source))
    }

    /// Returns the current time.
    pub fn now(&self) -> ReferenceTimeResult {
        self.0.now()
    }
}

impl Inspect for ReferenceTimeSource {
    fn inspect(&self, req: inspect::Request<'_>) {
        let now = self.now();
        req.respond()
            .field("ref_time", now.ref_time)
            .field("system_time", now.system_time.map(inspect::AsDisplay));
    }
}

impl From<Arc<dyn GetReferenceTime>> for ReferenceTimeSource {
    fn from(value: Arc<dyn GetReferenceTime>) -> Self {
        Self(value)
    }
}

impl ResolveResource<ReferenceTimeSourceKind, PlatformResource> for ReferenceTimeSource {
    type Output = ReferenceTimeSource;
    type Error = Infallible;

    fn resolve(
        &self,
        PlatformResource: PlatformResource,
        (): (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.clone())
    }
}
