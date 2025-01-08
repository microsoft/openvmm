// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for type-erased "resource" types, representing interchangeable
//! resources that can be resolved into the same type at runtime.
//!
//! This allows a device's resources to be described via [`mesh`] messages so
//! that the device initialization code does not have to be statically aware of
//! all the different possible resource types (e.g. via an `enum`). VMMs can
//! link in different resource resolvers to support different resource types
//! depending on compile-time configuration.

#![warn(missing_docs)]
// UNSAFETY: Uses transmute to allow for type erasure.
#![expect(unsafe_code)]

pub mod kind;

use async_trait::async_trait;
use inspect::Inspect;
use mesh::MeshPayload;
use mesh::Message;
use std::any::Any;
use std::borrow::Cow;
use std::fmt::Display;
use std::marker::PhantomData;
use std::sync::Arc;
use thiserror::Error;

/// Trait implemented by resource kinds.
///
/// A resource kind defines a family of interchangeable resource types, where
/// each resource type can be resolved to the same output type.
///
/// The output type is specified in the [`CanResolveTo`] trait.
///
/// Typically this trait will be implemented on an uninhabited tag type, e.g.
///
/// ```
/// enum DiskKind {}
///
/// trait Disk {};
///
/// impl vm_resource::ResourceKind for DiskKind {
///     const NAME: &'static str = "disk";
/// }
/// ```
pub trait ResourceKind: 'static + Send + Sync {
    /// The name of the resource kind. This must be unique amongst resource kinds.
    const NAME: &'static str;
}

/// Trait specifying that a [`ResourceKind`] can be resolved to a given output
/// type.
///
/// This should be implemented exactly once for each resource kind so that
/// Rust's type inference can determine the output type without callers having
/// to be explicit.
///
/// This trait is separate from [`ResourceKind`] so that it can be implemented
/// in a separate crate without violating Rust's coherence (orphan) rules. This
/// is important because the type a resource resolves to is usually of no
/// interest to the client constructing the resource, so there is no need to
/// include the crate defining the output time in the client's dependency graph.
pub trait CanResolveTo<O>: ResourceKind {
    /// Additional input (besides the resource itself) when resolving resources
    /// of this resource kind.
    type Input<'a>: Send;
}

/// An opaque resource of kind `K`, for erasing the resource's type.
///
/// The resource can later be resolved with a [`ResourceResolver`].
#[derive(MeshPayload)]
#[mesh(bound = "")]
pub struct Resource<K: ResourceKind> {
    #[mesh(encoding = "mesh::payload::encoding::OwningCowField")]
    id: Cow<'static, str>,
    message: Message,
    _phantom: PhantomData<fn(K) -> K>,
}

/// Trait for converting resources into opaque [`Resource`]s.
pub trait IntoResource<K: ResourceKind> {
    /// Converts `self` into a `Resource`.
    fn into_resource(self) -> Resource<K>;
}

impl<T: 'static + ResourceId<K> + MeshPayload + Send, K: ResourceKind> IntoResource<K> for T {
    fn into_resource(self) -> Resource<K> {
        Resource::new(self)
    }
}

impl<K: ResourceKind> Resource<K> {
    /// Wraps `value` as an opaque resource.
    pub fn new<T: 'static + ResourceId<K> + MeshPayload + Send>(value: T) -> Self {
        Self {
            id: Cow::Borrowed(T::ID),
            message: Message::new(value),
            _phantom: PhantomData,
        }
    }

    /// Returns the ID of the resource type.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl<K: ResourceKind> std::fmt::Debug for Resource<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resource").field("id", &self.id).finish()
    }
}

/// The generic resource defined for all resource kinds.
///
/// This can be used to get the default resource for the platform, for kinds
/// where that is supported and a platform resource is registered.
#[derive(MeshPayload)]
pub struct PlatformResource;

impl<K: ResourceKind> ResourceId<K> for PlatformResource {
    const ID: &'static str = "platform";
}

/// A trait identifying a resource type's ID (within a given [`ResourceKind`]).
///
/// ```
/// enum DiskKind {}
///
/// #[derive(mesh::MeshPayload)]
/// struct FileDiskConfig {
///     path: String,
/// }
///
/// impl vm_resource::ResourceId<DiskKind> for FileDiskConfig {
///     const ID: &'static str = "file";
/// }
/// ```
pub trait ResourceId<K> {
    /// The ID of this resource type.
    ///
    /// This must be unique amongst resource types of this kind. It does not
    /// need to be unique between types of different resource kinds.
    const ID: &'static str;
}

