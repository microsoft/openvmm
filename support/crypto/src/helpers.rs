// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Not all helpers are used by every backend
#![allow(dead_code)]

/// Remove leading zero bytes from a nonzero unsigned integer.
/// Returns `None` if the component is empty or consists entirely of zeros.
pub(crate) fn unpad_component(bytes: &[u8]) -> Option<Vec<u8>> {
    let start = bytes.iter().position(|&b| b != 0)?;
    Some(bytes[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::unpad_component;

    #[test]
    fn unpad_component_removes_only_leading_zeros() {
        for bytes in [
            vec![0x80, 0, 1, 0],
            vec![0, 0x80, 0, 1, 0],
            vec![0, 0, 0x80, 0, 1, 0],
        ] {
            assert_eq!(unpad_component(&bytes).unwrap(), [0x80, 0, 1, 0]);
        }
        assert_eq!(unpad_component(&[0, 0, 1]).unwrap(), [1]);
    }

    #[test]
    fn unpad_component_rejects_empty_or_zero() {
        for bytes in [vec![], vec![0], vec![0, 0]] {
            assert!(unpad_component(&bytes).is_none());
        }
    }
}
