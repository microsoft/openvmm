// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types and traits used to create and work with flowey nodes.

mod github_context;
mod spec;

pub use github_context::GhOutput;
pub use github_context::GhToRust;
pub use github_context::RustToGh;

use self::steps::ado::AdoRuntimeVar;
use self::steps::ado::AdoStepServices;
use self::steps::github::GhStepBuilder;
use self::steps::rust::RustRuntimeServices;
use self::user_facing::ClaimedGhParam;
use self::user_facing::GhPermission;
use self::user_facing::GhPermissionValue;
use crate::node::github_context::GhContextVarReader;
use github_context::state::Root;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use user_facing::GhParam;

/// Node types which are considered "user facing", and re-exported in the
/// `flowey` crate.
pub mod user_facing {
    pub use super::ClaimVar;
    pub use super::ClaimedReadVar;
    pub use super::ClaimedWriteVar;
    pub use super::FlowArch;
    pub use super::FlowBackend;
    pub use super::FlowNode;
    pub use super::FlowPlatform;
    pub use super::FlowPlatformKind;
    pub use super::GhUserSecretVar;
    pub use super::ImportCtx;
    pub use super::IntoRequest;
    pub use super::NodeCtx;
    pub use super::ReadVar;
    pub use super::SideEffect;
    pub use super::SimpleFlowNode;
    pub use super::StepCtx;
    pub use super::VarClaimed;
    pub use super::VarEqBacking;
    pub use super::VarNotClaimed;
    pub use super::WriteVar;
    pub use super::steps::ado::AdoResourcesRepositoryId;
    pub use super::steps::ado::AdoRuntimeVar;
    pub use super::steps::ado::AdoStepServices;
    pub use super::steps::github::ClaimedGhParam;
    pub use super::steps::github::GhParam;
    pub use super::steps::github::GhPermission;
    pub use super::steps::github::GhPermissionValue;
    pub use super::steps::rust::RustRuntimeServices;
    pub use crate::flowey_request;
    pub use crate::new_flow_node;
    pub use crate::new_simple_flow_node;
    pub use crate::node::FlowPlatformLinuxDistro;
    pub use crate::pipeline::Artifact;

    /// Helper method to streamline request validation in cases where a value is
    /// expected to be identical across all incoming requests.
    ///
    /// # Example: Request Aggregation Pattern
    ///
    /// When a node receives multiple requests, it often needs to ensure certain
    /// values are consistent across all requests. This helper simplifies that pattern:
    ///
    /// ```rust,ignore
    /// fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
    ///     let mut version = None;
    ///     let mut ensure_installed = Vec::new();
    ///
    ///     for req in requests {
    ///         match req {
    ///             Request::Version(v) => {
    ///                 // Ensure all requests agree on the version
    ///                 same_across_all_reqs("Version", &mut version, v)?;
    ///             }
    ///             Request::EnsureInstalled(v) => {
    ///                 ensure_installed.push(v);
    ///             }
    ///         }
    ///     }
    ///
    ///     let version = version.ok_or(anyhow::anyhow!("Missing required request: Version"))?;
    ///
    ///     // ... emit steps using aggregated requests
    ///     Ok(())
    /// }
    /// ```
    pub fn same_across_all_reqs<T: PartialEq>(
        req_name: &str,
        var: &mut Option<T>,
        new: T,
    ) -> anyhow::Result<()> {
        match (var.as_ref(), new) {
            (None, v) => *var = Some(v),
            (Some(old), new) => {
                if *old != new {
                    anyhow::bail!("`{}` must be consistent across requests", req_name);
                }
            }
        }

        Ok(())
    }

    /// Helper method to streamline request validation in cases where a value is
    /// expected to be identical across all incoming requests, using a custom
    /// comparison function.
    pub fn same_across_all_reqs_backing_var<V: VarEqBacking>(
        req_name: &str,
        var: &mut Option<V>,
        new: V,
    ) -> anyhow::Result<()> {
        match (var.as_ref(), new) {
            (None, v) => *var = Some(v),
            (Some(old), new) => {
                if !old.eq(&new) {
                    anyhow::bail!("`{}` must be consistent across requests", req_name);
                }
            }
        }

        Ok(())
    }

    /// Helper method to handle Linux distros that are supported only on one
    /// host architecture.
    /// match_arch!(var, arch, result)
    #[macro_export]
    macro_rules! match_arch {
        ($host_arch:expr, $match_arch:pat, $expr:expr) => {
            if matches!($host_arch, $match_arch) {
                $expr
            } else {
                anyhow::bail!("Linux distro not supported on host arch {}", $host_arch);
            }
        };
    }
}

/// Check if `ReadVar` / `WriteVar` instances are backed by the same underlying
/// flowey Var.
///
/// # Why not use `Eq`? Why have a whole separate trait?
///
/// `ReadVar` and `WriteVar` are, in some sense, flowey's analog to
/// "pointers", insofar as these types primary purpose is to mediate access to
/// some contained value, as opposed to being "values" themselves.
///
/// Assuming you agree with this analogy, then we can apply the same logic to
/// `ReadVar` and `WriteVar` as Rust does to `Box<T>` wrt. what the `Eq`
/// implementation should mean.
///
/// Namely: `Eq` should check the equality of the _contained objects_, as
/// opposed to the pointers themselves.
///
/// Unfortunately, unlike `Box<T>`, it is _impossible_ to have an `Eq` impl for
/// `ReadVar` / `WriteVar` that checks contents for equality, due to the fact
/// that these types exist at flow resolution time, whereas the values they
/// contain only exist at flow runtime.
///
/// As such, we have a separate trait to perform different kinds of equality
/// checks on Vars.
pub trait VarEqBacking {
    /// Check if `self` is backed by the same variable as `other`.
    fn eq(&self, other: &Self) -> bool;
}

impl<T> VarEqBacking for WriteVar<T>
where
    T: Serialize + DeserializeOwned,
{
    fn eq(&self, other: &Self) -> bool {
        self.backing_var == other.backing_var
    }
}

impl<T> VarEqBacking for ReadVar<T>
where
    T: Serialize + DeserializeOwned + PartialEq + Eq + Clone,
{
    fn eq(&self, other: &Self) -> bool {
        self.backing_var == other.backing_var
    }
}

// TODO: this should be generic across all tuple sizes
impl<T, U> VarEqBacking for (T, U)
where
    T: VarEqBacking,
    U: VarEqBacking,
{
    fn eq(&self, other: &Self) -> bool {
        (self.0.eq(&other.0)) && (self.1.eq(&other.1))
    }
}

/// Type corresponding to a step which performs a side-effect,
/// without returning a specific value.
///
/// e.g: A step responsible for installing a package from `apt` might claim a
/// `WriteVar<SideEffect>`, with any step requiring the package to have been
/// installed prior being able to claim the corresponding `ReadVar<SideEffect>.`
pub type SideEffect = ();

/// Uninhabited type used to denote that a particular [`WriteVar`] / [`ReadVar`]
/// is not currently claimed by any step, and cannot be directly accessed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VarNotClaimed {}

/// Uninhabited type used to denote that a particular [`WriteVar`] / [`ReadVar`]
/// is currently claimed by a step, and can be read/written to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VarClaimed {}

/// Write a value into a flowey Var at runtime, which can then be read via a
/// corresponding [`ReadVar`].
///
/// Vars in flowey must be serde de/serializable, in order to be de/serialized
/// between multiple steps/nodes.
///
/// In order to write a value into a `WriteVar`, it must first be _claimed_ by a
/// particular step (using the [`ClaimVar::claim`] API). Once claimed, the Var
/// can be written to using APIs such as [`RustRuntimeServices::write`], or
/// [`AdoStepServices::set_var`]
///
/// Note that it is only possible to write a value into a `WriteVar` _once_.
/// Once the value has been written, the `WriteVar` type is immediately
/// consumed, making it impossible to overwrite the stored value at some later
/// point in execution.
///
/// This "write-once" property is foundational to flowey's execution model, as
/// by recoding what step wrote to a Var, and what step(s) read from the Var, it
/// is possible to infer what order steps must be run in.
#[derive(Debug, Serialize, Deserialize)]
pub struct WriteVar<T: Serialize + DeserializeOwned, C = VarNotClaimed> {
    backing_var: String,
    /// If true, then readers on this var expect to read a side effect (`()`)
    /// and not `T`.
    is_side_effect: bool,

    #[serde(skip)]
    _kind: core::marker::PhantomData<(T, C)>,
}

/// A [`WriteVar`] which has been claimed by a particular step, allowing it
/// to be written to at runtime.
pub type ClaimedWriteVar<T> = WriteVar<T, VarClaimed>;

impl<T: Serialize + DeserializeOwned> WriteVar<T, VarNotClaimed> {
    /// (Internal API) Switch the claim marker to "claimed".
    fn into_claimed(self) -> WriteVar<T, VarClaimed> {
        let Self {
            backing_var,
            is_side_effect,
            _kind,
        } = self;

        WriteVar {
            backing_var,
            is_side_effect,
            _kind: std::marker::PhantomData,
        }
    }

    /// Write a static value into the Var.
    #[track_caller]
    pub fn write_static(self, ctx: &mut NodeCtx<'_>, val: T)
    where
        T: 'static,
    {
        let val = ReadVar::from_static(val);
        val.write_into(ctx, self, |v| v);
    }

    pub(crate) fn into_json(self) -> WriteVar<serde_json::Value> {
        WriteVar {
            backing_var: self.backing_var,
            is_side_effect: self.is_side_effect,
            _kind: std::marker::PhantomData,
        }
    }
}

impl WriteVar<SideEffect, VarNotClaimed> {
    /// Transforms this writer into one that can be used to write a `T`.
    ///
    /// This is useful when a reader only cares about the side effect of an
    /// operation, but the writer wants to provide output as well.
    pub fn discard_result<T: Serialize + DeserializeOwned>(self) -> WriteVar<T> {
        WriteVar {
            backing_var: self.backing_var,
            is_side_effect: true,
            _kind: std::marker::PhantomData,
        }
    }
}

/// Claim one or more flowey Vars for a particular step.
///
/// By having this be a trait, it is possible to `claim` both single instances
/// of `ReadVar` / `WriteVar`, as well as whole _collections_ of Vars.
//
// FUTURE: flowey should include a derive macro for easily claiming read/write
// vars in user-defined structs / enums.
pub trait ClaimVar {
    /// The claimed version of Self.
    type Claimed;
    /// Claim the Var for this step, allowing it to be accessed at runtime.
    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed;
}

/// Read the value of one or more flowey Vars.
///
/// By having this be a trait, it is possible to `read` both single
/// instances of `ReadVar` / `WriteVar`, as well as whole _collections_ of
/// Vars.
pub trait ReadVarValue {
    /// The read value of Self.
    type Value;
    /// Read the value of the Var at runtime.
    fn read_value(self, rt: &mut RustRuntimeServices<'_>) -> Self::Value;
}

impl<T: Serialize + DeserializeOwned> ClaimVar for ReadVar<T> {
    type Claimed = ClaimedReadVar<T>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> ClaimedReadVar<T> {
        if let ReadVarBacking::RuntimeVar {
            var,
            is_side_effect: _,
        } = &self.backing_var
        {
            ctx.backend.borrow_mut().on_claimed_runtime_var(var, true);
        }
        self.into_claimed()
    }
}

impl<T: Serialize + DeserializeOwned> ClaimVar for WriteVar<T> {
    type Claimed = ClaimedWriteVar<T>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> ClaimedWriteVar<T> {
        ctx.backend
            .borrow_mut()
            .on_claimed_runtime_var(&self.backing_var, false);
        self.into_claimed()
    }
}

impl<T: Serialize + DeserializeOwned> ReadVarValue for ClaimedReadVar<T> {
    type Value = T;