/// Trait implemented to resolve resource type `T` as resource kind `K`.
pub trait ResolveResource<K: CanResolveTo<Self::Output>, T>: Send + Sync {
    /// The output type for resource resolution.
    type Output;
    /// The error type for `resolve`.
    type Error: Into<Box<dyn std::error::Error + Send + Sync>>;

    /// Resolves the resource.
    fn resolve(&self, resource: T, input: K::Input<'_>) -> Result<Self::Output, Self::Error>;
}

/// Trait implemented to resolve resource type `T` as resource kind `K`.
///
/// Unlike [`ResolveResource`], this allows for async operation, including
/// calling into other resource resolvers to resolve sub-resources.
#[async_trait]
pub trait AsyncResolveResource<K: CanResolveTo<Self::Output>, T>: Send + Sync {
    /// The output type for resource resolution.
    type Output;
    /// The error type for `resolve`.
    type Error: Into<Box<dyn std::error::Error + Send + Sync>>;

    /// Resolves the resource.
    ///
    /// `resolver` can be used to resolve sub-resources.
    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: T,
        input: K::Input<'_>,
    ) -> Result<Self::Output, Self::Error>;
}

#[repr(transparent)]
struct TypedResolver<T, R> {
    resolver: R,
    _phantom: PhantomData<fn(T)>,
}

#[repr(transparent)]
struct TypedAsyncResolver<T, R> {
    resolver: R,
    _phantom: PhantomData<fn(T)>,
}

#[async_trait]
trait DynResolveResource<K: CanResolveTo<O>, O>: Send + Sync {
    async fn dyn_resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Resource<K>,
        input: K::Input<'_>,
    ) -> Result<O, ResolveError>;
}

#[async_trait]
impl<K, R, T, O> DynResolveResource<K, O> for TypedResolver<T, R>
where
    K: CanResolveTo<O>,
    O: 'static,
    R: ResolveResource<K, T, Output = O>,
    T: 'static + MeshPayload + ResourceId<K> + Send,
{
    async fn dyn_resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: Resource<K>,
        input: K::Input<'_>,
    ) -> Result<O, ResolveError> {
        let parsed = resource
            .message
            .parse()
            .map_err(|source| ResolveError::ParseError {
                kind: K::NAME,
                id: resource.id.clone(),
                source,
            })?;

        let resolved =
            self.resolver
                .resolve(parsed, input)
                .map_err(|source| ResolveError::ResolverError {
                    kind: K::NAME,
                    id: resource.id.clone(),
                    source: source.into(),
                })?;

        Ok(resolved)
    }
}

#[async_trait]
impl<R, T, K, O> DynResolveResource<K, O> for TypedAsyncResolver<T, R>
where
    K: CanResolveTo<O>,
    O: 'static,
    R: AsyncResolveResource<K, T, Output = O>,
    T: 'static + MeshPayload + ResourceId<K> + Send,
{
    async fn dyn_resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Resource<K>,
        input: K::Input<'_>,
    ) -> Result<O, ResolveError> {
        let parsed = resource
            .message
            .parse()
            .map_err(|source| ResolveError::ParseError {
                kind: K::NAME,
                id: resource.id.clone(),
                source,
            })?;

        let resolved = self
            .resolver
            .resolve(resolver, parsed, input)
            .await
            .map_err(|source| ResolveError::ResolverError {
                kind: K::NAME,
                id: resource.id.clone(),
                source: source.into(),
            })?;

        Ok(resolved)
    }
}

struct UntypedResolver<K, O>(Box<dyn DynResolveResource<K, O>>);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResolverKey {
    kind: &'static str,
    id: &'static str,
}

impl Display for ResolverKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}

/// A resource resolver capable of resolving resources of multiple types and
/// kinds.
#[derive(Clone)]
pub struct ResourceResolver {
    resolvers: Arc<Vec<(ResolverKey, Arc<dyn Any + Send + Sync>)>>,
}

