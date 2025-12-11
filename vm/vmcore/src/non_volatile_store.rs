// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the [`NonVolatileStore`] trait.

#![forbid(unsafe_code)]

use thiserror::Error;

/// Error when accessing a [`NonVolatileStore`]
#[derive(Error, Debug)]
#[error("error accessing non-volatile store")]
pub struct NonVolatileStoreError(#[from] anyhow::Error);

impl NonVolatileStoreError {
    /// Create a new [`NonVolatileStoreError`]
    pub fn new(e: impl Into<anyhow::Error>) -> NonVolatileStoreError {
        Self(e.into())
    }
}

/// Save and restore hunks of data to a non-volatile storage medium.
///
/// E.g: certain devices contain onboard non-volatile storage (e.g: UEFI's nvram
/// variables, the TPM's internal state, etc...) that must be persisted across
/// reboots.
///
/// This trait provides a generic mechanism for persisting / restoring this kind
/// of non-volatile data, while leaving the details of how it gets stored to
/// supporting infrastructure.
#[async_trait::async_trait]
pub trait NonVolatileStore: Send + Sync {
    /// Write `data` to a non-volatile storage medium.
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError>;

    /// Read any previously written `data`. Returns `None` if no data exists.
    async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError>;
}

// Boilerplate: forward `NonVolatileStore` methods for `Box<dyn NonVolatileStore>`
#[async_trait::async_trait]
impl NonVolatileStore for Box<dyn NonVolatileStore> {
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError> {
        (**self).persist(data).await
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError> {
        (**self).restore().await
    }
}

// Boilerplate: forward `NonVolatileStore` methods for `&mut NonVolatileStore`
#[async_trait::async_trait]
impl<T> NonVolatileStore for &mut T
where
    T: NonVolatileStore,
{
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError> {
        (**self).persist(data).await
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError> {
        (**self).restore().await
    }
}

/// An ephemeral implementation of [`NonVolatileStore`] backed by an in-memory
/// buffer. Useful for tests, stateless VM scenarios.
#[derive(Default)]
pub struct EphemeralNonVolatileStore(Option<Vec<u8>>);

impl EphemeralNonVolatileStore {
    /// Shortcut to create a [`Box<dyn NonVolatileStore>`] backed by an
    /// [`EphemeralNonVolatileStore`].
    pub fn new_boxed() -> Box<dyn NonVolatileStore> {
        Box::new(Self::default())
    }
}

#[async_trait::async_trait]
impl NonVolatileStore for EphemeralNonVolatileStore {
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError> {
        self.0 = Some(data);
        Ok(())
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError> {
        Ok(self.0.clone())
    }
}

/// Resource-related definitions.
///
/// TODO: split resolvers and resources, move resources to another crate.
pub mod resources {
    use super::EphemeralNonVolatileStore;
    use super::NonVolatileStore;
    use mesh::MeshPayload;
    use std::convert::Infallible;
    use vm_resource::CanResolveTo;
    use vm_resource::ResolveResource;
    use vm_resource::ResourceId;
    use vm_resource::declare_static_resolver;
    use vm_resource::kind::NonVolatileStoreKind;

    impl CanResolveTo<ResolvedNonVolatileStore> for NonVolatileStoreKind {
        type Input<'a> = &'a ();
    }

    /// The output from resolving a [`NonVolatileStoreKind`].
    pub struct ResolvedNonVolatileStore(pub Box<dyn NonVolatileStore>);

    impl<T: 'static + NonVolatileStore> From<T> for ResolvedNonVolatileStore {
        fn from(store: T) -> Self {
            Self(Box::new(store))
        }
    }

    /// A resolver for [`EphemeralNonVolatileStore`].
    pub struct EphemeralNonVolatileStoreResolver;

    /// A resource handle for [`EphemeralNonVolatileStore`].
    #[derive(MeshPayload)]
    pub struct EphemeralNonVolatileStoreHandle;

    impl ResourceId<NonVolatileStoreKind> for EphemeralNonVolatileStoreHandle {
        const ID: &'static str = "ephemeral";
    }

    declare_static_resolver! {
        EphemeralNonVolatileStoreResolver,
        (NonVolatileStoreKind, EphemeralNonVolatileStoreHandle),
    }

    impl ResolveResource<NonVolatileStoreKind, EphemeralNonVolatileStoreHandle>
        for EphemeralNonVolatileStoreResolver
    {
        type Error = Infallible;
        type Output = ResolvedNonVolatileStore;

        fn resolve(
            &self,
            EphemeralNonVolatileStoreHandle: EphemeralNonVolatileStoreHandle,
            _input: &(),
        ) -> Result<Self::Output, Infallible> {
            Ok(EphemeralNonVolatileStore::default().into())
        }
    }
}