    fn read_value(self, rt: &mut RustRuntimeServices<'_>) -> Self::Value {
        match self.backing_var {
            ReadVarBacking::RuntimeVar {
                var,
                is_side_effect,
            } => {
                // Always get the data to validate that the variable is actually there.
                let data = rt.get_var(&var, is_side_effect);
                if is_side_effect {
                    // This was converted into a `ReadVar<SideEffect>` from
                    // another type, so parse the value that a
                    // `WriteVar<SideEffect>` would have written.
                    serde_json::from_slice(b"null").expect("should be deserializing into ()")
                } else {
                    // This is a normal variable.
                    serde_json::from_slice(&data).expect("improve this error path")
                }
            }
            ReadVarBacking::Inline(val) => val,
        }
    }
}

impl<T: ClaimVar> ClaimVar for Vec<T> {
    type Claimed = Vec<T::Claimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Vec<T::Claimed> {
        self.into_iter().map(|v| v.claim(ctx)).collect()
    }
}

impl<T: ReadVarValue> ReadVarValue for Vec<T> {
    type Value = Vec<T::Value>;

    fn read_value(self, rt: &mut RustRuntimeServices<'_>) -> Self::Value {
        self.into_iter().map(|v| v.read_value(rt)).collect()
    }
}

impl<T: ClaimVar> ClaimVar for Option<T> {
    type Claimed = Option<T::Claimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Option<T::Claimed> {
        self.map(|x| x.claim(ctx))
    }
}

impl<T: ReadVarValue> ReadVarValue for Option<T> {
    type Value = Option<T::Value>;

    fn read_value(self, rt: &mut RustRuntimeServices<'_>) -> Self::Value {
        self.map(|x| x.read_value(rt))
    }
}

impl<U: Ord, T: ClaimVar> ClaimVar for BTreeMap<U, T> {
    type Claimed = BTreeMap<U, T::Claimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> BTreeMap<U, T::Claimed> {
        self.into_iter().map(|(k, v)| (k, v.claim(ctx))).collect()
    }
}

impl<U: Ord, T: ReadVarValue> ReadVarValue for BTreeMap<U, T> {
    type Value = BTreeMap<U, T::Value>;

    fn read_value(self, rt: &mut RustRuntimeServices<'_>) -> Self::Value {
        self.into_iter()
            .map(|(k, v)| (k, v.read_value(rt)))
            .collect()
    }
}

macro_rules! impl_tuple_claim {
    ($($T:tt)*) => {
        impl<$($T,)*> $crate::node::ClaimVar for ($($T,)*)
        where
            $($T: $crate::node::ClaimVar,)*
        {
            type Claimed = ($($T::Claimed,)*);

            #[expect(non_snake_case)]
            fn claim(self, ctx: &mut $crate::node::StepCtx<'_>) -> Self::Claimed {
                let ($($T,)*) = self;
                ($($T.claim(ctx),)*)
            }
        }

        impl<$($T,)*> $crate::node::ReadVarValue for ($($T,)*)
        where
            $($T: $crate::node::ReadVarValue,)*
        {
            type Value = ($($T::Value,)*);

            #[expect(non_snake_case)]
            fn read_value(self, rt: &mut $crate::node::RustRuntimeServices<'_>) -> Self::Value {
                let ($($T,)*) = self;
                ($($T.read_value(rt),)*)
            }
        }
    };
}

impl_tuple_claim!(A B C D E F G H I J);
impl_tuple_claim!(A B C D E F G H I);
impl_tuple_claim!(A B C D E F G H);
impl_tuple_claim!(A B C D E F G);
impl_tuple_claim!(A B C D E F);
impl_tuple_claim!(A B C D E);
impl_tuple_claim!(A B C D);
impl_tuple_claim!(A B C);
impl_tuple_claim!(A B);
impl_tuple_claim!(A);

impl ClaimVar for () {
    type Claimed = ();

    fn claim(self, _ctx: &mut StepCtx<'_>) -> Self::Claimed {}
}

impl ReadVarValue for () {
    type Value = ();

    fn read_value(self, _rt: &mut RustRuntimeServices<'_>) -> Self::Value {}
}

/// Read a custom, user-defined secret by passing in the secret name.
///
/// Intended usage is to get a secret using the [`crate::pipeline::Pipeline::gh_use_secret`] API
/// and to use the returned value through the [`NodeCtx::get_gh_context_var`] API.
#[derive(Serialize, Deserialize, Clone)]
pub struct GhUserSecretVar(pub(crate) String);

/// Read a value from a flowey Var at runtime, returning the value written by
/// the Var's corresponding [`WriteVar`].
///
/// Vars in flowey must be serde de/serializable, in order to be de/serialized
/// between multiple steps/nodes.
///
/// In order to read the value contained within a `ReadVar`, it must first be
/// _claimed_ by a particular step (using the [`ClaimVar::claim`] API). Once
/// claimed, the Var can be read using APIs such as
/// [`RustRuntimeServices::read`], or [`AdoStepServices::get_var`]
///
/// Note that all `ReadVar`s in flowey are _immutable_. In other words:
/// reading the value of a `ReadVar` multiple times from multiple nodes will
/// _always_ return the same value.
///
/// This is a natural consequence `ReadVar` obtaining its value from the result
/// of a write into [`WriteVar`], whose API enforces that there can only ever be
/// a single Write to a `WriteVar`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReadVar<T, C = VarNotClaimed> {
    backing_var: ReadVarBacking<T>,
    #[serde(skip)]
    _kind: std::marker::PhantomData<C>,
}

/// A [`ReadVar`] which has been claimed by a particular step, allowing it to
/// be read at runtime.
pub type ClaimedReadVar<T> = ReadVar<T, VarClaimed>;

// cloning is fine, since you can totally have multiple dependents
impl<T: Serialize + DeserializeOwned, C> Clone for ReadVar<T, C> {
    fn clone(&self) -> Self {
        ReadVar {
            backing_var: self.backing_var.clone(),
            _kind: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
enum ReadVarBacking<T> {
    RuntimeVar {
        var: String,
        /// If true, then don't try to parse this variable--it was converted
        /// into a side effect (of type `()`) from another type, so the
        /// serialization will not match.
        ///
        /// If false, it may still be a "side effect" variable, but type `T`
        /// matches its serialization.
        is_side_effect: bool,
    },
    Inline(T),
}

// avoid requiring types to include an explicit clone bound
impl<T: Serialize + DeserializeOwned> Clone for ReadVarBacking<T> {
    fn clone(&self) -> Self {
        match self {
            Self::RuntimeVar {
                var,
                is_side_effect,
            } => Self::RuntimeVar {
                var: var.clone(),
                is_side_effect: *is_side_effect,
            },
            Self::Inline(v) => {
                Self::Inline(serde_json::from_value(serde_json::to_value(v).unwrap()).unwrap())
            }
        }
    }
}

impl<T: Serialize + DeserializeOwned> ReadVar<T> {
    /// (Internal API) Switch the claim marker to "claimed".
    fn into_claimed(self) -> ReadVar<T, VarClaimed> {
        let Self { backing_var, _kind } = self;

        ReadVar {
            backing_var,
            _kind: std::marker::PhantomData,
        }
    }

    /// Discard any type information associated with the Var, and treat the Var
    /// as through it was only a side effect.
    ///
    /// e.g: if a Node returns a `ReadVar<PathBuf>`, but you know that the mere
    /// act of having _run_ the node has ensured the file is placed in a "magic
    /// location" for some other node, then it may be useful to treat the
    /// `ReadVar<PathBuf>` as a simple `ReadVar<SideEffect>`, which can be
    /// passed along as part of a larger bundle of `Vec<ReadVar<SideEffect>>`.
    #[must_use]
    pub fn into_side_effect(self) -> ReadVar<SideEffect> {
        ReadVar {
            backing_var: match self.backing_var {
                ReadVarBacking::RuntimeVar {
                    var,
                    is_side_effect: _,
                } => ReadVarBacking::RuntimeVar {
                    var,
                    is_side_effect: true,
                },
                ReadVarBacking::Inline(_) => ReadVarBacking::Inline(()),
            },
            _kind: std::marker::PhantomData,
        }
    }

    /// Maps a `ReadVar<T>` to a new `ReadVar<U>`, by applying a function to the
    /// Var at runtime.
    #[track_caller]
    #[must_use]
    pub fn map<F, U>(&self, ctx: &mut NodeCtx<'_>, f: F) -> ReadVar<U>
    where
        T: 'static,
        U: Serialize + DeserializeOwned + 'static,
        F: FnOnce(T) -> U + 'static,
    {
        let (read_from, write_into) = ctx.new_var();
        self.write_into(ctx, write_into, f);
        read_from
    }

    /// Maps a `ReadVar<T>` into an existing `WriteVar<U>` by applying a
    /// function to the Var at runtime.
    #[track_caller]
    pub fn write_into<F, U>(&self, ctx: &mut NodeCtx<'_>, write_into: WriteVar<U>, f: F)
    where
        T: 'static,
        U: Serialize + DeserializeOwned + 'static,
        F: FnOnce(T) -> U + 'static,
    {
        let this = self.clone();
        ctx.emit_minor_rust_step("🌼 write_into Var", move |ctx| {
            let this = this.claim(ctx);
            let write_into = write_into.claim(ctx);
            move |rt| {
                let this = rt.read(this);
                rt.write(write_into, &f(this));
            }
        });
    }

    /// Zips self (`ReadVar<T>`) with another `ReadVar<U>`, returning a new
    /// `ReadVar<(T, U)>`
    #[track_caller]
    #[must_use]
    pub fn zip<U>(&self, ctx: &mut NodeCtx<'_>, other: ReadVar<U>) -> ReadVar<(T, U)>
    where
        T: 'static,
        U: Serialize + DeserializeOwned + 'static,
    {
        let (read_from, write_into) = ctx.new_var();
        let this = self.clone();
        ctx.emit_minor_rust_step("🌼 Zip Vars", move |ctx| {
            let this = this.claim(ctx);
            let other = other.claim(ctx);
            let write_into = write_into.claim(ctx);
            move |rt| {
                let this = rt.read(this);
                let other = rt.read(other);
                rt.write(write_into, &(this, other));
            }
        });
        read_from
    }

    /// Create a new `ReadVar` from a static value.
    ///
    /// **WARNING:** Static values **CANNOT BE SECRETS**, as they are encoded as
    /// plain-text in the output flow.
    #[track_caller]
    #[must_use]
    pub fn from_static(val: T) -> ReadVar<T>
    where
        T: 'static,
    {
        ReadVar {
            backing_var: ReadVarBacking::Inline(val),
            _kind: std::marker::PhantomData,
        }
    }

    /// If this [`ReadVar`] contains a static value, return it.
    ///
    /// Nodes can opt-in to using this method as a way to generate optimized
    /// steps in cases where the value of a variable is known ahead of time.
    ///
    /// e.g: a node doing a git checkout could leverage this method to decide
    /// whether its ADO backend should emit a conditional step for checking out
    /// a repo, or if it can statically include / exclude the checkout request.
    pub fn get_static(&self) -> Option<T> {
        match self.clone().backing_var {
            ReadVarBacking::Inline(v) => Some(v),
            _ => None,
        }
    }

    /// Transpose a `Vec<ReadVar<T>>` into a `ReadVar<Vec<T>>`
    #[track_caller]
    #[must_use]
    pub fn transpose_vec(ctx: &mut NodeCtx<'_>, vec: Vec<ReadVar<T>>) -> ReadVar<Vec<T>>
    where
        T: 'static,
    {
        let (read_from, write_into) = ctx.new_var();
        ctx.emit_minor_rust_step("🌼 Transpose Vec<ReadVar<T>>", move |ctx| {
            let vec = vec.claim(ctx);
            let write_into = write_into.claim(ctx);
            move |rt| {
                let mut v = Vec::new();
                for var in vec {
                    v.push(rt.read(var));
                }
                rt.write(write_into, &v);
            }
        });
        read_from
    }

    /// Returns a new instance of this variable with an artificial dependency on
    /// `other`.
    ///
    /// This is useful for making explicit a non-explicit dependency between the
    /// two variables. For example, if `self` contains a path to a file, and
    /// `other` is only written once that file has been created, then this
    /// method can be used to return a new `ReadVar` which depends on `other`
    /// but is otherwise identical to `self`. This ensures that when the new
    /// variable is read, the file has been created.
    ///
    /// In general, it is better to ensure that the dependency is explicit, so
    /// that if you have a variable with a path, then you know that the file
    /// exists when you read it. This method is useful in cases where this is
    /// not naturally the case, e.g., when you are providing a path as part of a
    /// request, as opposed to the path being returned to you.
    #[must_use]
    pub fn depending_on<U>(&self, ctx: &mut NodeCtx<'_>, other: &ReadVar<U>) -> Self
    where
        T: 'static,
        U: Serialize + DeserializeOwned + 'static,
    {
        // This could probably be handled without an additional Rust step with some
        // additional work in the backend, but this is simple enough for now.
        ctx.emit_minor_rust_stepv("🌼 Add dependency", |ctx| {
            let this = self.clone().claim(ctx);
            other.clone().claim(ctx);
            move |rt| rt.read(this)
        })
    }

    /// Consume this `ReadVar` outside the context of a step, signalling that it
    /// won't be used.
    pub fn claim_unused(self, ctx: &mut NodeCtx<'_>) {
        match self.backing_var {
            ReadVarBacking::RuntimeVar {
                var,
                is_side_effect: _,
            } => ctx.backend.borrow_mut().on_unused_read_var(&var),
            ReadVarBacking::Inline(_) => {}
        }
    }

    pub(crate) fn into_json(self) -> ReadVar<serde_json::Value> {
        match self.backing_var {
            ReadVarBacking::RuntimeVar {
                var,
                is_side_effect,
            } => ReadVar {
                backing_var: ReadVarBacking::RuntimeVar {
                    var,
                    is_side_effect,
                },
                _kind: std::marker::PhantomData,
            },
            ReadVarBacking::Inline(v) => ReadVar {
                backing_var: ReadVarBacking::Inline(serde_json::to_value(v).unwrap()),
                _kind: std::marker::PhantomData,
            },
        }
    }
}

/// DANGER: obtain a handle to a [`ReadVar`] "out of thin air".
///
/// This should NEVER be used from within a flowey node. This is a sharp tool,
/// and should only be used by code implementing flow / pipeline resolution
/// logic.
#[must_use]
pub fn thin_air_read_runtime_var<T>(backing_var: String) -> ReadVar<T>
where
    T: Serialize + DeserializeOwned,
{
    ReadVar {
        backing_var: ReadVarBacking::RuntimeVar {
            var: backing_var,
            is_side_effect: false,
        },
        _kind: std::marker::PhantomData,
    }
}

/// DANGER: obtain a handle to a [`WriteVar`] "out of thin air".
///
/// This should NEVER be used from within a flowey node. This is a sharp tool,
/// and should only be used by code implementing flow / pipeline resolution
/// logic.
#[must_use]
pub fn thin_air_write_runtime_var<T>(backing_var: String) -> WriteVar<T>
where
    T: Serialize + DeserializeOwned,
{
    WriteVar {
        backing_var,
        is_side_effect: false,
        _kind: std::marker::PhantomData,
    }
}

/// DANGER: obtain a [`ReadVar`] backing variable and side effect status.
///
/// This should NEVER be used from within a flowey node. This relies on
/// flowey variable implementation details, and should only be used by code
/// implementing flow / pipeline resolution logic.
pub fn read_var_internals<T: Serialize + DeserializeOwned, C>(
    var: &ReadVar<T, C>,
) -> (Option<String>, bool) {
    match var.backing_var {
        ReadVarBacking::RuntimeVar {
            var: ref s,
            is_side_effect,
        } => (Some(s.clone()), is_side_effect),
        ReadVarBacking::Inline(_) => (None, false),
    }
}

pub trait ImportCtxBackend {
    fn on_possible_dep(&mut self, node_handle: NodeHandle);
}

/// Context passed to [`FlowNode::imports`].
pub struct ImportCtx<'a> {
    backend: &'a mut dyn ImportCtxBackend,
}

impl ImportCtx<'_> {
    /// Declare that a Node can be referenced in [`FlowNode::emit`]
    pub fn import<N: FlowNodeBase + 'static>(&mut self) {
        self.backend.on_possible_dep(NodeHandle::from_type::<N>())
    }
}

pub fn new_import_ctx(backend: &mut dyn ImportCtxBackend) -> ImportCtx<'_> {
    ImportCtx { backend }
}

#[derive(Debug)]
pub enum CtxAnchor {
    PostJob,
}

pub trait NodeCtxBackend {
    /// Handle to the current node this `ctx` corresponds to
    fn current_node(&self) -> NodeHandle;

    /// Return a string which uniquely identifies this particular Var
    /// registration.
    ///
    /// Typically consists of `{current node handle}{ordinal}`
    fn on_new_var(&mut self) -> String;

    /// Invoked when a node claims a particular runtime variable
    fn on_claimed_runtime_var(&mut self, var: &str, is_read: bool);

    /// Invoked when a node marks a particular runtime variable as unused
    fn on_unused_read_var(&mut self, var: &str);

    /// Invoked when a node sets a request on a node.
    ///
    /// - `node_typeid` will always correspond to a node that was previously
    ///   passed to `on_register`.
    /// - `req` may be an error, in the case where the NodeCtx failed to
    ///   serialize the provided request.
    // FIXME: this should be using type-erased serde
    fn on_request(&mut self, node_handle: NodeHandle, req: anyhow::Result<Box<[u8]>>);

    fn on_emit_rust_step(
        &mut self,
        label: &str,
        can_merge: bool,
        code: Box<dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()>>,
    );

    fn on_emit_ado_step(
        &mut self,
        label: &str,
        yaml_snippet: Box<dyn for<'a> FnOnce(&'a mut AdoStepServices<'_>) -> String>,
        inline_script: Option<
            Box<dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()>>,
        >,
        condvar: Option<String>,
    );

    fn on_emit_gh_step(
        &mut self,
        label: &str,
        uses: &str,
        with: BTreeMap<String, ClaimedGhParam>,
        condvar: Option<String>,
        outputs: BTreeMap<String, Vec<GhOutput>>,
        permissions: BTreeMap<GhPermission, GhPermissionValue>,
        gh_to_rust: Vec<GhToRust>,
        rust_to_gh: Vec<RustToGh>,
    );

    fn on_emit_side_effect_step(&mut self);

    fn backend(&mut self) -> FlowBackend;
    fn platform(&mut self) -> FlowPlatform;
    fn arch(&mut self) -> FlowArch;

    /// Return a node-specific persistent store path. The backend does not need
    /// to ensure that the path exists - flowey will automatically emit a step
    /// to construct the directory at runtime.
    fn persistent_dir_path_var(&mut self) -> Option<String>;
}

pub fn new_node_ctx(backend: &mut dyn NodeCtxBackend) -> NodeCtx<'_> {
    NodeCtx {
        backend: Rc::new(RefCell::new(backend)),
    }
}

/// What backend the flow is being running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FlowBackend {
    /// Running locally.
    Local,
    /// Running on ADO.
    Ado,
    /// Running on GitHub Actions
    Github,
}

/// The kind platform the flow is being running on, Windows or Unix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FlowPlatformKind {
    Windows,
    Unix,
}

/// The kind platform the flow is being running on, Windows or Unix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FlowPlatformLinuxDistro {
    /// Fedora (including WSL2)
    Fedora,
    /// Ubuntu (including WSL2)
    Ubuntu,
    /// Arch Linux (including WSL2)
    Arch,
    /// Nix environment (detected via IN_NIX_SHELL env var or having a `/nix/store` in PATH)
    Nix,
    /// An unknown distribution
    Unknown,
}

/// What platform the flow is being running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FlowPlatform {
    /// Windows
    Windows,
    /// Linux (including WSL2)
    Linux(FlowPlatformLinuxDistro),
    /// macOS
    MacOs,
}

impl FlowPlatform {
    pub fn kind(&self) -> FlowPlatformKind {
        match self {
            Self::Windows => FlowPlatformKind::Windows,
            Self::Linux(_) | Self::MacOs => FlowPlatformKind::Unix,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux(_) => "linux",
            Self::MacOs => "macos",
        }
    }

