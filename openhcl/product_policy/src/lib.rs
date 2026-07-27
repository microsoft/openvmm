// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured product policy: enum, per-VM value, codec.
//!
//! To add a product: define its body in a sibling module, then add
//! one `N => Variant(body::Body),` line to `define_product_policy!`
//! below. `N` is a mesh wire tag and must never be reused.

#![forbid(unsafe_code)]

extern crate alloc;

/// Cwcow policy body and validation methods.
pub mod cwcow;
/// Shared helpers for product policy validation and serialization.
pub mod product_policy_helpers;
/// Sivm policy body and validation methods.
pub mod sivm;
/// UEFI enforced security settings.
pub mod uefi_security_policy;

use alloc::vec::Vec;

#[doc(hidden)]
pub use paste::paste as __paste;

/// Per-VM measured product policy.
///
/// `None` means no policy was installed; any `Some(_)` carries the
/// decoded variant body.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(transparent))]
pub struct MeasuredProductPolicy(Option<ProductPolicy>);

impl MeasuredProductPolicy {
    /// Wrap the decoded policy (or its absence).
    pub fn new(policy: Option<ProductPolicy>) -> Self {
        Self(policy)
    }

    /// The decoded policy, if any.
    pub fn raw(&self) -> Option<&ProductPolicy> {
        self.0.as_ref()
    }
}

#[derive(mesh_protobuf::Protobuf)]
struct ProductPolicyInternal {
    #[mesh(1)]
    magic: u64,
    #[mesh(2)]
    policy: ProductPolicy,
}

impl ProductPolicyInternal {
    /// Magic header for an encoded product policy payload ("OHCLPOL").
    const MAGIC: u64 = 0x4F48434C504F4C00;
}

/// Defines the `ProductPolicy` enum and, for each variant `Foo(Body)`,
/// a `MeasuredProductPolicy::foo(|body| ...) -> anyhow::Result<Option<T>>`
/// accessor (`Ok(None)` when the policy is absent or a different
/// variant; closure errors propagate).
macro_rules! define_product_policy {
    (
        package = $pkg:literal ;
        $(
            $(#[$vmeta:meta])*
            $tag:literal => $variant:ident ( $body:path )
        );+ $(;)?
    ) => {
        /// Measured product policy. Mesh tags are part of the wire
        /// format and must never be reused.
        #[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq)]
        #[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(
            feature = "manifest",
            serde(rename_all = "snake_case", deny_unknown_fields)
        )]
        #[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
        #[cfg_attr(feature = "inspect", inspect(external_tag))]
        #[mesh(package = $pkg)]
        pub enum ProductPolicy {
            $(
                $(#[$vmeta])*
                #[mesh($tag)]
                $variant($body),
            )+
        }

        impl ProductPolicy {
            /// Lowercased variant name.
            pub fn name(&self) -> &'static str {
                $crate::__paste! {
                    match self {
                        $( Self::$variant(_) => stringify!([<$variant:lower>]), )+
                    }
                }
            }
        }

        $crate::__paste! {
            $(
                impl $crate::MeasuredProductPolicy {
                    #[doc = concat!(
                        "Run `f` over the `",
                        stringify!($variant),
                        "` body if installed. Closure errors propagate via the outer `Result`; the inner `Option` signals whether the closure ran."
                    )]
                    pub fn [<$variant:lower>]<T>(
                        &self,
                        f: impl ::core::ops::FnOnce(&$body) -> ::anyhow::Result<T>,
                    ) -> ::anyhow::Result<::core::option::Option<T>> {
                        match self.raw() {
                            ::core::option::Option::Some(ProductPolicy::$variant(p)) => {
                                f(p).map(::core::option::Option::Some)
                            }
                            _ => ::core::result::Result::Ok(::core::option::Option::None),
                        }
                    }
                }
            )+
        }
    };
}

define_product_policy! {
    package = "openhcl.product_policy";

    /// Sivm.
    1 => Sivm(sivm::SivmPolicy);

    /// Cwcow.
    2 => Cwcow(cwcow::CwcowPolicy);
}

// --- Codec ---

/// Errors from [`decode_product_policy`].
#[derive(Debug)]
pub enum ProductPolicyDecodeError {
    /// `mesh_protobuf` rejected the bytes.
    Mesh(mesh_protobuf::Error),
    /// The decoded payload did not carry the expected magic header.
    BadMagic,
}

impl core::fmt::Display for ProductPolicyDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mesh(_) => write!(f, "product policy mesh decode error"),
            Self::BadMagic => write!(f, "product policy magic header mismatch"),
        }
    }
}

impl core::error::Error for ProductPolicyDecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Mesh(e) => Some(e),
            Self::BadMagic => None,
        }
    }
}

/// Encode a policy as `mesh_protobuf` bytes for the IGVM payload.
pub fn encode_product_policy(policy: &ProductPolicy) -> Vec<u8> {
    let policy = ProductPolicyInternal {
        magic: ProductPolicyInternal::MAGIC,
        policy: policy.clone(),
    };
    mesh_protobuf::encode(policy)
}

/// Decode `mesh_protobuf` bytes. Caller must skip empty payloads
/// (which signal "no policy installed") before calling.
pub fn decode_product_policy(bytes: &[u8]) -> Result<ProductPolicy, ProductPolicyDecodeError> {
    let data: ProductPolicyInternal =
        mesh_protobuf::decode(bytes).map_err(ProductPolicyDecodeError::Mesh)?;

    if data.magic != ProductPolicyInternal::MAGIC {
        return Err(ProductPolicyDecodeError::BadMagic);
    }

    Ok(data.policy)
}

pub use uefi_security_policy::UefiSecurityPolicy;

#[cfg(test)]
mod tests;
