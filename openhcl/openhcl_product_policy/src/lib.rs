// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured product policy: wire types, runtime global, per-product views.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(feature = "std")]
extern crate std;

mod wire;
pub mod cwcow;

pub use wire::{
    ProductPolicy, ProductPolicyDecodeError, decode_product_policy, encode_product_policy,
};

/// Emits the per-product view scaffolding. Hand-write `validate_*`
/// in an `impl<'a> $view<'a> { ... }` next to the invocation.
#[macro_export]
macro_rules! product_view {
    ($view:ident, $body:ty, $variant:path) => {
        #[doc = concat!("View over the ", stringify!($variant), " policy body.")]
        pub struct $view<'a>(Option<&'a $body>);

        impl<'a> $view<'a> {
            /// Borrowed policy view.
            pub fn from_policy(p: &'a $body) -> Self {
                Self(Some(p))
            }
            /// Empty view; `validate_*` methods are no-ops on it.
            pub fn empty() -> Self {
                Self(None)
            }
            /// Whether this product's policy is in effect.
            pub fn is_active(&self) -> bool {
                self.0.is_some()
            }
            /// The borrowed body, if any.
            pub fn body(&self) -> Option<&'a $body> {
                self.0
            }
        }

        #[cfg(feature = "std")]
        impl $view<'static> {
            /// View over the globally-installed policy.
            pub fn current() -> Self {
                match $crate::get() {
                    Some($variant(p)) => Self::from_policy(p),
                    _ => Self::empty(),
                }
            }
        }

        /// Globally-installed policy as this product's view.
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

    /// Install the policy. Idempotent for the same value; `Err(())`
    /// on conflict.
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

    /// The installed policy, if any.
    pub fn get() -> Option<&'static ProductPolicy> {
        POLICY.get().and_then(Option::as_ref)
    }
}

#[cfg(feature = "std")]
pub use global::{get, init};