    /// The suffix to use for executables on this platform.
    pub fn exe_suffix(&self) -> &'static str {
        if self == &Self::Windows { ".exe" } else { "" }
    }

    /// The full name for a binary on this platform (i.e. `name + self.exe_suffix()`).
    pub fn binary(&self, name: &str) -> String {
        format!("{}{}", name, self.exe_suffix())
    }
}

impl std::fmt::Display for FlowPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// What architecture the flow is being running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FlowArch {
    X86_64,
    Aarch64,
}

impl FlowArch {
    fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

impl std::fmt::Display for FlowArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// Context object for an individual step.
pub struct StepCtx<'a> {
    backend: Rc<RefCell<&'a mut dyn NodeCtxBackend>>,
}

impl StepCtx<'_> {
    /// What backend the flow is being running on (e.g: locally, ADO, GitHub,
    /// etc...)
    pub fn backend(&self) -> FlowBackend {
        self.backend.borrow_mut().backend()
    }

    /// What platform the flow is being running on (e.g: windows, linux, wsl2,
    /// etc...).
    pub fn platform(&self) -> FlowPlatform {
        self.backend.borrow_mut().platform()
    }
}

const NO_ADO_INLINE_SCRIPT: Option<
    for<'a> fn(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()>,
> = None;

/// Context object for a `FlowNode`.
pub struct NodeCtx<'a> {
    backend: Rc<RefCell<&'a mut dyn NodeCtxBackend>>,
}

impl<'ctx> NodeCtx<'ctx> {
    /// Emit a Rust-based step.
    ///
    /// As a convenience feature, this function returns a special _optional_
    /// [`ReadVar<SideEffect>`], which will not result in a "unused variable"
    /// error if no subsequent step ends up claiming it.
    pub fn emit_rust_step<F, G>(&mut self, label: impl AsRef<str>, code: F) -> ReadVar<SideEffect>
    where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
    {
        self.emit_rust_step_inner(label.as_ref(), false, code)
    }

