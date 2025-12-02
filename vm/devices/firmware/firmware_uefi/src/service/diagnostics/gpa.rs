// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest Physical Address validation and type safety for diagnostics buffer

use inspect::Inspect;
use std::num::NonZeroU32;
use thiserror::Error;

/// Invalid GPA values that should be rejected
const INVALID_GPA_VALUES: [u32; 2] = [0, u32::MAX];

/// Errors that can occur when validating a GPA
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum GpaError {
    /// GPA value is invalid (0 or u32::MAX)
    #[error("Invalid GPA value: {0:#x}")]
    InvalidValue(u32),
}

/// A validated Guest Physical Address for the diagnostics buffer.
///
/// This type guarantees that:
/// - The GPA is not 0
/// - The GPA is not u32::MAX
///
/// This consolidates validation logic that was previously split between
/// multiple locations in the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gpa(NonZeroU32);

impl Gpa {
    /// Create a new validated GPA.
    ///
    /// # Arguments
    /// * `value` - The raw u32 GPA value to validate
    ///
    /// # Errors
    /// Returns `GpaError::InvalidValue` if the value is 0 or u32::MAX
    pub fn new(value: u32) -> Result<Self, GpaError> {
        if INVALID_GPA_VALUES.contains(&value) {
            return Err(GpaError::InvalidValue(value));
        }

        let non_zero = NonZeroU32::new(value).ok_or(GpaError::InvalidValue(value))?;
        Ok(Self(non_zero))
    }

    /// Get the raw u32 value of this GPA
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Get the raw u32 value as u64 for use with guest memory operations
    pub fn as_u64(self) -> u64 {
        self.0.get() as u64
    }
}

impl Inspect for Gpa {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.get().inspect(req);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_gpa() {
        assert!(Gpa::new(0x1000).is_ok());
        assert!(Gpa::new(0xDEADBEEF).is_ok());
        assert!(Gpa::new(1).is_ok());
        assert_eq!(Gpa::new(0x1000).unwrap().get(), 0x1000);
    }

    #[test]
    fn test_invalid_gpa_zero() {
        assert_eq!(Gpa::new(0), Err(GpaError::InvalidValue(0)));
    }

    #[test]
    fn test_invalid_gpa_max() {
        assert_eq!(Gpa::new(u32::MAX), Err(GpaError::InvalidValue(u32::MAX)));
    }

    #[test]
    fn test_as_u64() {
        let gpa = Gpa::new(0x1234_5678).unwrap();
        assert_eq!(gpa.as_u64(), 0x1234_5678u64);
    }
}