impl Inspect for ResourceResolver {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for private::StaticResolver { key, .. } in private::STATIC_RESOLVERS
            .iter()
            .copied()
            .flatten()
            .copied()
            .flatten()
        {
            resp.child(&format!("{}/{}", key.kind, key.id), |req| {
                req.respond();
            });
        }
        for (key, _) in &*self.resolvers {
            resp.child(&format!("{}/{}", key.kind, key.id), |req| {
                req.respond();
            });
        }
    }
}

/// Declares the resource kinds and types that a static resolver can resolve.
///
/// This can be used along with [`register_static_resolvers`] to build up the
/// list of resolvers at link time, simplifying construction of the resolver
/// list.
///
/// ```ignore
/// declare_static_resolver! {
///     FileDiskResolver,
///     (DiskConfigKind, FileDiskConfig),
///     (DiskHandleKind, FileDiskHandle),
/// }
/// ```
#[macro_export]
macro_rules! declare_static_resolver {
    ($resolver:tt, $(($kind:ty, $resource:ty $(,)?)),* $(,)?) => {
        const _: () = {
            use $crate::private::{StaticResolver, StaticResolverList, UntypedStaticResolver};

            impl StaticResolverList for $resolver {
                const RESOLVERS: &'static [StaticResolver] = &[
                    $(StaticResolver::new::<$kind, $resource, _>(&UntypedStaticResolver::new::<$resource, _>(&$resolver)),)*
                ];
            }
        };
    };
}

/// Declares the resource kinds and types that an async static resolver can
/// resolve.
///
/// See [`declare_static_resolver`].
#[macro_export]
macro_rules! declare_static_async_resolver {
    ($resolver:tt, $(($kind:ty, $resource:ty $(,)?)),* $(,)?) => {
        const _: () = {
            use $crate::private::{StaticResolver, StaticResolverList, UntypedStaticResolver};

            impl StaticResolverList for $resolver {
                const RESOLVERS: &'static [StaticResolver] = &[
                    $(StaticResolver::new::<$kind, $resource, _>(&UntypedStaticResolver::new_async::<$resource, _>(&$resolver)),)*
                ];
            }
        };
    };
}