    /// Emit a Rust-based step that cannot fail.
    ///
    /// This is equivalent to `emit_rust_step`, but it is for steps that cannot
    /// fail and that do not need to be emitted as a separate step in a YAML
    /// pipeline. This simplifies the pipeline logs.
    ///
    /// As a convenience feature, this function returns a special _optional_
    /// [`ReadVar<SideEffect>`], which will not result in a "unused variable"
    /// error if no subsequent step ends up claiming it.
    pub fn emit_minor_rust_step<F, G>(
        &mut self,
        label: impl AsRef<str>,
        code: F,
    ) -> ReadVar<SideEffect>
    where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) + 'static,
    {
        self.emit_rust_step_inner(label.as_ref(), true, |ctx| {
            let f = code(ctx);
            |rt| {
                f(rt);
                Ok(())
            }
        })
    }

    /// Emit a Rust-based step, creating a new `ReadVar<T>` from the step's
    /// return value.
    ///
    /// This is a convenience function that streamlines the following common
    /// flowey pattern:
    ///
    /// ```ignore
    /// // creating a new Var explicitly
    /// let (read_foo, write_foo) = ctx.new_var();
    /// ctx.emit_rust_step("foo", |ctx| {
    ///     let write_foo = write_foo.claim(ctx);
    ///     |rt| {
    ///         rt.write(write_foo, &get_foo());
    ///         Ok(())
    ///     }
    /// });
    ///
    /// // creating a new Var automatically
    /// let read_foo = ctx.emit_rust_stepv("foo", |ctx| |rt| Ok(get_foo()));
    /// ```
    #[must_use]
    #[track_caller]
    pub fn emit_rust_stepv<T, F, G>(&mut self, label: impl AsRef<str>, code: F) -> ReadVar<T>
    where
        T: Serialize + DeserializeOwned + 'static,
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<T> + 'static,
    {
        self.emit_rust_stepv_inner(label.as_ref(), false, code)
    }

    /// Emit a Rust-based step, creating a new `ReadVar<T>` from the step's
    /// return value.
    ///
    /// This is equivalent to `emit_rust_stepv`, but it is for steps that cannot
    /// fail and that do not need to be emitted as a separate step in a YAML
    /// pipeline. This simplifies the pipeline logs.
    ///
    /// This is a convenience function that streamlines the following common
    /// flowey pattern:
    ///
    /// ```ignore
    /// // creating a new Var explicitly
    /// let (read_foo, write_foo) = ctx.new_var();
    /// ctx.emit_minor_rust_step("foo", |ctx| {
    ///     let write_foo = write_foo.claim(ctx);
    ///     |rt| {
    ///         rt.write(write_foo, &get_foo());
    ///     }
    /// });
    ///
    /// // creating a new Var automatically
    /// let read_foo = ctx.emit_minor_rust_stepv("foo", |ctx| |rt| get_foo());
    /// ```
    #[must_use]
    #[track_caller]
    pub fn emit_minor_rust_stepv<T, F, G>(&mut self, label: impl AsRef<str>, code: F) -> ReadVar<T>
    where
        T: Serialize + DeserializeOwned + 'static,
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> T + 'static,
    {
        self.emit_rust_stepv_inner(label.as_ref(), true, |ctx| {
            let f = code(ctx);
            |rt| Ok(f(rt))
        })
    }

    fn emit_rust_step_inner<F, G>(
        &mut self,
        label: &str,
        can_merge: bool,
        code: F,
    ) -> ReadVar<SideEffect>
    where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
    {
        let (read, write) = self.new_prefixed_var("auto_se");

        let ctx = &mut StepCtx {
            backend: self.backend.clone(),
        };
        write.claim(ctx);

        let code = code(ctx);
        self.backend
            .borrow_mut()
            .on_emit_rust_step(label.as_ref(), can_merge, Box::new(code));
        read
    }

    #[must_use]
    #[track_caller]
    fn emit_rust_stepv_inner<T, F, G>(
        &mut self,
        label: impl AsRef<str>,
        can_merge: bool,
        code: F,
    ) -> ReadVar<T>
    where
        T: Serialize + DeserializeOwned + 'static,
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<T> + 'static,
    {
        let (read, write) = self.new_var();

        let ctx = &mut StepCtx {
            backend: self.backend.clone(),
        };
        let write = write.claim(ctx);

        let code = code(ctx);
        self.backend.borrow_mut().on_emit_rust_step(
            label.as_ref(),
            can_merge,
            Box::new(|rt| {
                let val = code(rt)?;
                rt.write(write, &val);
                Ok(())
            }),
        );
        read
    }

    /// Load an ADO global runtime variable into a flowey [`ReadVar`].
    #[track_caller]
    #[must_use]
    pub fn get_ado_variable(&mut self, ado_var: AdoRuntimeVar) -> ReadVar<String> {
        let (var, write_var) = self.new_var();
        self.emit_ado_step(format!("🌼 read {}", ado_var.as_raw_var_name()), |ctx| {
            let write_var = write_var.claim(ctx);
            |rt| {
                rt.set_var(write_var, ado_var);
                "".into()
            }
        });
        var
    }

    /// Emit an ADO step.
    pub fn emit_ado_step<F, G>(&mut self, display_name: impl AsRef<str>, yaml_snippet: F)
    where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut AdoStepServices<'_>) -> String + 'static,
    {
        self.emit_ado_step_inner(display_name, None, |ctx| {
            (yaml_snippet(ctx), NO_ADO_INLINE_SCRIPT)
        })
    }

    /// Emit an ADO step, conditionally executed based on the value of `cond` at
    /// runtime.
    pub fn emit_ado_step_with_condition<F, G>(
        &mut self,
        display_name: impl AsRef<str>,
        cond: ReadVar<bool>,
        yaml_snippet: F,
    ) where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut AdoStepServices<'_>) -> String + 'static,
    {
        self.emit_ado_step_inner(display_name, Some(cond), |ctx| {
            (yaml_snippet(ctx), NO_ADO_INLINE_SCRIPT)
        })
    }

    /// Emit an ADO step, conditionally executed based on the value of`cond` at
    /// runtime.
    pub fn emit_ado_step_with_condition_optional<F, G>(
        &mut self,
        display_name: impl AsRef<str>,
        cond: Option<ReadVar<bool>>,
        yaml_snippet: F,
    ) where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> G,
        G: for<'a> FnOnce(&'a mut AdoStepServices<'_>) -> String + 'static,
    {
        self.emit_ado_step_inner(display_name, cond, |ctx| {
            (yaml_snippet(ctx), NO_ADO_INLINE_SCRIPT)
        })
    }

    /// Emit an ADO step which invokes a rust callback using an inline script.
    ///
    /// By using the `{{FLOWEY_INLINE_SCRIPT}}` template in the returned yaml
    /// snippet, flowey will interpolate a command ~roughly akin to `flowey
    /// exec-snippet <rust-snippet-id>` into the generated yaml.
    ///
    /// e.g: if we wanted to _manually_ wrap the bash ADO snippet for whatever
    /// reason:
    ///
    /// ```text
    /// - bash: |
    ///     echo "hello there!"
    ///     {{FLOWEY_INLINE_SCRIPT}}
    ///     echo echo "bye!"
    /// ```
    ///
    /// # Limitations
    ///
    /// At the moment, due to flowey API limitations, it is only possible to
    /// embed a single inline script into a YAML step.
    ///
    /// In the future, rather than having separate methods for "emit step with X
    /// inline scripts", flowey should support declaring "first-class" callbacks
    /// via a (hypothetical) `ctx.new_callback_var(|ctx| |rt, input: Input| ->
    /// Output { ... })` API, at which point.
    ///
    /// If such an API were to exist, one could simply use the "vanilla" emit
    /// yaml step functions with these first-class callbacks.
    pub fn emit_ado_step_with_inline_script<F, G, H>(
        &mut self,
        display_name: impl AsRef<str>,
        yaml_snippet: F,
    ) where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> (G, H),
        G: for<'a> FnOnce(&'a mut AdoStepServices<'_>) -> String + 'static,
        H: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
    {
        self.emit_ado_step_inner(display_name, None, |ctx| {
            let (f, g) = yaml_snippet(ctx);
            (f, Some(g))
        })
    }

    fn emit_ado_step_inner<F, G, H>(
        &mut self,
        display_name: impl AsRef<str>,
        cond: Option<ReadVar<bool>>,
        yaml_snippet: F,
    ) where
        F: for<'a> FnOnce(&'a mut StepCtx<'_>) -> (G, Option<H>),
        G: for<'a> FnOnce(&'a mut AdoStepServices<'_>) -> String + 'static,
        H: for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
    {
        let condvar = match cond.map(|c| c.backing_var) {
            // it seems silly to allow this... but it's not hard so why not?
            Some(ReadVarBacking::Inline(cond)) => {
                if !cond {
                    return;
                } else {
                    None
                }
            }
            Some(ReadVarBacking::RuntimeVar {
                var,
                is_side_effect,
            }) => {
                assert!(!is_side_effect);
                self.backend.borrow_mut().on_claimed_runtime_var(&var, true);
                Some(var)
            }
            None => None,
        };

        let (yaml_snippet, inline_script) = yaml_snippet(&mut StepCtx {
            backend: self.backend.clone(),
        });
        self.backend.borrow_mut().on_emit_ado_step(
            display_name.as_ref(),
            Box::new(yaml_snippet),
            if let Some(inline_script) = inline_script {
                Some(Box::new(inline_script))
            } else {
                None
            },
            condvar,
        );
    }

    /// Load a GitHub context variable into a flowey [`ReadVar`].
    #[track_caller]
    #[must_use]
    pub fn get_gh_context_var(&mut self) -> GhContextVarReader<'ctx, Root> {
        GhContextVarReader {
            ctx: NodeCtx {
                backend: self.backend.clone(),
            },
            _state: std::marker::PhantomData,
        }
    }

    /// Emit a GitHub Actions action step.
    pub fn emit_gh_step(
        &mut self,
        display_name: impl AsRef<str>,
        uses: impl AsRef<str>,
    ) -> GhStepBuilder {
        GhStepBuilder::new(display_name, uses)
    }

    fn emit_gh_step_inner(
        &mut self,
        display_name: impl AsRef<str>,
        cond: Option<ReadVar<bool>>,
        uses: impl AsRef<str>,
        with: Option<BTreeMap<String, GhParam>>,
        outputs: BTreeMap<String, Vec<WriteVar<String>>>,
        run_after: Vec<ReadVar<SideEffect>>,
        permissions: BTreeMap<GhPermission, GhPermissionValue>,
    ) {
        let condvar = match cond.map(|c| c.backing_var) {
            // it seems silly to allow this... but it's not hard so why not?
            Some(ReadVarBacking::Inline(cond)) => {
                if !cond {
                    return;
                } else {
                    None
                }
            }
            Some(ReadVarBacking::RuntimeVar {
                var,
                is_side_effect,
            }) => {
                assert!(!is_side_effect);
                self.backend.borrow_mut().on_claimed_runtime_var(&var, true);
                Some(var)
            }
            None => None,
        };

        let with = with
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.claim(&mut StepCtx {
                        backend: self.backend.clone(),
                    }),
                )
            })
            .collect();

        for var in run_after {
            var.claim(&mut StepCtx {
                backend: self.backend.clone(),
            });
        }

        let outputvars = outputs
            .into_iter()
            .map(|(name, vars)| {
                (
                    name,
                    vars.into_iter()
                        .map(|var| {
                            let var = var.claim(&mut StepCtx {
                                backend: self.backend.clone(),
                            });
                            GhOutput {
                                backing_var: var.backing_var,
                                is_secret: false,
                                is_object: false,
                            }
                        })
                        .collect(),
                )
            })
            .collect();

        self.backend.borrow_mut().on_emit_gh_step(
            display_name.as_ref(),
            uses.as_ref(),
            with,
            condvar,
            outputvars,
            permissions,
            Vec::new(),
            Vec::new(),
        );
    }

    /// Emit a "side-effect" step, which simply claims a set of side-effects in
    /// order to resolve another set of side effects.
    ///
    /// The same functionality could be achieved (less efficiently) by emitting
    /// a Rust step (or ADO step, or github step, etc...) that claims both sets
    /// of side-effects, and then does nothing. By using this method - flowey is
    /// able to avoid emitting that additional noop step at runtime.
    pub fn emit_side_effect_step(
        &mut self,
        use_side_effects: impl IntoIterator<Item = ReadVar<SideEffect>>,
        resolve_side_effects: impl IntoIterator<Item = WriteVar<SideEffect>>,
    ) {
        let mut backend = self.backend.borrow_mut();
        for var in use_side_effects.into_iter() {
            if let ReadVarBacking::RuntimeVar {
                var,
                is_side_effect: _,
            } = &var.backing_var
            {
                backend.on_claimed_runtime_var(var, true);
            }
        }

        for var in resolve_side_effects.into_iter() {
            backend.on_claimed_runtime_var(&var.backing_var, false);
        }

        backend.on_emit_side_effect_step();
    }

    /// What backend the flow is being running on (e.g: locally, ADO, GitHub,
    /// etc...)
    pub fn backend(&self) -> FlowBackend {
        self.backend.borrow_mut().backend()
    }

    /// What platform the flow is being running on (e.g: windows, linux, wsl2,
    /// etc...).
    pub fn platform(&self) -> FlowPlatform {
        self.backend.borrow_mut().platform()
    }

    /// What architecture the flow is being running on (x86_64 or Aarch64)
    pub fn arch(&self) -> FlowArch {
        self.backend.borrow_mut().arch()
    }

    /// Set a request on a particular node.
    pub fn req<R>(&mut self, req: R)
    where
        R: IntoRequest + 'static,
    {
        let mut backend = self.backend.borrow_mut();
        backend.on_request(
            NodeHandle::from_type::<R::Node>(),
            serde_json::to_vec(&req.into_request())
                .map(Into::into)
                .map_err(Into::into),
        );
    }

    /// Set a request on a particular node, simultaneously creating a new flowey
    /// Var in the process.
    #[track_caller]
    #[must_use]
    pub fn reqv<T, R>(&mut self, f: impl FnOnce(WriteVar<T>) -> R) -> ReadVar<T>
    where
        T: Serialize + DeserializeOwned,
        R: IntoRequest + 'static,
    {
        let (read, write) = self.new_var();
        self.req::<R>(f(write));
        read
    }

    /// Set multiple requests on a particular node.
    pub fn requests<N>(&mut self, reqs: impl IntoIterator<Item = N::Request>)
    where
        N: FlowNodeBase + 'static,
    {
        let mut backend = self.backend.borrow_mut();
        for req in reqs.into_iter() {
            backend.on_request(
                NodeHandle::from_type::<N>(),
                serde_json::to_vec(&req).map(Into::into).map_err(Into::into),
            );
        }
    }

    /// Allocate a new flowey Var, returning two handles: one for reading the
    /// value, and another for writing the value.
    #[track_caller]
    #[must_use]
    pub fn new_var<T>(&self) -> (ReadVar<T>, WriteVar<T>)
    where
        T: Serialize + DeserializeOwned,
    {
        self.new_prefixed_var("")
    }

    #[track_caller]
    #[must_use]
    fn new_prefixed_var<T>(&self, prefix: &'static str) -> (ReadVar<T>, WriteVar<T>)
    where
        T: Serialize + DeserializeOwned,
    {
        // normalize call path to ensure determinism between windows and linux
        let caller = std::panic::Location::caller()
            .to_string()
            .replace('\\', "/");

        // until we have a proper way to "split" debug info related to vars, we
        // kinda just lump it in with the var name itself.
        //
        // HACK: to work around cases where - depending on what the
        // current-working-dir is when incoking flowey - the returned
        // caller.file() path may leak the full path of the file (as opposed to
        // the relative path), resulting in inconsistencies between build
        // environments.
        //
        // For expediency, and to preserve some semblance of useful error
        // messages, we decided to play some sketchy games with the resulting
        // string to only preserve the _consistent_ bit of the path for a human
        // to use as reference.
        //
        // This is not ideal in the slightest, but it works OK for now
        let caller = caller
            .split_once("flowey/")
            .expect("due to a known limitation with flowey, all flowey code must have an ancestor dir called 'flowey/' somewhere in its full path")
            .1;

        let colon = if prefix.is_empty() { "" } else { ":" };
        let ordinal = self.backend.borrow_mut().on_new_var();
        let backing_var = format!("{prefix}{colon}{ordinal}:{caller}");

        (
            ReadVar {
                backing_var: ReadVarBacking::RuntimeVar {
                    var: backing_var.clone(),
                    is_side_effect: false,
                },
                _kind: std::marker::PhantomData,
            },
            WriteVar {
                backing_var,
                is_side_effect: false,
                _kind: std::marker::PhantomData,
            },
        )
    }

    /// Allocate special [`SideEffect`] var which can be used to schedule a
    /// "post-job" step associated with some existing step.
    ///
    /// This "post-job" step will then only run after all other regular steps
    /// have run (i.e: steps required to complete any top-level objectives
    /// passed in via [`crate::pipeline::PipelineJob::dep_on`]). This makes it
    /// useful for implementing various "cleanup" or "finalize" tasks.
    ///
    /// e.g: the Cache node uses this to upload the contents of a cache
    /// directory at the end of a Job.
    #[track_caller]
    #[must_use]
    pub fn new_post_job_side_effect(&self) -> (ReadVar<SideEffect>, WriteVar<SideEffect>) {
        self.new_prefixed_var("post_job")
    }

    /// Return a flowey Var pointing to a **node-specific** directory which
    /// will be persisted between runs, if such a directory is available.
    ///
    /// WARNING: this method is _very likely_ to return None when running on CI
    /// machines, as most CI agents are wiped between jobs!
    ///
    /// As such, it is NOT recommended that node authors reach for this method
    /// directly, and instead use abstractions such as the
    /// `flowey_lib_common::cache` Node, which implements node-level persistence
    /// in a way that works _regardless_ if a persistent_dir is available (e.g:
    /// by falling back to uploading / downloading artifacts to a "cache store"
    /// on platforms like ADO or Github Actions).
    #[track_caller]
    #[must_use]
    pub fn persistent_dir(&mut self) -> Option<ReadVar<PathBuf>> {
        let path: ReadVar<PathBuf> = ReadVar {
            backing_var: ReadVarBacking::RuntimeVar {
                var: self.backend.borrow_mut().persistent_dir_path_var()?,
                is_side_effect: false,
            },
            _kind: std::marker::PhantomData,
        };

        let folder_name = self
            .backend
            .borrow_mut()
            .current_node()
            .modpath()
            .replace("::", "__");

        Some(
            self.emit_rust_stepv("🌼 Create persistent store dir", |ctx| {
                let path = path.claim(ctx);
                |rt| {
                    let dir = rt.read(path).join(folder_name);
                    fs_err::create_dir_all(&dir)?;
                    Ok(dir)
                }
            }),
        )
    }

    /// Check to see if a persistent dir is available, without yet creating it.
    pub fn supports_persistent_dir(&mut self) -> bool {
        self.backend
            .borrow_mut()
            .persistent_dir_path_var()
            .is_some()
    }
}

