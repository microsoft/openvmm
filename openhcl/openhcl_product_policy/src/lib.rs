// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-wire types for the OpenHCL paravisor measured product-policy
//! payload, plus a process-wide accessor and per-product view
//! scaffolding.
//!
//! At boot, `underhill_core` decodes the policy from the measured
//! VTL2 config region and calls [`init`] exactly once with the
//! resulting `Option<ProductPolicy>`. Consumers reach for the
//! product-specific helpers (e.g. [`cwcow::policy`]) and call
//! `validate_*` methods unconditionally — the helper returns an
//! empty view when no policy is installed or when the installed
//! policy is for a different product, and every `validate_*` method
//! is a no-op on an empty view.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(feature = "std")]
extern crate std;

mod wire;
pub mod cwcow;

pub use wire::{
    ProductPolicy, ProductPolicyDecodeError, decode_product_policy,
    encode_product_policy,
};

/// Generate the per-product view scaffolding.
///
/// Emits:
/// - `pub struct $view<'a>(Option<&'a $body>)` with `from_policy` /
///   `empty` / `is_active` / `body`
/// - (under `feature = "std"`) `$view<'static>::current()` reading the global
/// - (under `feature = "std"`) a module-level
///   `pub fn policy() -> $view<'static>` convenience accessor
///
/// Hand-write the product-specific `validate_*` methods in an
/// `impl<'a> $view<'a> { ... }` next to this invocation — both must
/// live in the same crate so Rust's orphan rule for inherent impls
/// is satisfied.
#[macro_export]
macro_rules! product_view {
    ($view:ident, $body:ty, $variant:path) => {

        #[doc = concat!("A view over the ", stringify!($variant), " product policy body.")]
        pub struct $view<'a>(Option<&'a $body>);

        impl<'a> $view<'a> {
            /// View over the borrowed policy body.
            pub fn from_policy(p: &'a $body) -> Self {
                Self(Some(p))
            }
            /// An explicitly empty view. Every `validate_*` method is
            /// a no-op on an empty view.
            pub fn empty() -> Self {
                Self(None)
            }
            /// Whether a policy of this product is in effect.
            pub fn is_active(&self) -> bool {
                self.0.is_some()
            }
            /// The borrowed policy body, if any.
            pub fn body(&self) -> Option<&'a $body> {
                self.0
            }
        }

        #[cfg(feature = "std")]
        impl $view<'static> {
            /// View over the globally-installed product policy.
            pub fn current() -> Self {
                match $crate::get() {
                    Some($variant(p)) => Self::from_policy(p),
                    _ => Self::empty(),
                }
            }
        }

        /// Free fn over the globally-installed product policy.
        #[cfg(feature = "std")]
        pub fn policy() -> $view<'static> {
            <$view<'static>>::current()
        }
    };
}

#[cfg(feature = "std")]
mod global {
    use crate::wire::ProductPolicy;
    use cvm_tracing::CVM_ALLOWED;
    use std::sync::OnceLock;

    static POLICY: OnceLock<Option<ProductPolicy>> = OnceLock::new();

    /// Install the parsed product policy at boot.
    ///
    /// Logs the product variant tag (or `"none"`) at info level
    /// before installing. Idempotent for the same value — so the
    /// servicing-restart path doesn't trip the guard. Returns
    /// `Err(())` only when called with a *different* value after a
    /// previous install.
    pub fn init(policy: Option<ProductPolicy>) -> Result<(), ()> {
        match POLICY.get() {
            Some(existing) if *existing == policy => Ok(()),
            Some(_) => Err(()),
            None => {
                let name = policy.as_ref().map_or("none", ProductPolicy::name);
                tracing::info!(
                    CVM_ALLOWED,
                    product = name,
                    "installing measured product policy"
                );
                POLICY.set(policy).map_err(|_| ())
            }
        }
    }

    /// Returns the installed policy, or `None` if [`init`] has not
    /// been called or was called with `None`. Consumers normally
    /// prefer the product-specific helpers in the per-product
    /// modules (e.g. [`crate::cwcow::policy`]).
    pub fn get() -> Option<&'static ProductPolicy> {
        POLICY.get().and_then(Option::as_ref)
    }
}

#[cfg(feature = "std")]
pub use global::{get, init};
