// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for encoding procedural operations into ACPI
//! Machine Language (AML).

/// An AML operation.
pub trait OperationObject {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>);

    fn to_bytes(&self) -> Vec<u8> {
        let mut byte_stream = Vec::new();
        self.append_to_vec(&mut byte_stream);
        byte_stream
    }
}

/// A bitwise AND AML operation.
pub struct AndOp {
    pub operand1: Vec<u8>,
    pub operand2: Vec<u8>,
    pub target_name: Vec<u8>,
}

impl OperationObject for AndOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x7b);
        byte_stream.extend_from_slice(&self.operand1);
        byte_stream.extend_from_slice(&self.operand2);
        byte_stream.extend_from_slice(&self.target_name);
    }
}

/// A bitwise OR AML operation.
pub struct OrOp {
    pub operand1: Vec<u8>,
    pub operand2: Vec<u8>,
    pub target_name: Vec<u8>,
}

impl OperationObject for OrOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x7d);
        byte_stream.extend_from_slice(&self.operand1);
        byte_stream.extend_from_slice(&self.operand2);
        byte_stream.extend_from_slice(&self.target_name);
    }
}

/// An AML operation to return from a procedure.
pub struct ReturnOp {
    pub result: Vec<u8>,
}

impl OperationObject for ReturnOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0xa4);
        byte_stream.extend_from_slice(&self.result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::encode_integer;
    use crate::aml::test_helpers::verify_expected_bytes;

    #[test]
    fn verify_and_operation() {
        let op = AndOp {
            operand1: vec![b'S', b'T', b'A', b'_'],
            operand2: encode_integer(13),
            target_name: vec![b'S', b'T', b'A', b'_'],
        };
        let bytes = op.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x7b, b'S', b'T', b'A', b'_', 0x0a, 0x0d, b'S', b'T', b'A', b'_',
            ],
        );
    }

    #[test]
    fn verify_or_operation() {
        let op = OrOp {
            operand1: vec![b'S', b'T', b'A', b'_'],
            operand2: encode_integer(13),
            target_name: vec![b'S', b'T', b'A', b'_'],
        };
        let bytes = op.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x7d, b'S', b'T', b'A', b'_', 0x0a, 0x0d, b'S', b'T', b'A', b'_',
            ],
        );
    }

    #[test]
    fn verify_return_operation() {
        let op = ReturnOp {
            result: vec![b'S', b'T', b'A', b'_'],
        };
        let bytes = op.to_bytes();
        verify_expected_bytes(&bytes, &[0xa4, b'S', b'T', b'A', b'_']);
    }
}
