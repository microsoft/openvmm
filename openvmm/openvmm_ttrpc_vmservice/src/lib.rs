// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust bindings to the `vmservice.proto` TTRPC API

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![expect(clippy::enum_variant_names, clippy::large_enum_variant)]
#![expect(clippy::allow_attributes)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use mesh_rpc as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/vmservice.rs"));

#[cfg(test)]
mod tests {
    // see enum_zero_value test for more info
    macro_rules! validate_enums {
        ( $($ty:ty),+ $(,)?) => { $(
            {
                assert_eq!(
                    <$ty>::from_i32(0),
                    Some(<$ty>::default()),
                    "enum {} default should be zero value",
                    stringify!($ty),
                );
                let s = <$ty>::default().as_str_name();
                assert!(
                    s.ends_with("_UNSPECIFIED") || s.ends_with("_UNKNOWN"),
                    "enum {} default ({}) should be unspecified or unknown",
                    stringify!($ty),
                    s,
                );
            };
        )+ };
    }

    /// Validate that default enum value is either unspecified or unknown.
    ///
    /// There's no easy way to have a vector of types (enums) to iterate over.
    /// Also, `prost` enumerations don't implement a specific trait for the `as_str_name` function,
    /// so we cannot write:
    ///
    /// ```
    /// vec![
    ///     VmState::default(),
    ///     IpProtocol::default(),
    ///     /* ... */
    /// ].map(|v| v.as_str_name())
    /// ```
    ///
    /// Instead, use a macro (`validate_enums!`) to handle list of enum types.
    #[test]
    fn enum_zero_value() {
        use super::*;

        validate_enums![
            VmState,
            IpProtocol,
            DiskType,
            ResourceModifyType,
            uefi::initial_variables::SecureBootTemplate,
            vm_config::GuestPowerAction,
            vm_properties_request::PropertiesType,
            capabilities_response::Resource,
            capabilities_response::SupportedGuestOs,
        ];
    }
}
