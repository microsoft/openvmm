// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core abstractions for declaring and resolving type-safe test artifacts in
//! `petri`.
//!
//! NOTE: this crate does not define any concrete Artifact types itself.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// exported to support the `declare_artifacts!` macro
#[doc(hidden)]
pub use paste;

/// A trait that marks a type as being the type-safe ID for a petri artifact.
///
/// This trait should never be implemented manually! It will be automatically
/// implemented on the correct type when declaring artifacts using
/// [`declare_artifacts!`](crate::declare_artifacts).
pub trait ArtifactId: 'static {
    /// A globally unique ID corresponding to this artifact.
    #[doc(hidden)]
    const GLOBAL_UNIQUE_ID: &'static str;

    /// ...in case you decide to flaunt the trait-level docs regarding manually
    /// implementing this trait.
    #[doc(hidden)]
    fn i_know_what_im_doing_with_this_manual_impl_instead_of_using_the_declare_artifacts_macro();
}

/// A type-safe handle to a particular Artifact, as declared using the
/// [`declare_artifacts!`](crate::declare_artifacts) macro.
#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactHandle<A: ArtifactId>(core::marker::PhantomData<A>);

impl<A: ArtifactId + std::fmt::Debug> std::fmt::Debug for ArtifactHandle<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.erase(), f)
    }
}

/// A type-erased handle to a particular Artifact, with no information as to
/// what exactly the artifact is.
#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ErasedArtifactHandle {
    artifact_id_str: &'static str,
}

impl std::fmt::Debug for ErasedArtifactHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the `declare_artifacts!` macro uses `module_path!` under-the-hood to
        // generate an artifact_id_str based on the artifact's crate + module
        // path. To avoid collisions, the mod is named `TYPE_NAME__ty`, but to
        // make it easier to parse output, we strip the `__ty`.
        write!(
            f,
            "{}",
            self.artifact_id_str
                .strip_suffix("__ty")
                .unwrap_or(self.artifact_id_str)
        )
    }
}

impl<A: ArtifactId> PartialEq<ErasedArtifactHandle> for ArtifactHandle<A> {
    fn eq(&self, other: &ErasedArtifactHandle) -> bool {
        &self.erase() == other
    }
}

impl<A: ArtifactId> PartialEq<ArtifactHandle<A>> for ErasedArtifactHandle {
    fn eq(&self, other: &ArtifactHandle<A>) -> bool {
        self == &other.erase()
    }
}

impl<A: ArtifactId> ArtifactHandle<A> {
    /// Create a new typed artifact handle. It is unlikely you will need to call
    /// this directly.
    pub const fn new() -> Self {
        Self(core::marker::PhantomData)
    }
}

/// Helper trait to allow uniform handling of both typed and untyped artifact
/// handles in various contexts.
pub trait AsArtifactHandle {
    /// Return a type-erased handle to the given artifact.
    fn erase(&self) -> ErasedArtifactHandle;
}

impl AsArtifactHandle for ErasedArtifactHandle {
    fn erase(&self) -> ErasedArtifactHandle {
        *self
    }
}

impl<A: ArtifactId> AsArtifactHandle for ArtifactHandle<A> {
    fn erase(&self) -> ErasedArtifactHandle {
        ErasedArtifactHandle {
            artifact_id_str: A::GLOBAL_UNIQUE_ID,
        }
    }
}

/// Declare one or more type-safe artifacts.
#[macro_export]
macro_rules! declare_artifacts {
    (
        $(
            $(#[$doc:meta])*
            $name:ident
        ),*
        $(,)?
    ) => {
        $(
            $crate::paste::paste! {
                $(#[$doc])*
                #[allow(non_camel_case_types)]
                pub const $name: $crate::ArtifactHandle<$name> = $crate::ArtifactHandle::new();

                #[doc = concat!("Type-tag for [`",  stringify!($name), "`]")]
                #[allow(non_camel_case_types)]
                #[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
                pub enum $name {}

                #[allow(non_snake_case)]
                mod [< $name __ty >] {
                    impl $crate::ArtifactId for super::$name {
                        const GLOBAL_UNIQUE_ID: &'static str = module_path!();
                        fn i_know_what_im_doing_with_this_manual_impl_instead_of_using_the_declare_artifacts_macro() {}
                    }
                }
            }
        )*
    };
}

/// An object-safe trait used to abstract details of artifact resolution.
///
/// Test authors are expected to use the [`TestArtifactResolver`] and
/// [`TestArtifacts`] abstractions to interact with resolvers, and should not
/// use this API directly.
pub trait TestArtifactResolverBackend {
    /// Given an artifact handle, return its corresponding PathBuf.
    ///
    /// This method must use type-erased handles, as using typed artifact
    /// handles in this API would cause the trait to no longer be object-safe.
    fn resolve(&self, id: ErasedArtifactHandle) -> anyhow::Result<PathBuf>;

    /// Invoked once all calls to `resolve` has gone through.
    ///
    /// Callers must ensure that this method is called after their final call to
    /// `resolve`. By doing this, it is possible to implement "dry-run"
    /// resolvers, which simply list a test's required dependencies, and then
    /// terminate the test run.
    fn finalize(self: Box<Self>) {}
}

/// A set of dependencies required to run a test.
pub struct TestArtifactResolver {
    backend: Box<dyn TestArtifactResolverBackend>,
    artifacts: Vec<(ErasedArtifactHandle, bool)>,
}

impl TestArtifactResolver {
    /// Create an empty set of dependencies.
    pub fn new(backend: Box<dyn TestArtifactResolverBackend>) -> Self {
        TestArtifactResolver {
            backend,
            artifacts: Vec::new(),
        }
    }

    /// Add a dependency to the set of required artifacts.
    pub fn require(mut self, dependency: impl AsArtifactHandle) -> Self {
        self.artifacts.push((dependency.erase(), false));
        self
    }

    /// Add an optional dependency to the set of artifacts.
    pub fn try_require(mut self, dependency: impl AsArtifactHandle) -> Self {
        self.artifacts.push((dependency.erase(), true));
        self
    }

    /// Finalize the set of dependencies.
    pub fn finalize(self) -> TestArtifacts {
        let mut failed = String::new();
        let mut resolved = HashMap::new();

        for (a, optional) in self.artifacts {
            match self.backend.resolve(a) {
                Ok(p) => {
                    resolved.insert(a, p);
                }
                Err(_) if optional => {}
                Err(e) => failed.push_str(&format!("{:?} - {:#}\n", a, e)),
            }
        }

        self.backend.finalize();

        if !failed.is_empty() {
            panic!("Artifact resolution failed:\n{}", failed);
        }

        TestArtifacts {
            artifacts: Arc::new(resolved),
        }
    }
}

/// A resolved set of test artifacts, returned by
/// [`TestArtifactResolver::finalize`].
#[derive(Clone)]
pub struct TestArtifacts {
    artifacts: Arc<HashMap<ErasedArtifactHandle, PathBuf>>,
}

impl TestArtifacts {
    /// Try to resolve an artifact to a path.
    #[track_caller]
    pub fn try_resolve(&self, artifact: impl AsArtifactHandle) -> Option<PathBuf> {
        self.artifacts.get(&artifact.erase()).cloned()
    }

    /// Resolve an artifact to a path.
    #[track_caller]
    pub fn resolve(&self, artifact: impl AsArtifactHandle) -> PathBuf {
        self.try_resolve(artifact.erase())
            .unwrap_or_else(|| panic!("Artifact not initially required: {:?}", artifact.erase()))
    }
}
