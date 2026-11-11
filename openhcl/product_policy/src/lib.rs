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

/// Absolute path of the measured product policy file in the OpenHCL initrd.
pub const PRODUCT_POLICY_PATH: &str = "/openhcl-product-policy";

/// Per-VM measured product policy and allowed PCI devices.
///
/// A `None` policy means no policy was installed; any `Some(_)` carries the
/// decoded variant body.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(transparent))]
pub struct MeasuredProductPolicy {
    policy: Option<ProductPolicy>,
    #[cfg_attr(feature = "inspect", inspect(skip))]
    allowed_pci_devices: Vec<AllowedPciDevice>,
}

impl MeasuredProductPolicy {
    /// Wrap the decoded policy (or its absence).
    pub fn new(policy: Option<ProductPolicy>) -> Self {
        Self {
            policy,
            allowed_pci_devices: Vec::new(),
        }
    }

    /// The decoded policy, if any.
    pub fn raw(&self) -> Option<&ProductPolicy> {
        self.policy.as_ref()
    }

    /// The PCI devices that OpenHCL's VPCI relay may expose to VTL0.
    pub fn allowed_pci_devices(&self) -> &[AllowedPciDevice] {
        &self.allowed_pci_devices
    }
}

#[derive(mesh_protobuf::Protobuf)]
struct ProductPolicyInternal {
    #[mesh(1)]
    magic: u64,
    #[mesh(2)]
    policy: Option<ProductPolicy>,
    #[mesh(3)]
    allowed_pci_devices: Vec<AllowedPciDevice>,
}

impl ProductPolicyInternal {
    /// Magic header for an encoded product policy payload ("OHCLPOL").
    const MAGIC: u64 = 0x4F48434C504F4C00;
}

/// A PCI device that OpenHCL's VPCI relay may expose to VTL0: one with every
/// ID that is set. Mesh tags are part of the wire format and must never be
/// reused.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(
    feature = "manifest",
    derive(serde::Deserialize),
    serde(deny_unknown_fields)
)]
#[mesh(package = "openhcl.product_policy")]
pub struct AllowedPciDevice {
    /// Vendor ID.
    #[mesh(1)]
    pub vendor_id: Option<u16>,
    /// Device ID.
    #[mesh(2)]
    pub device_id: Option<u16>,
    /// Revision ID.
    #[mesh(3)]
    pub revision_id: Option<u8>,
    /// Base class code.
    #[mesh(4)]
    pub base_class: Option<u8>,
    /// Subclass code.
    #[mesh(5)]
    pub sub_class: Option<u8>,
    /// Programming interface.
    #[mesh(6)]
    pub prog_if: Option<u8>,
    /// Subsystem vendor ID.
    #[mesh(7)]
    pub sub_vendor_id: Option<u16>,
    /// Subsystem ID.
    #[mesh(8)]
    pub sub_system_id: Option<u16>,
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

/// Encode a policy and device list as `mesh_protobuf` bytes for the initrd
/// policy file.
pub fn encode_product_policy(
    policy: Option<&ProductPolicy>,
    allowed_pci_devices: &[AllowedPciDevice],
) -> Vec<u8> {
    let policy = ProductPolicyInternal {
        magic: ProductPolicyInternal::MAGIC,
        policy: policy.cloned(),
        allowed_pci_devices: allowed_pci_devices.to_vec(),
    };
    mesh_protobuf::encode(policy)
}

/// Decode `mesh_protobuf` bytes.
pub fn decode_product_policy(
    bytes: &[u8],
) -> Result<MeasuredProductPolicy, ProductPolicyDecodeError> {
    let data: ProductPolicyInternal =
        mesh_protobuf::decode(bytes).map_err(ProductPolicyDecodeError::Mesh)?;

    if data.magic != ProductPolicyInternal::MAGIC {
        return Err(ProductPolicyDecodeError::BadMagic);
    }

    Ok(MeasuredProductPolicy {
        policy: data.policy,
        allowed_pci_devices: data.allowed_pci_devices,
    })
}

pub use uefi_security_policy::UefiSecurityPolicy;

#[cfg(test)]
mod tests;
