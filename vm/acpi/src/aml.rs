// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared utilities for generating ACPI Machine Language (AML),
//! particularly for use in differentiated and secondary system
//! description tables (DSDT and SSDT, respectively).

pub mod devices;
pub mod helpers;
pub mod objects;
pub mod ops;
pub mod resources;

pub use self::devices::*;
pub use self::helpers::*;
pub use self::objects::*;
pub use self::ops::*;
pub use self::resources::*;

#[cfg(test)]
pub mod test_helpers {
    pub fn verify_expected_bytes(actual: &[u8], expected: &[u8]) {
        assert_eq!(
            actual.len(),
            expected.len(),
            "Length of buffer does not match"
        );
        for i in 0..actual.len() {
            assert_eq!(actual[i], expected[i], "Mismatch at index {}", i);
        }
    }
}
