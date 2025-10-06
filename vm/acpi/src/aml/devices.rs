// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for encoding devices into ACPI Machine Language (AML).

use super::helpers::*;
use super::objects::*;
use super::ops::*;

/// An AML Method
pub struct Method {
    pub name: [u8; 4],
    pub sync_level: u8,
    pub is_serialized: bool,
    pub arg_count: u8,
    operations: Vec<u8>,
}

impl Method {
    /// Constructs a new [`Method`].
    pub fn new(name: &[u8; 4]) -> Self {
        let local_name: [u8; 4] = [name[0], name[1], name[2], name[3]];
        Self {
            name: local_name,
            sync_level: 0,
            is_serialized: false,
            arg_count: 0,
            operations: vec![],
        }
    }

    /// Set the number of arguments the method accepts.
    pub fn set_arg_count(&mut self, arg_count: u8) {
        self.arg_count = arg_count;
    }

    /// Add an operation to the method body.
    pub fn add_operation(&mut self, op: &impl OperationObject) {
        op.append_to_vec(&mut self.operations);
    }
}

impl AmlObject for Method {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x14);
        byte_stream.extend_from_slice(&encode_package_len(5 + self.operations.len()));
        byte_stream.extend_from_slice(&self.name);
        byte_stream.push(
            self.sync_level << 4 | if self.is_serialized { 1 << 3 } else { 0 } | self.arg_count,
        );
        byte_stream.extend_from_slice(&self.operations);
    }
}

/// An AML Device
pub struct Device {
    name: Vec<u8>,
    objects: Vec<u8>,
}

impl Device {
    /// Construct a new [`Device`]
    pub fn new(name: &[u8]) -> Self {
        Self {
            name: encode_name(name),
            objects: vec![],
        }
    }

    /// Add an object to the body of the device.
    pub fn add_object(&mut self, obj: &impl AmlObject) {
        obj.append_to_vec(&mut self.objects);
    }
}

impl AmlObject for Device {
    // A device object consists of the extended identifier (0x5b 0x82) followed by the length, the name and then the
    // contained objects.
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x5b);
        byte_stream.push(0x82);
        let length = self.name.len() + self.objects.len();
        byte_stream.extend_from_slice(&encode_package_len(length));
        byte_stream.extend_from_slice(&self.name);
        byte_stream.extend_from_slice(&self.objects);
    }
}

/// An EISA identifier for a device.
pub struct EisaId(pub [u8; 7]);

impl AmlObject for EisaId {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let mut id: [u8; 4] = [0; 4];
        id[0] = (self.0[0] - b'@') << 2 | (self.0[1] - b'@') >> 3;
        id[1] = (self.0[1] & 7) << 5 | (self.0[2] - b'@');
        id[2] = char_to_hex(self.0[3]) << 4 | char_to_hex(self.0[4]);
        id[3] = char_to_hex(self.0[5]) << 4 | char_to_hex(self.0[6]);
        byte_stream.append(&mut encode_integer(u32::from_le_bytes(id) as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::test_helpers::verify_expected_bytes;

    #[test]
    fn verify_eisaid() {
        let eisa_id = EisaId(*b"PNP0003");
        let bytes = eisa_id.to_bytes();
        verify_expected_bytes(&bytes, &[0xc, 0x41, 0xd0, 0, 0x3]);
    }

    #[test]
    fn verify_method() {
        let op = AndOp {
            operand1: vec![b'S', b'T', b'A', b'_'],
            operand2: encode_integer(13),
            target_name: vec![b'S', b'T', b'A', b'_'],
        };
        let mut method = Method::new(b"_DIS");
        method.add_operation(&op);
        let bytes = method.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x14, 0x11, 0x5F, 0x44, 0x49, 0x53, 0x00, 0x7b, b'S', b'T', b'A', b'_', 0x0a, 0x0d,
                b'S', b'T', b'A', b'_',
            ],
        );
    }

    #[test]
    fn verify_device_object() {
        let package = Package(vec![0]);
        let nobj = NamedObject::new(b"FOO", &package);
        let mut device = Device::new(b"DEV");
        device.add_object(&nobj);
        let bytes = device.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x5b, 0x82, 14, b'D', b'E', b'V', b'_', 8, b'F', b'O', b'O', b'_', 0x12, 3, 1, 0,
            ],
        );
    }
}