// FUTURE: explore using type-erased serde here, instead of relying on
// `serde_json` in `flowey_core`.
pub trait RuntimeVarDb {
    fn get_var(&mut self, var_name: &str) -> (Vec<u8>, bool) {
        self.try_get_var(var_name)
            .unwrap_or_else(|| panic!("db is missing var {}", var_name))
    }

    fn try_get_var(&mut self, var_name: &str) -> Option<(Vec<u8>, bool)>;
    fn set_var(&mut self, var_name: &str, is_secret: bool, value: Vec<u8>);
}

impl RuntimeVarDb for Box<dyn RuntimeVarDb> {
    fn try_get_var(&mut self, var_name: &str) -> Option<(Vec<u8>, bool)> {
        (**self).try_get_var(var_name)
    }

    fn set_var(&mut self, var_name: &str, is_secret: bool, value: Vec<u8>) {
        (**self).set_var(var_name, is_secret, value)
    }
}

pub mod steps {
    pub mod ado {
        use crate::node::ClaimedReadVar;
        use crate::node::ClaimedWriteVar;
        use crate::node::ReadVarBacking;
        use serde::Deserialize;
        use serde::Serialize;
        use std::borrow::Cow;

        /// An ADO repository declared as a resource in the top-level pipeline.
        ///
        /// Created via [`crate::pipeline::Pipeline::ado_add_resources_repository`].
        ///
        /// Consumed via [`AdoStepServices::resolve_repository_id`].
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct AdoResourcesRepositoryId {
            pub(crate) repo_id: String,
        }

        impl AdoResourcesRepositoryId {
            /// Create a `AdoResourcesRepositoryId` corresponding to `self`
            /// (i.e: the repo which stores the current pipeline).
            ///
            /// This is safe to do from any context, as the `self` resource will
            /// _always_ be available.
            pub fn new_self() -> Self {
                Self {
                    repo_id: "self".into(),
                }
            }

            /// (dangerous) get the raw ID associated with this resource.
            ///
            /// It is highly recommended to avoid losing type-safety, and
            /// sticking to [`AdoStepServices::resolve_repository_id`].in order
            /// to resolve this type to a String.
            pub fn dangerous_get_raw_id(&self) -> &str {
                &self.repo_id
            }

            /// (dangerous) create a new ID out of thin air.
            ///
            /// It is highly recommended to avoid losing type-safety, and
            /// sticking to [`AdoStepServices::resolve_repository_id`].in order
            /// to resolve this type to a String.
            pub fn dangerous_new(repo_id: &str) -> Self {
                Self {
                    repo_id: repo_id.into(),
                }
            }
        }

        /// Handle to an ADO variable.
        ///
        /// Includes a (non-exhaustive) list of associated constants
        /// corresponding to global ADO vars which are _always_ available.
        #[derive(Clone, Debug, Serialize, Deserialize)]
        pub struct AdoRuntimeVar {
            is_secret: bool,
            ado_var: Cow<'static, str>,
        }

        #[allow(non_upper_case_globals)]
        impl AdoRuntimeVar {
            /// `build.SourceBranch`
            ///
            /// NOTE: Includes the full branch ref (ex: `refs/heads/main`) so
            /// unlike `build.SourceBranchName`, a branch like `user/foo/bar`
            /// won't be stripped to just `bar`
            pub const BUILD__SOURCE_BRANCH: AdoRuntimeVar =
                AdoRuntimeVar::new("build.SourceBranch");

            /// `build.BuildNumber`
            pub const BUILD__BUILD_NUMBER: AdoRuntimeVar = AdoRuntimeVar::new("build.BuildNumber");

            /// `System.AccessToken`
            pub const SYSTEM__ACCESS_TOKEN: AdoRuntimeVar =
                AdoRuntimeVar::new_secret("System.AccessToken");

            /// `System.System.JobAttempt`
            pub const SYSTEM__JOB_ATTEMPT: AdoRuntimeVar =
                AdoRuntimeVar::new_secret("System.JobAttempt");
        }

        impl AdoRuntimeVar {
            const fn new(s: &'static str) -> Self {
                Self {
                    is_secret: false,
                    ado_var: Cow::Borrowed(s),
                }
            }

            const fn new_secret(s: &'static str) -> Self {
                Self {
                    is_secret: true,
                    ado_var: Cow::Borrowed(s),
                }
            }

            /// Check if the ADO var is tagged as being a secret
            pub fn is_secret(&self) -> bool {
                self.is_secret
            }

            /// Get the raw underlying ADO variable name
            pub fn as_raw_var_name(&self) -> String {
                self.ado_var.as_ref().into()
            }

            /// Get a handle to an ADO runtime variable corresponding to a
            /// global ADO variable with the given name.
            ///
            /// This method should be used rarely and with great care!
            ///
            /// ADO variables are global, and sidestep the type-safe data flow
            /// between flowey nodes entirely!
            pub fn dangerous_from_global(ado_var_name: impl AsRef<str>, is_secret: bool) -> Self {
                Self {
                    is_secret,
                    ado_var: ado_var_name.as_ref().to_owned().into(),
                }
            }
        }

        pub fn new_ado_step_services(
            fresh_ado_var: &mut dyn FnMut() -> String,
        ) -> AdoStepServices<'_> {
            AdoStepServices {
                fresh_ado_var,
                ado_to_rust: Vec::new(),
                rust_to_ado: Vec::new(),
            }
        }

        pub struct CompletedAdoStepServices {
            pub ado_to_rust: Vec<(String, String, bool)>,
            pub rust_to_ado: Vec<(String, String)>,
        }

        impl CompletedAdoStepServices {
            pub fn from_ado_step_services(access: AdoStepServices<'_>) -> Self {
                let AdoStepServices {
                    fresh_ado_var: _,
                    ado_to_rust,
                    rust_to_ado,
                } = access;

                Self {
                    ado_to_rust,
                    rust_to_ado,
                }
            }
        }

