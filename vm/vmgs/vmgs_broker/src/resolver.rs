// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for VMGS files.

use crate::VmgsClient;
use crate::non_volatile_store::EncryptionNotSupported;
use crate::non_volatile_store::VmgsNonVolatileStore;
use vm_resource::AsyncResolveResource;
use vm_resource::CanResolveTo;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vm_resource::ResourceKind;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::NonVolatileStoreKind;
use vmcore::non_volatile_store::resources::ResolvedNonVolatileStore;
use vmgs_resources::VmgsFileHandle;

/// A resource resolver for VMGS files.
pub struct VmgsFileResolver;

declare_static_async_resolver! {
    VmgsFileResolver,
    (NonVolatileStoreKind, VmgsFileHandle),
}

/// Errors that can occur while resolving a VMGS file.
#[derive(Debug, thiserror::Error)]
pub enum VmgsFileResolverError {
    /// Error creating the VMGS non-volatile store.
    #[error("error creating VMGS non-volatile store")]
    Store(#[source] EncryptionNotSupported),
    /// Error resolving the underlying non-volatile store.
    #[error("error resolving VMGS client")]
    Client(#[source] vm_resource::ResolveError),
}

#[async_trait::async_trait]
impl AsyncResolveResource<NonVolatileStoreKind, VmgsFileHandle> for VmgsFileResolver {
    type Output = ResolvedNonVolatileStore;
    type Error = VmgsFileResolverError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VmgsFileHandle,
        _: &(),
    ) -> Result<Self::Output, Self::Error> {
        Ok(VmgsNonVolatileStore::new(
            resolver
                .resolve::<VmgsClientKind, _>(PlatformResource.into_resource(), ())
                .await
                .map_err(VmgsFileResolverError::Client)?,
            vmgs_format::FileId(resource.file_id),
            resource.encrypted,
        )
        .map_err(VmgsFileResolverError::Store)?
        .into())
    }
}

/// A resource kind for getting a [`VmgsClient`].
///
/// This is primarily used with [`PlatformResource`].
pub enum VmgsClientKind {}

impl ResourceKind for VmgsClientKind {
    const NAME: &'static str = "vmgs_client";
}

impl CanResolveTo<VmgsClient> for VmgsClientKind {
    type Input<'a> = ();
}

impl ResolveResource<VmgsClientKind, PlatformResource> for VmgsClient {
    type Output = VmgsClient;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        PlatformResource: PlatformResource,
        (): (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.clone())
    }
}
