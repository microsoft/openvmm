// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for encoding procedural operations into ACPI
//! Machine Language (AML).

use super::helpers::encode_package_len;

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

/// An AML If conditional operation.
pub struct IfOp {
    /// Pre-serialized predicate expression.
    pub predicate: Vec<u8>,
    /// Pre-serialized body operations.
    pub body: Vec<u8>,
}

impl OperationObject for IfOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0xa0); // IfOp
        let inner_len = self.predicate.len() + self.body.len();
        byte_stream.extend_from_slice(&encode_package_len(inner_len));
        byte_stream.extend_from_slice(&self.predicate);
        byte_stream.extend_from_slice(&self.body);
    }
}

/// An AML Else operation (must follow an IfOp).
pub struct ElseOp {
    /// Pre-serialized body operations.
    pub body: Vec<u8>,
}

impl OperationObject for ElseOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0xa1); // ElseOp
        byte_stream.extend_from_slice(&encode_package_len(self.body.len()));
        byte_stream.extend_from_slice(&self.body);
    }
}

/// An AML Store operation (Store source to destination).
pub struct StoreOp {
    /// Pre-serialized source operand.
    pub source: Vec<u8>,
    /// Pre-serialized destination (must be a SuperName — name, local, arg, etc.).
    pub destination: Vec<u8>,
}

impl OperationObject for StoreOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x70); // StoreOp
        byte_stream.extend_from_slice(&self.source);
        byte_stream.extend_from_slice(&self.destination);
    }
}

/// An AML LEqual comparison.
pub struct LEqualOp {
    /// Pre-serialized left operand.
    pub left: Vec<u8>,
    /// Pre-serialized right operand.
    pub right: Vec<u8>,
}

impl OperationObject for LEqualOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x93); // LEqualOp
        byte_stream.extend_from_slice(&self.left);
        byte_stream.extend_from_slice(&self.right);
    }
}

/// An AML CreateDWordField operation.
pub struct CreateDWordFieldOp {
    /// Pre-serialized source buffer.
    pub source_buffer: Vec<u8>,
    /// Pre-serialized byte index.
    pub byte_index: Vec<u8>,
    /// 4-byte field name.
    pub field_name: [u8; 4],
}

impl OperationObject for CreateDWordFieldOp {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x8a); // CreateDWordFieldOp
        byte_stream.extend_from_slice(&self.source_buffer);
        byte_stream.extend_from_slice(&self.byte_index);
        byte_stream.extend_from_slice(&self.field_name);
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