/// Registers a static resolver, declared via [`declare_static_resolver`] or
/// [`declare_static_async_resolver`], so that it is automatically available to
/// any [`ResourceResolver`] in the binary.
#[macro_export]
macro_rules! register_static_resolvers {
    {} => {};
    { $( $(#[$a:meta])* $resolver:ty ),+ $(,)? } => {
        $(
        $(#[$a])*
        const _: () = {
            use $crate::private::{linkme, StaticResolver, StaticResolverList, STATIC_RESOLVERS};

            #[linkme::distributed_slice(STATIC_RESOLVERS)]
            #[linkme(crate = linkme)]
            static RESOLVER: Option<&'static &'static [StaticResolver]> =
                Some(&<$resolver as StaticResolverList>::RESOLVERS);
        };
        )*
    };
}

#[doc(hidden)]
pub mod private {
    use super::AsyncResolveResource;
    use super::DynResolveResource;
    use super::ResolveResource;
    use super::ResolverKey;
    use super::ResourceId;
    use super::TypedAsyncResolver;
    use super::TypedResolver;
    use crate::CanResolveTo;
    pub use linkme;
    use mesh::MeshPayload;
    use std::any::Any;

    // Use Option<&&[X]> in case the linker inserts some stray nulls, as we
    // think it might on Windows. The double pointer is necessary since &[X]
    // alone is two pointers wide.
    //
    // See <https://devblogs.microsoft.com/oldnewthing/20181108-00/?p=100165>.
    #[linkme::distributed_slice]
    pub static STATIC_RESOLVERS: [Option<&'static &'static [StaticResolver]>] = [..];

    // Always have at least one entry to work around linker bugs.
    //
    // See <https://github.com/llvm/llvm-project/issues/65855>.
    #[linkme::distributed_slice(STATIC_RESOLVERS)]
    static WORKAROUND: Option<&'static &'static [StaticResolver]> = None;

    pub trait StaticResolverList: Send {
        const RESOLVERS: &'static [StaticResolver];
    }

    pub struct StaticResolver {
        pub(super) key: ResolverKey,
        pub(super) resolver: &'static (dyn Any + Send + Sync),
    }

    pub struct UntypedStaticResolver<K: CanResolveTo<O>, O: 'static>(
        pub(super) &'static dyn DynResolveResource<K, O>,
    );

    impl<K: CanResolveTo<O>, O> UntypedStaticResolver<K, O> {
        pub const fn new<T, R>(resolver: &'static R) -> Self
        where
            T: 'static + ResourceId<K> + MeshPayload + Send,
            R: ResolveResource<K, T, Output = O>,
        {
            // SAFETY: TypedResolver<T, R> contains a &'static R and is transparent.
            let resolver = unsafe {
                std::mem::transmute::<&'static R, &'static TypedResolver<T, R>>(resolver)
            };
            Self(resolver)
        }

        pub const fn new_async<T, R>(resolver: &'static R) -> Self
        where
            T: 'static + ResourceId<K> + MeshPayload + Send,
            R: AsyncResolveResource<K, T, Output = O>,
        {
            // SAFETY: TypedAsyncResolver<T, R> contains a &'static R and is transparent.
            let resolver = unsafe {
                std::mem::transmute::<&'static R, &'static TypedAsyncResolver<T, R>>(resolver)
            };
            Self(resolver)
        }
    }

    impl StaticResolver {
        pub const fn new<K: CanResolveTo<O>, T: ResourceId<K> + MeshPayload, O>(
            resolver: &'static UntypedStaticResolver<K, O>,
        ) -> Self {
            Self {
                key: ResolverKey {
                    kind: K::NAME,
                    id: T::ID,
                },
                resolver,
            }
        }
    }
}

/// An error returned by [`ResourceResolver::resolve`].
#[derive(Debug, Error)]
pub enum ResolveError {
    /// The resolver can't be found.
    #[error("no resolver for {kind}:{id}")]
    NoResolver {
        /// The resource kind.
        kind: &'static str,
        /// The resource type's ID.
        id: Cow<'static, str>,
    },
    /// The resource couldn't be parsed back to the expected type.
    #[error("failed to parse resource of type {kind}:{id}")]
    ParseError {
        /// The resource kind.
        kind: &'static str,
        /// The resource type's ID.
        id: Cow<'static, str>,
        /// The underlying error.
        #[source]
        source: mesh::payload::Error,
    },
    /// The resource couldn't be resolved.
    #[error("failed to resolve resource of type {kind}:{id}")]
    ResolverError {
        /// The resource kind.
        kind: &'static str,
        /// The resource type's ID.
        id: Cow<'static, str>,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl ResourceResolver {
    /// Returns a new resolver, which initially only supports the static resolvers.
    pub fn new() -> Self {
        // Ensure the static resolvers don't have duplicates.
        let mut static_resolvers = private::STATIC_RESOLVERS
            .iter()
            .copied()
            .flatten()
            .copied()
            .flatten()
            .collect::<Vec<_>>();

        static_resolvers.sort_by_key(|r| &r.key);
        for (x, y) in static_resolvers.iter().zip(static_resolvers.iter().skip(1)) {
            if x.key == y.key {
                panic!("duplicate static resolver for {}", x.key);
            }
        }

        Self {
            resolvers: Arc::new(Vec::new()),
        }
    }

    /// Adds a dynamic resolver.
    ///
    /// Panics if a resolver already exists for this resource type.
    pub fn add_resolver<K, O, T, R>(&mut self, resolver: R)
    where
        K: CanResolveTo<O>,
        O: 'static,
        T: 'static + ResourceId<K> + MeshPayload + Send,
        R: 'static + ResolveResource<K, T, Output = O>,
    {
        let key = ResolverKey {
            kind: K::NAME,
            id: T::ID,
        };
        if self.find_resolver::<K, O>(T::ID).is_some() {
            panic!("duplicate resolver for {}", key);
        }
        let resolver = TypedResolver::<T, _> {
            resolver,
            _phantom: PhantomData,
        };
        let resolver = UntypedResolver::<K, O>(Box::new(resolver));
        Arc::make_mut(&mut self.resolvers).push((key, Arc::new(resolver)));
    }

    /// Adds a dynamic async resolver.
    ///
    /// Panics if a resolver already exists for this resource type.
    pub fn add_async_resolver<K, O, T, R>(&mut self, resolver: R)
    where
        K: CanResolveTo<O>,
        O: 'static,
        T: 'static + ResourceId<K> + MeshPayload + Send,
        R: 'static + AsyncResolveResource<K, T, Output = O>,
    {
        let key = ResolverKey {
            kind: K::NAME,
            id: T::ID,
        };
        if self.find_resolver::<K, O>(T::ID).is_some() {
            panic!("duplicate resolver for {}", key);
        }
        let resolver = TypedAsyncResolver::<T, _> {
            resolver,
            _phantom: PhantomData,
        };
        let resolver = UntypedResolver::<K, O>(Box::new(resolver));
        Arc::make_mut(&mut self.resolvers).push((key, Arc::new(resolver)));
    }

    fn find_resolver<K: CanResolveTo<O>, O: 'static>(
        &self,
        id: &str,
    ) -> Option<&dyn DynResolveResource<K, O>> {
        for private::StaticResolver { key, resolver } in private::STATIC_RESOLVERS
            .iter()
            .copied()
            .flatten()
            .copied()
            .flatten()
        {
            if key.kind == K::NAME && key.id == id {
                return Some(
                    resolver
                        .downcast_ref::<private::UntypedStaticResolver<K, O>>()
                        .unwrap()
                        .0,
                );
            }
        }
        for (key, resolver) in &*self.resolvers {
            if key.kind == K::NAME && key.id == id {
                return Some(
                    resolver
                        .downcast_ref::<UntypedResolver<K, O>>()
                        .unwrap()
                        .0
                        .as_ref(),
                );
            }
        }
        None
    }

    /// Resolves a resource.
    pub async fn resolve<K: CanResolveTo<O>, O: 'static>(
        &self,
        resource: Resource<K>,
        input: K::Input<'_>,
    ) -> Result<O, ResolveError> {
        let resolver =
            self.find_resolver(&resource.id)
                .ok_or_else(|| ResolveError::NoResolver {
                    kind: K::NAME,
                    id: resource.id.clone(),
                })?;

        resolver.dyn_resolve(self, resource, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::ResolveResource;
    use super::Resource;
    use super::ResourceId;
    use super::ResourceKind;
    use super::ResourceResolver;
    use crate::CanResolveTo;
    use mesh::payload::Protobuf;
    use mesh::MeshPayload;
    use pal_async::async_test;
    use std::convert::Infallible;

    enum TestConfigKind {}

    impl ResourceKind for TestConfigKind {
        const NAME: &'static str = "test_config";
    }

    impl CanResolveTo<Resource<TestHandleKind>> for TestConfigKind {
        type Input<'a> = ();
    }

    enum TestHandleKind {}

    impl ResourceKind for TestHandleKind {
        const NAME: &'static str = "test_handle";
    }

    impl CanResolveTo<TestConcreteObject> for TestHandleKind {
        type Input<'a> = ();
    }

    #[derive(Protobuf)]
    struct TestConfig {
        value: u32,
    }

    impl ResourceId<TestConfigKind> for TestConfig {
        const ID: &'static str = "foo";
    }

    #[derive(MeshPayload)]
    struct TestHandle {
        valuex2: u32,
    }

    impl ResourceId<TestHandleKind> for TestHandle {
        const ID: &'static str = "open_foo";
    }

    struct TestConcreteObject {
        result: String,
    }

    struct TestResolver;

    impl ResolveResource<TestConfigKind, TestConfig> for TestResolver {
        type Output = Resource<TestHandleKind>;
        type Error = Infallible;

        fn resolve(
            &self,
            resource: TestConfig,
            _: (),
        ) -> Result<Resource<TestHandleKind>, Self::Error> {
            Ok(Resource::new(TestHandle {
                valuex2: resource.value * 2,
            }))
        }
    }

    impl ResolveResource<TestHandleKind, TestHandle> for TestResolver {
        type Output = TestConcreteObject;
        type Error = Infallible;

        fn resolve(&self, resource: TestHandle, _: ()) -> Result<TestConcreteObject, Self::Error> {
            Ok(TestConcreteObject {
                result: resource.valuex2.to_string(),
            })
        }
    }

    declare_static_resolver!(
        TestResolver,
        (TestConfigKind, TestConfig),
        (TestHandleKind, TestHandle),
    );

    register_static_resolvers!(TestResolver);

    #[async_test]
    async fn test_resources() {
        let resolver = ResourceResolver::new();

        // Resolve from TestConfig -> TestHandle -> TestConcreteResult.
        let x = resolver
            .resolve(Resource::new(TestConfig { value: 5 }), ())
            .await
            .unwrap();

        assert_eq!(resolver.resolve(x, ()).await.unwrap().result, "10");
    }
}