        pub struct AdoStepServices<'a> {
            fresh_ado_var: &'a mut dyn FnMut() -> String,
            ado_to_rust: Vec<(String, String, bool)>,
            rust_to_ado: Vec<(String, String)>,
        }

        impl AdoStepServices<'_> {
            /// Return the raw string identifier for the given
            /// [`AdoResourcesRepositoryId`].
            pub fn resolve_repository_id(&self, repo_id: AdoResourcesRepositoryId) -> String {
                repo_id.repo_id
            }

            /// Set the specified flowey Var using the value of the given ADO var.
            // TODO: is there a good way to allow auto-casting the ADO var back
            // to a WriteVar<T>, instead of just a String? It's complicated by
            // the fact that the ADO var to flowey bridge is handled by the ADO
            // backend, which itself needs to know type info...
            pub fn set_var(&mut self, var: ClaimedWriteVar<String>, from_ado_var: AdoRuntimeVar) {
                self.ado_to_rust.push((
                    from_ado_var.ado_var.into(),
                    var.backing_var,
                    from_ado_var.is_secret,
                ))
            }

            /// Get the value of a flowey Var as a ADO runtime variable.
            pub fn get_var(&mut self, var: ClaimedReadVar<String>) -> AdoRuntimeVar {
                let backing_var = if let ReadVarBacking::RuntimeVar {
                    var,
                    is_side_effect,
                } = &var.backing_var
                {
                    assert!(!is_side_effect);
                    var
                } else {
                    todo!("support inline ado read vars")
                };

                let new_ado_var_name = (self.fresh_ado_var)();

                self.rust_to_ado
                    .push((backing_var.clone(), new_ado_var_name.clone()));
                AdoRuntimeVar::dangerous_from_global(new_ado_var_name, false)
            }
        }
    }

    pub mod github {
        use crate::node::ClaimVar;
        use crate::node::NodeCtx;
        use crate::node::ReadVar;
        use crate::node::ReadVarBacking;
        use crate::node::SideEffect;
        use crate::node::StepCtx;
        use crate::node::VarClaimed;
        use crate::node::VarNotClaimed;
        use crate::node::WriteVar;
        use std::collections::BTreeMap;

        pub struct GhStepBuilder {
            display_name: String,
            cond: Option<ReadVar<bool>>,
            uses: String,
            with: Option<BTreeMap<String, GhParam>>,
            outputs: BTreeMap<String, Vec<WriteVar<String>>>,
            run_after: Vec<ReadVar<SideEffect>>,
            permissions: BTreeMap<GhPermission, GhPermissionValue>,
        }

        impl GhStepBuilder {
            /// Creates a new GitHub step builder, with the given display name and
            /// action to use. For example, the following code generates the following yaml:
            ///
            /// ```ignore
            /// GhStepBuilder::new("Check out repository code", "actions/checkout@v4").finish()
            /// ```
            ///
            /// ```ignore
            /// - name: Check out repository code
            ///   uses: actions/checkout@v4
            /// ```
            ///
            /// For more information on the yaml syntax for the `name` and `uses` parameters,
            /// see <https://docs.github.com/en/actions/writing-workflows/workflow-syntax-for-github-actions#jobsjob_idstepsname>
            pub fn new(display_name: impl AsRef<str>, uses: impl AsRef<str>) -> Self {
                Self {
                    display_name: display_name.as_ref().into(),
                    cond: None,
                    uses: uses.as_ref().into(),
                    with: None,
                    outputs: BTreeMap::new(),
                    run_after: Vec::new(),
                    permissions: BTreeMap::new(),
                }
            }

            /// Adds a condition [`ReadVar<bool>`] to the step,
            /// such that the step only executes if the condition is true.
            /// This is equivalent to using an `if` conditional in the yaml.
            ///
            /// For more information on the yaml syntax for `if` conditionals, see
            /// <https://docs.github.com/en/actions/writing-workflows/workflow-syntax-for-github-actions#jobsjob_idstepsname>
            pub fn condition(mut self, cond: ReadVar<bool>) -> Self {
                self.cond = Some(cond);
                self
            }

            /// Adds a parameter to the step, specified as a key-value pair corresponding
            /// to the param name and value. For example the following code generates the following yaml:
            ///
            /// ```rust,ignore
            /// let (client_id, write_client_id) = ctx.new_var();
            /// let (tenant_id, write_tenant_id) = ctx.new_var();
            /// let (subscription_id, write_subscription_id) = ctx.new_var();
            /// // ... insert rust step writing to each of those secrets ...
            /// GhStepBuilder::new("Azure Login", "Azure/login@v2")
            ///               .with("client-id", client_id)
            ///               .with("tenant-id", tenant_id)
            ///               .with("subscription-id", subscription_id)
            /// ```
            ///
            /// ```text
            /// - name: Azure Login
            ///   uses: Azure/login@v2
            ///   with:
            ///     client-id: ${{ env.floweyvar1 }} // Assuming the backend wrote client_id to floweyvar1
            ///     tenant-id: ${{ env.floweyvar2 }} // Assuming the backend wrote tenant-id to floweyvar2
            ///     subscription-id: ${{ env.floweyvar3 }} // Assuming the backend wrote subscription-id to floweyvar3
            /// ```
            ///
            /// For more information on the yaml syntax for the `with` parameters,
            /// see <https://docs.github.com/en/actions/writing-workflows/workflow-syntax-for-github-actions#jobsjob_idstepswith>
            pub fn with(mut self, k: impl AsRef<str>, v: impl Into<GhParam>) -> Self {
                self.with.get_or_insert_with(BTreeMap::new);
                if let Some(with) = &mut self.with {
                    with.insert(k.as_ref().to_string(), v.into());
                }
                self
            }

            /// Specifies an output to read from the step, specified as a key-value pair
            /// corresponding to the output name and the flowey var to write the output to.
            ///
            /// This is equivalent to writing into `v` the output of a step in the yaml using:
            /// `${{ steps.<backend-assigned-step-id>.outputs.<k> }}`
            ///
            /// For more information on step outputs, see
            /// <https://docs.github.com/en/actions/sharing-automations/creating-actions/metadata-syntax-for-github-actions#outputs-for-composite-actions>
            pub fn output(mut self, k: impl AsRef<str>, v: WriteVar<String>) -> Self {
                self.outputs
                    .entry(k.as_ref().to_string())
                    .or_default()
                    .push(v);
                self
            }

            /// Specifies a side-effect that must be resolved before this step can run.
            pub fn run_after(mut self, side_effect: ReadVar<SideEffect>) -> Self {
                self.run_after.push(side_effect);
                self
            }

            /// Declare that this step requires a certain GITHUB_TOKEN permission in order to run.
            ///
            /// For more info about Github Actions permissions, see [`gh_grant_permissions`](crate::pipeline::PipelineJob::gh_grant_permissions) and
            /// <https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/assigning-permissions-to-jobs>
            pub fn requires_permission(
                mut self,
                perm: GhPermission,
                value: GhPermissionValue,
            ) -> Self {
                self.permissions.insert(perm, value);
                self
            }

            /// Finish building the step, emitting it to the backend and returning a side-effect.
            #[track_caller]
            pub fn finish(self, ctx: &mut NodeCtx<'_>) -> ReadVar<SideEffect> {
                let (side_effect, claim_side_effect) = ctx.new_prefixed_var("auto_se");
                ctx.backend
                    .borrow_mut()
                    .on_claimed_runtime_var(&claim_side_effect.backing_var, false);

                ctx.emit_gh_step_inner(
                    self.display_name,
                    self.cond,
                    self.uses,
                    self.with,
                    self.outputs,
                    self.run_after,
                    self.permissions,
                );

                side_effect
            }
        }

        #[derive(Clone, Debug)]
        pub enum GhParam<C = VarNotClaimed> {
            Static(String),
            FloweyVar(ReadVar<String, C>),
        }

        impl From<String> for GhParam {
            fn from(param: String) -> GhParam {
                GhParam::Static(param)
            }
        }

        impl From<&str> for GhParam {
            fn from(param: &str) -> GhParam {
                GhParam::Static(param.to_string())
            }
        }

        impl From<ReadVar<String>> for GhParam {
            fn from(param: ReadVar<String>) -> GhParam {
                GhParam::FloweyVar(param)
            }
        }

        pub type ClaimedGhParam = GhParam<VarClaimed>;

        impl ClaimVar for GhParam {
            type Claimed = ClaimedGhParam;

            fn claim(self, ctx: &mut StepCtx<'_>) -> ClaimedGhParam {
                match self {
                    GhParam::Static(s) => ClaimedGhParam::Static(s),
                    GhParam::FloweyVar(var) => match &var.backing_var {
                        ReadVarBacking::RuntimeVar { is_side_effect, .. } => {
                            assert!(!is_side_effect);
                            ClaimedGhParam::FloweyVar(var.claim(ctx))
                        }
                        ReadVarBacking::Inline(var) => ClaimedGhParam::Static(var.clone()),
                    },
                }
            }
        }

        /// The assigned permission value for a scope.
        ///
        /// For more details on how these values affect a particular scope, refer to:
        /// <https://docs.github.com/en/actions/using-jobs/assigning-permissions-to-jobs>
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
        pub enum GhPermissionValue {
            Read,
            Write,
            None,
        }

        /// Refers to the scope of a permission granted to the GITHUB_TOKEN
        /// for a job.
        ///
        /// For more details on each scope, refer to:
        /// <https://docs.github.com/en/actions/using-jobs/assigning-permissions-to-jobs>
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
        pub enum GhPermission {
            Actions,
            Attestations,
            Checks,
            Contents,
            Deployments,
            Discussions,
            IdToken,
            Issues,
            Packages,
            Pages,
            PullRequests,
            RepositoryProjects,
            SecurityEvents,
            Statuses,
        }
    }

    pub mod rust {
        use crate::node::ClaimedWriteVar;
        use crate::node::FlowArch;
        use crate::node::FlowBackend;
        use crate::node::FlowPlatform;
        use crate::node::ReadVarValue;
        use crate::node::RuntimeVarDb;
        use serde::Serialize;
        use serde::de::DeserializeOwned;

        pub fn new_rust_runtime_services(
            runtime_var_db: &mut dyn RuntimeVarDb,
            backend: FlowBackend,
            platform: FlowPlatform,
            arch: FlowArch,
        ) -> anyhow::Result<RustRuntimeServices<'_>> {
            Ok(RustRuntimeServices {
                runtime_var_db,
                backend,
                platform,
                arch,
                has_read_secret: false,
                sh: xshell::Shell::new()?,
            })
        }

        pub struct RustRuntimeServices<'a> {
            runtime_var_db: &'a mut dyn RuntimeVarDb,
            backend: FlowBackend,
            platform: FlowPlatform,
            arch: FlowArch,
            has_read_secret: bool,
            /// A pre-initialized xshell::Shell for running commands.
            pub sh: xshell::Shell,
        }

        impl RustRuntimeServices<'_> {
            /// What backend the flow is being running on (e.g: locally, ADO,
            /// GitHub, etc...)
            pub fn backend(&self) -> FlowBackend {
                self.backend
            }

            /// What platform the flow is being running on (e.g: windows, linux,
            /// etc...).
            pub fn platform(&self) -> FlowPlatform {
                self.platform
            }

            /// What arch the flow is being running on (X86_64 or Aarch64)
            pub fn arch(&self) -> FlowArch {
                self.arch
            }

            /// Write a value.
            ///
            /// If this step has already read a secret value, then this will be
            /// written as a secret value, as a conservative estimate to avoid
            /// leaking secrets. Use [`write_secret`](Self::write_secret) or
            /// [`write_not_secret`](Self::write_not_secret) to override this
            /// behavior.
            pub fn write<T>(&mut self, var: ClaimedWriteVar<T>, val: &T)
            where
                T: Serialize + DeserializeOwned,
            {
                self.write_maybe_secret(var, val, self.has_read_secret)
            }

            /// Write a secret value, such as a key or token.
            ///
            /// Flowey will avoid logging this value, and if the value is
            /// converted to a CI environment variable, the CI system will be
            /// told not to print the value either.
            pub fn write_secret<T>(&mut self, var: ClaimedWriteVar<T>, val: &T)
            where
                T: Serialize + DeserializeOwned,
            {
                self.write_maybe_secret(var, val, true)
            }

            /// Write a value that is not secret, even if this step has already
            /// read secret values.
            ///
            /// Usually [`write`](Self::write) is preferred--use this only when
            /// your step reads secret values and you explicitly want to write a
            /// non-secret value.
            pub fn write_not_secret<T>(&mut self, var: ClaimedWriteVar<T>, val: &T)
            where
                T: Serialize + DeserializeOwned,
            {
                self.write_maybe_secret(var, val, false)
            }

            fn write_maybe_secret<T>(&mut self, var: ClaimedWriteVar<T>, val: &T, is_secret: bool)
            where
                T: Serialize + DeserializeOwned,
            {
                let val = if var.is_side_effect {
                    b"null".to_vec()
                } else {
                    serde_json::to_vec(val).expect("improve this error path")
                };
                self.runtime_var_db
                    .set_var(&var.backing_var, is_secret, val);
            }

            pub fn write_all<T>(
                &mut self,
                vars: impl IntoIterator<Item = ClaimedWriteVar<T>>,
                val: &T,
            ) where
                T: Serialize + DeserializeOwned,
            {
                for var in vars {
                    self.write(var, val)
                }
            }

            pub fn read<T: ReadVarValue>(&mut self, var: T) -> T::Value {
                var.read_value(self)
            }

            pub(crate) fn get_var(&mut self, var: &str, is_side_effect: bool) -> Vec<u8> {
                let (v, is_secret) = self.runtime_var_db.get_var(var);
                self.has_read_secret |= is_secret && !is_side_effect;
                v
            }

            /// DANGEROUS: Set the value of _Global_ Environment Variable (GitHub Actions only).
            ///
            /// It is up to the caller to ensure that the variable does not get
            /// unintentionally overwritten or used.
            ///
            /// This method should be used rarely and with great care!
            pub fn dangerous_gh_set_global_env_var(
                &mut self,
                var: String,
                gh_env_var: String,
            ) -> anyhow::Result<()> {
                if !matches!(self.backend, FlowBackend::Github) {
                    return Err(anyhow::anyhow!(
                        "dangerous_set_gh_env_var can only be used on GitHub Actions"
                    ));
                }

                let gh_env_file_path = std::env::var("GITHUB_ENV")?;
                let mut gh_env_file = fs_err::OpenOptions::new()
                    .append(true)
                    .open(gh_env_file_path)?;
                let gh_env_var_assignment = format!(
                    r#"{}<<EOF
{}
EOF
"#,
                    gh_env_var, var
                );
                std::io::Write::write_all(&mut gh_env_file, gh_env_var_assignment.as_bytes())?;

                Ok(())
            }
        }
    }
}

/// The base underlying implementation of all FlowNode variants.
///
/// Do not implement this directly! Use the `new_flow_node!` family of macros
/// instead!
pub trait FlowNodeBase {
    type Request: Serialize + DeserializeOwned;

    fn imports(&mut self, ctx: &mut ImportCtx<'_>);
    fn emit(&mut self, requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()>;

    /// A noop method that all human-written impls of `FlowNodeBase` are
    /// required to implement.
    ///
    /// By implementing this method, you're stating that you "know what you're
    /// doing" by having this manual impl.
    fn i_know_what_im_doing_with_this_manual_impl(&mut self);
}

pub mod erased {
    use crate::node::FlowNodeBase;
    use crate::node::NodeCtx;
    use crate::node::user_facing::*;

    pub struct ErasedNode<N: FlowNodeBase>(pub N);

    impl<N: FlowNodeBase> ErasedNode<N> {
        pub fn from_node(node: N) -> Self {
            Self(node)
        }
    }

    impl<N> FlowNodeBase for ErasedNode<N>
    where
        N: FlowNodeBase,
    {
        // FIXME: this should be using type-erased serde
        type Request = Box<[u8]>;

        fn imports(&mut self, ctx: &mut ImportCtx<'_>) {
            self.0.imports(ctx)
        }

        fn emit(&mut self, requests: Vec<Box<[u8]>>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let mut converted_requests = Vec::new();
            for req in requests {
                converted_requests.push(serde_json::from_slice(&req)?)
            }

            self.0.emit(converted_requests, ctx)
        }

        fn i_know_what_im_doing_with_this_manual_impl(&mut self) {}
    }
}

/// Cheap handle to a registered [`FlowNode`]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeHandle(std::any::TypeId);

impl Ord for NodeHandle {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.modpath().cmp(other.modpath())
    }
}

impl PartialOrd for NodeHandle {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Debug for NodeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.try_modpath(), f)
    }
}

impl NodeHandle {
    pub fn from_type<N: FlowNodeBase + 'static>() -> NodeHandle {
        NodeHandle(std::any::TypeId::of::<N>())
    }

    pub fn from_modpath(modpath: &str) -> NodeHandle {
        node_luts::erased_node_by_modpath().get(modpath).unwrap().0
    }

    pub fn try_from_modpath(modpath: &str) -> Option<NodeHandle> {
        node_luts::erased_node_by_modpath()
            .get(modpath)
            .map(|(s, _)| *s)
    }

    pub fn new_erased_node(&self) -> Box<dyn FlowNodeBase<Request = Box<[u8]>>> {
        let ctor = node_luts::erased_node_by_typeid().get(self).unwrap();
        ctor()
    }

    pub fn modpath(&self) -> &'static str {
        node_luts::modpath_by_node_typeid().get(self).unwrap()
    }

    pub fn try_modpath(&self) -> Option<&'static str> {
        node_luts::modpath_by_node_typeid().get(self).cloned()
    }

    /// Return a dummy NodeHandle, which will panic if `new_erased_node` is ever
    /// called on it.
    pub fn dummy() -> NodeHandle {
        NodeHandle(std::any::TypeId::of::<()>())
    }
}

pub fn list_all_registered_nodes() -> impl Iterator<Item = NodeHandle> {
    node_luts::modpath_by_node_typeid().keys().cloned()
}

// Encapsulate these look up tables in their own module to limit the scope of
// the HashMap import.
//
// In general, using HashMap in flowey is a recipe for disaster, given that
// iterating through the hash-map will result in non-deterministic orderings,
// which can cause annoying ordering churn.
//
// That said, in this case, it's OK since the code using these LUTs won't ever
// iterate through the map.
//
// Why is the HashMap even necessary vs. a BTreeMap?
//
// Well... NodeHandle's `Ord` impl does a `modpath` comparison instead of a
// TypeId comparison, since TypeId will vary between compilations.
mod node_luts {
    use super::FlowNodeBase;
    use super::NodeHandle;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    pub(super) fn modpath_by_node_typeid() -> &'static HashMap<NodeHandle, &'static str> {
        static TYPEID_TO_MODPATH: OnceLock<HashMap<NodeHandle, &'static str>> = OnceLock::new();

        TYPEID_TO_MODPATH.get_or_init(|| {
            let mut lookup = HashMap::new();
            for crate::node::private::FlowNodeMeta {
                module_path,
                ctor: _,
                typeid,
            } in crate::node::private::FLOW_NODES
            {
                let existing = lookup.insert(
                    NodeHandle(*typeid),
                    module_path
                        .strip_suffix("::_only_one_call_to_flowey_node_per_module")
                        .unwrap(),
                );
                // if this were to fire for an array where the key is a TypeId...
                // something has gone _terribly_ wrong
                assert!(existing.is_none())
            }

            lookup
        })
    }

    pub(super) fn erased_node_by_typeid()
    -> &'static HashMap<NodeHandle, fn() -> Box<dyn FlowNodeBase<Request = Box<[u8]>>>> {
        static LOOKUP: OnceLock<
            HashMap<NodeHandle, fn() -> Box<dyn FlowNodeBase<Request = Box<[u8]>>>>,
        > = OnceLock::new();

        LOOKUP.get_or_init(|| {
            let mut lookup = HashMap::new();
            for crate::node::private::FlowNodeMeta {
                module_path: _,
                ctor,
                typeid,
            } in crate::node::private::FLOW_NODES
            {
                let existing = lookup.insert(NodeHandle(*typeid), *ctor);
                // if this were to fire for an array where the key is a TypeId...
                // something has gone _terribly_ wrong
                assert!(existing.is_none())
            }

            lookup
        })
    }

    pub(super) fn erased_node_by_modpath() -> &'static HashMap<
        &'static str,
        (
            NodeHandle,
            fn() -> Box<dyn FlowNodeBase<Request = Box<[u8]>>>,
        ),
    > {
        static MODPATH_LOOKUP: OnceLock<
            HashMap<
                &'static str,
                (
                    NodeHandle,
                    fn() -> Box<dyn FlowNodeBase<Request = Box<[u8]>>>,
                ),
            >,
        > = OnceLock::new();

        MODPATH_LOOKUP.get_or_init(|| {
            let mut lookup = HashMap::new();
            for crate::node::private::FlowNodeMeta { module_path, ctor, typeid } in crate::node::private::FLOW_NODES {
                let existing = lookup.insert(module_path.strip_suffix("::_only_one_call_to_flowey_node_per_module").unwrap(), (NodeHandle(*typeid), *ctor));
                if existing.is_some() {
                    panic!("conflicting node registrations at {module_path}! please ensure there is a single node per module!")
                }
            }
            lookup
        })
    }
}

#[doc(hidden)]
pub mod private {
    pub use linkme;

    pub struct FlowNodeMeta {
        pub module_path: &'static str,
        pub ctor: fn() -> Box<dyn super::FlowNodeBase<Request = Box<[u8]>>>,
        pub typeid: std::any::TypeId,
    }

    #[linkme::distributed_slice]
    pub static FLOW_NODES: [FlowNodeMeta] = [..];

    // UNSAFETY: linkme uses manual link sections, which are unsafe.
    #[expect(unsafe_code)]
    #[linkme::distributed_slice(FLOW_NODES)]
    static DUMMY_FLOW_NODE: FlowNodeMeta = FlowNodeMeta {
        module_path: "<dummy>::_only_one_call_to_flowey_node_per_module",
        ctor: || unreachable!(),
        typeid: std::any::TypeId::of::<()>(),
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! new_flow_node_base {
    (struct Node) => {
        /// (see module-level docs)
        #[non_exhaustive]
        pub struct Node;

        mod _only_one_call_to_flowey_node_per_module {
            const _: () = {
                use $crate::node::private::linkme;

                fn new_erased() -> Box<dyn $crate::node::FlowNodeBase<Request = Box<[u8]>>> {
                    Box::new($crate::node::erased::ErasedNode(super::Node))
                }

                #[linkme::distributed_slice($crate::node::private::FLOW_NODES)]
                #[linkme(crate = linkme)]
                static FLOW_NODE: $crate::node::private::FlowNodeMeta =
                    $crate::node::private::FlowNodeMeta {
                        module_path: module_path!(),
                        ctor: new_erased,
                        typeid: std::any::TypeId::of::<super::Node>(),
                    };
            };
        }
    };
}

/// A reusable unit of automation logic in flowey.
///
/// FlowNodes process requests, emit steps, and can depend on other nodes. They are
/// the building blocks for creating complex automation workflows.
///
/// # The Node/Request Pattern
///
/// Every node has an associated **Request** type that defines what the node can do.
/// Nodes receive a vector of requests and process them together, allowing for
/// aggregation and conflict resolution.
///
/// # Example: Basic FlowNode Implementation
///
/// ```rust,ignore
/// use flowey_core::node::*;
///
/// // Define the node
/// new_flow_node!(struct Node);
///
/// // Define requests using the flowey_request! macro
/// flowey_request! {
///     pub enum Request {
///         InstallRust(String),                    // Install specific version
///         EnsureInstalled(WriteVar<SideEffect>),  // Ensure it's installed
///         GetCargoHome(WriteVar<PathBuf>),        // Get CARGO_HOME path
///     }
/// }
///
/// impl FlowNode for Node {
///     type Request = Request;
///
///     fn imports(ctx: &mut ImportCtx<'_>) {
///         // Declare node dependencies
///         ctx.import::<other_node::Node>();
///     }
///
///     fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
///         // 1. Aggregate and validate requests
///         let mut version = None;
///         let mut ensure_installed = Vec::new();
///         let mut get_cargo_home = Vec::new();
///
///         for req in requests {
///             match req {
///                 Request::InstallRust(v) => {
///                     same_across_all_reqs("version", &mut version, v)?;
///                 }
///                 Request::EnsureInstalled(var) => ensure_installed.push(var),
///                 Request::GetCargoHome(var) => get_cargo_home.push(var),
///             }
///         }
///
///         let version = version.ok_or(anyhow::anyhow!("Version not specified"))?;
///
///         // 2. Emit steps to do the work
///         ctx.emit_rust_step("install rust", |ctx| {
///             let ensure_installed = ensure_installed.claim(ctx);
///             let get_cargo_home = get_cargo_home.claim(ctx);
///             move |rt| {
///                 // Install rust with the specified version
///                 // Write to all the output variables
///                 for var in ensure_installed {
///                     rt.write(var, &());
///                 }
///                 for var in get_cargo_home {
///                     rt.write(var, &PathBuf::from("/path/to/cargo"));
///                 }
///                 Ok(())
///             }
///         });
///
///         Ok(())
///     }
/// }
/// ```
///
/// # When to Use FlowNode vs SimpleFlowNode
///
/// **Use `FlowNode`** when you need to:
/// - Aggregate multiple requests and process them together
/// - Resolve conflicts between requests
/// - Perform complex request validation
///
/// **Use [`SimpleFlowNode`]** when:
/// - Each request can be processed independently
/// - No aggregation logic is needed
pub trait FlowNode {
    /// The request type that defines what operations this node can perform.
    ///
    /// Use the [`crate::flowey_request!`] macro to define this type.
    type Request: Serialize + DeserializeOwned;

    /// A list of nodes that this node is capable of taking a dependency on.
    ///
    /// Attempting to take a dep on a node that wasn't imported via this method
    /// will result in an error during flow resolution time.
    ///
    /// * * *
    ///
    /// To put it bluntly: This is boilerplate.
    ///
    /// We (the flowey devs) are thinking about ways to avoid requiring this
    /// method, but do not have a good solution at this time.
    fn imports(ctx: &mut ImportCtx<'_>);

    /// Given a set of incoming `requests`, emit various steps to run, set
    /// various dependencies, etc...
    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()>;
}

#[macro_export]
macro_rules! new_flow_node {
    (struct Node) => {
        $crate::new_flow_node_base!(struct Node);

        impl $crate::node::FlowNodeBase for Node
        where
            Node: FlowNode,
        {
            type Request = <Node as FlowNode>::Request;

            fn imports(&mut self, dep: &mut $crate::node::ImportCtx<'_>) {
                <Node as FlowNode>::imports(dep)
            }

            fn emit(
                &mut self,
                requests: Vec<Self::Request>,
                ctx: &mut $crate::node::NodeCtx<'_>,
            ) -> anyhow::Result<()> {
                <Node as FlowNode>::emit(requests, ctx)
            }

            fn i_know_what_im_doing_with_this_manual_impl(&mut self) {}
        }
    };
}

/// A helper trait to streamline implementing [`FlowNode`] instances that only
/// ever operate on a single request at a time.
///
/// In essence, [`SimpleFlowNode`] handles the boilerplate (and rightward-drift)
/// of manually writing:
///
/// ```ignore
/// impl FlowNode for Node {
///     fn imports(dep: &mut ImportCtx<'_>) { ... }
///     fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) {
///         for req in requests {
///             Node::process_request(req, ctx)
///         }
///     }
/// }
/// ```
///
/// Nodes which accept a `struct Request` often fall into this pattern, whereas
/// nodes which accept a `enum Request` typically require additional logic to
/// aggregate / resolve incoming requests.
pub trait SimpleFlowNode {
    type Request: Serialize + DeserializeOwned;

    /// A list of nodes that this node is capable of taking a dependency on.
    ///
    /// Attempting to take a dep on a node that wasn't imported via this method
    /// will result in an error during flow resolution time.
    ///
    /// * * *
    ///
    /// To put it bluntly: This is boilerplate.
    ///
    /// We (the flowey devs) are thinking about ways to avoid requiring this
    /// method, but do not have a good solution at this time.
    fn imports(ctx: &mut ImportCtx<'_>);

    /// Process a single incoming `Self::Request`
    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()>;
}

#[macro_export]
macro_rules! new_simple_flow_node {
    (struct Node) => {
        $crate::new_flow_node_base!(struct Node);

        impl $crate::node::FlowNodeBase for Node
        where
            Node: $crate::node::SimpleFlowNode,
        {
            type Request = <Node as $crate::node::SimpleFlowNode>::Request;

            fn imports(&mut self, dep: &mut $crate::node::ImportCtx<'_>) {
                <Node as $crate::node::SimpleFlowNode>::imports(dep)
            }

            fn emit(&mut self, requests: Vec<Self::Request>, ctx: &mut $crate::node::NodeCtx<'_>) -> anyhow::Result<()> {
                for req in requests {
                    <Node as $crate::node::SimpleFlowNode>::process_request(req, ctx)?
                }

                Ok(())
            }

            fn i_know_what_im_doing_with_this_manual_impl(&mut self) {}
        }
    };
}

/// A "glue" trait which improves [`NodeCtx::req`] ergonomics, by tying a
/// particular `Request` type to its corresponding [`FlowNode`].
///
/// This trait should be autogenerated via [`flowey_request!`] - do not try to
/// implement it manually!
///
/// [`flowey_request!`]: crate::flowey_request
pub trait IntoRequest {
    type Node: FlowNodeBase;
    fn into_request(self) -> <Self::Node as FlowNodeBase>::Request;

    /// By implementing this method manually, you're indicating that you know what you're
    /// doing,
    #[doc(hidden)]
    #[expect(nonstandard_style)]
    fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self);
}

#[doc(hidden)]
#[macro_export]
macro_rules! __flowey_request_inner {
    //
    // @emit_struct: emit structs for each variant of the request enum
    //
    (@emit_struct [$req:ident]
        $(#[$a:meta])*
        $variant:ident($($tt:tt)*),
        $($rest:tt)*
    ) => {
        $(#[$a])*
        #[derive($crate::reexports::Serialize, $crate::reexports::Deserialize)]
        pub struct $variant($($tt)*);

        impl IntoRequest for $variant {
            type Node = Node;
            fn into_request(self) -> $req {
                $req::$variant(self)
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }

        $crate::__flowey_request_inner!(@emit_struct [$req] $($rest)*);
    };
    (@emit_struct [$req:ident]
        $(#[$a:meta])*
        $variant:ident { $($tt:tt)* },
        $($rest:tt)*
    ) => {
        $(#[$a])*
        #[derive($crate::reexports::Serialize, $crate::reexports::Deserialize)]
        pub struct $variant {
            $($tt)*
        }

        impl IntoRequest for $variant {
            type Node = Node;
            fn into_request(self) -> $req {
                $req::$variant(self)
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }

        $crate::__flowey_request_inner!(@emit_struct [$req] $($rest)*);
    };
    (@emit_struct [$req:ident]
        $(#[$a:meta])*
        $variant:ident,
        $($rest:tt)*
    ) => {
        $(#[$a])*
        #[derive(Serialize, Deserialize)]
        pub struct $variant;

        impl IntoRequest for $variant {
            type Node = Node;
            fn into_request(self) -> $req {
                $req::$variant(self)
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }

        $crate::__flowey_request_inner!(@emit_struct [$req] $($rest)*);
    };
    (@emit_struct [$req:ident]
    ) => {};

    //
    // @emit_req_enum: build up root request enum
    //
    (@emit_req_enum [$req:ident($($root_a:meta,)*), $($prev:ident[$($prev_a:meta,)*])*]
        $(#[$a:meta])*
        $variant:ident($($tt:tt)*),
        $($rest:tt)*
    ) => {
        $crate::__flowey_request_inner!(@emit_req_enum [$req($($root_a,)*), $($prev[$($prev_a,)*])* $variant[$($a,)*]] $($rest)*);
    };
    (@emit_req_enum [$req:ident($($root_a:meta,)*), $($prev:ident[$($prev_a:meta,)*])*]
        $(#[$a:meta])*
        $variant:ident { $($tt:tt)* },
        $($rest:tt)*
    ) => {
        $crate::__flowey_request_inner!(@emit_req_enum [$req($($root_a,)*), $($prev[$($prev_a,)*])* $variant[$($a,)*]] $($rest)*);
    };
    (@emit_req_enum [$req:ident($($root_a:meta,)*), $($prev:ident[$($prev_a:meta,)*])*]
        $(#[$a:meta])*
        $variant:ident,
        $($rest:tt)*
    ) => {
        $crate::__flowey_request_inner!(@emit_req_enum [$req($($root_a,)*), $($prev[$($prev_a,)*])* $variant[$($a,)*]] $($rest)*);
    };
    (@emit_req_enum [$req:ident($($root_a:meta,)*), $($prev:ident[$($prev_a:meta,)*])*]
    ) => {
        #[derive(Serialize, Deserialize)]
        pub enum $req {$(
            $(#[$prev_a])*
            $prev(self::req::$prev),
        )*}

        impl IntoRequest for $req {
            type Node = Node;
            fn into_request(self) -> $req {
                self
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }
    };
}

/// Declare a new `Request` type for the current `Node`.
///
/// ## `struct` and `enum` Requests
///
/// When wrapping a vanilla Rust `struct` and `enum` declaration, this macro
/// simply derives [`Serialize`], [`Deserialize`], and [`IntoRequest`] for the
/// type, and does nothing else.
///
/// ## `enum_struct` Requests
///
/// This macro also supports a special kind of `enum_struct` derive, which
/// allows declaring a Request enum where each variant is split off into its own
/// separate (named) `struct`.
///
/// e.g:
///
/// ```ignore
/// flowey_request! {
///     pub enum_struct Foo {
///         Bar,
///         Baz(pub usize),
///         Qux(pub String),
///     }
/// }
/// ```
///
/// will be expanded into:
///
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// pub enum Foo {
///    Bar(req::Bar),
///    Baz(req::Baz),
///    Qux(req::Qux),
/// }
///
/// pud mod req {
///     #[derive(Serialize, Deserialize)]
///     pub struct Bar;
///
///     #[derive(Serialize, Deserialize)]
///     pub struct Baz(pub usize);
///
///     #[derive(Serialize, Deserialize)]
///     pub struct Qux(pub String);
/// }
/// ```
#[macro_export]
macro_rules! flowey_request {
    (
        $(#[$root_a:meta])*
        pub enum_struct $req:ident {
            $($tt:tt)*
        }
    ) => {
        $crate::__flowey_request_inner!(@emit_req_enum [$req($($root_a,)*),] $($tt)*);
        pub mod req {
            use super::*;
            $crate::__flowey_request_inner!(@emit_struct [$req] $($tt)*);
        }
    };

    (
        $(#[$a:meta])*
        pub enum $req:ident {
            $($tt:tt)*
        }
    ) => {
        $(#[$a])*
        #[derive($crate::reexports::Serialize, $crate::reexports::Deserialize)]
        pub enum $req {
            $($tt)*
        }

        impl $crate::node::IntoRequest for $req {
            type Node = Node;
            fn into_request(self) -> $req {
                self
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }
    };

    (
        $(#[$a:meta])*
        pub struct $req:ident {
            $($tt:tt)*
        }
    ) => {
        $(#[$a])*
        #[derive($crate::reexports::Serialize, $crate::reexports::Deserialize)]
        pub struct $req {
            $($tt)*
        }

        impl $crate::node::IntoRequest for $req {
            type Node = Node;
            fn into_request(self) -> $req {
                self
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }
    };

    (
        $(#[$a:meta])*
        pub struct $req:ident($($tt:tt)*);
    ) => {
        $(#[$a])*
        #[derive($crate::reexports::Serialize, $crate::reexports::Deserialize)]
        pub struct $req($($tt)*);

        impl $crate::node::IntoRequest for $req {
            type Node = Node;
            fn into_request(self) -> $req {
                self
            }
            fn do_not_manually_impl_this_trait__use_the_flowey_request_macro_instead(&mut self) {}
        }
    };
}

/// Construct a command to run via the flowey shell.
///
/// This is a thin wrapper around [`xshell::cmd!`] that abstracts the
/// underlying shell implementation, making it possible to swap in a
/// different shell backend in the future (e.g. wrapping commands in `nix-shell --pure`)
/// without touching every callsite.
///
/// # Example
///
/// ```ignore
/// flowey::shell_cmd!(rt, "cargo build --release").run()?;
/// ```
#[macro_export]
macro_rules! shell_cmd {
    ($rt:expr, $cmd:literal) => {
        $crate::reexports::xshell::cmd!($rt.sh, $cmd)
    };
}
