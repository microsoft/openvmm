// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for encoding various objects into ACPI Machine Language (AML).

use super::helpers::*;

/// A trait indicating that a particular type can be serialized
/// into a byte stream of ACPI Machine Language (AML).
pub trait AmlObject {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>);

    fn to_bytes(&self) -> Vec<u8> {
        let mut byte_stream = Vec::new();
        self.append_to_vec(&mut byte_stream);
        byte_stream
    }
}

/// A named AML object.
pub struct NamedObject {
    name: Vec<u8>,
    object: Vec<u8>,
}

impl NamedObject {
    /// Construct a new [`NamedObject`]
    pub fn new(name: &[u8], object: &impl AmlObject) -> Self {
        let encoded_name = encode_name(name);
        assert!(!encoded_name.is_empty());
        NamedObject {
            name: encoded_name,
            object: object.to_bytes(),
        }
    }
}

impl AmlObject for NamedObject {
    // A named object consists of the identifier (0x8) followed by the 4-byte name
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(8);
        byte_stream.extend_from_slice(&self.name);
        byte_stream.extend_from_slice(&self.object);
    }
}

pub struct GenericObject<T: AsRef<[u8]>>(pub T);

impl<T> AmlObject for GenericObject<T>
where
    T: AsRef<[u8]>,
{
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let buffer = self.0.as_ref();
        byte_stream.extend_from_slice(buffer);
    }
}

/// A named AML integer.
pub struct NamedInteger {
    data: NamedObject,
}

impl NamedInteger {
    /// Construct a new [`NamedInteger`]
    pub fn new(name: &[u8], value: u64) -> Self {
        Self {
            data: NamedObject::new(name, &GenericObject(encode_integer(value))),
        }
    }
}

impl AmlObject for NamedInteger {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        self.data.append_to_vec(byte_stream);
    }
}

/// A named AML string.
pub struct NamedString {
    data: NamedObject,
}

impl NamedString {
    /// Construct a new [`NamedString`]
    pub fn new(name: &[u8], value: &[u8]) -> Self {
        Self {
            data: NamedObject::new(name, &GenericObject(encode_string(value))),
        }
    }
}

impl AmlObject for NamedString {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        self.data.append_to_vec(byte_stream);
    }
}

/// A structured AML package.
pub struct StructuredPackage<T: AsRef<[u8]>> {
    pub elem_count: u8,
    pub elem_data: T,
}

impl<T> AmlObject for StructuredPackage<T>
where
    T: AsRef<[u8]>,
{
    // A package consists of the identifier (0x12), followed by the length (including itself),
    // the number of elements (depends on what package contents represent) and the content.
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let buffer = self.elem_data.as_ref();
        byte_stream.push(0x12);
        byte_stream.extend_from_slice(&encode_package_len(buffer.len() + 1));
        byte_stream.push(self.elem_count);
        byte_stream.extend_from_slice(buffer);
    }
}

pub struct Package<T: AsRef<[u8]>>(pub T);

impl<T> AmlObject for Package<T>
where
    T: AsRef<[u8]>,
{
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let buffer = self.0.as_ref();
        StructuredPackage {
            elem_count: buffer.len() as u8,
            elem_data: buffer,
        }
        .append_to_vec(byte_stream);
    }
}

pub struct Buffer<T: AsRef<[u8]>>(pub T);

impl<T> AmlObject for Buffer<T>
where
    T: AsRef<[u8]>,
{
    // A buffer consists of the identifier (0x11), followed by the length (including itself), followed by the size of
    // the buffer in bytes and then the content.
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let buffer = self.0.as_ref();
        let encoded_len = encode_integer(buffer.len().try_into().unwrap());
        byte_stream.push(0x11);
        byte_stream.extend_from_slice(&encode_package_len(buffer.len() + encoded_len.len()));
        byte_stream.extend_from_slice(&encoded_len);
        byte_stream.extend_from_slice(buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::test_helpers::verify_expected_bytes;

    #[test]
    fn verify_package() {
        let package = Package(vec![1, 2, 3, 4]);
        let bytes = package.to_bytes();
        verify_expected_bytes(&bytes, &[0x12, 6, 4, 1, 2, 3, 4]);
    }

    #[test]
    fn verify_large_package() {
        let package = Package(vec![0; 0xff]);
        let bytes = package.to_bytes();
        assert_eq!(bytes.len(), 0xff + 4);
        verify_expected_bytes(&bytes[..5], &[0x12, (1 << 6) | 2, 0x10, 0xff, 0]);
        assert_eq!(bytes[0xff + 3], 0);
    }

    #[test]
    fn verify_named_object() {
        let package = Package(vec![0]);
        let nobj = NamedObject::new(b"FOO", &package);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(&bytes, &[8, b'F', b'O', b'O', b'_', 0x12, 3, 1, 0]);
    }

    #[test]
    fn verify_named_integers() {
        let nobj = NamedInteger::new(b"FOO", 0);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(&bytes, &[8, b'F', b'O', b'O', b'_', 0]);

        let nobj = NamedInteger::new(b"FOO", 1);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(&bytes, &[8, b'F', b'O', b'O', b'_', 1]);

        let nobj = NamedInteger::new(b"FOO", 2);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(&bytes, &[8, b'F', b'O', b'O', b'_', 0xa, 2]);

        let nobj = NamedInteger::new(b"FOO", 0x100);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(&bytes, &[8, b'F', b'O', b'O', b'_', 0xb, 0x00, 0x01]);

        let nobj = NamedInteger::new(b"FOO", 0x10000);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[8, b'F', b'O', b'O', b'_', 0xc, 0x00, 0x00, 0x01, 0x00],
        );

        let nobj = NamedInteger::new(b"FOO", 0x100000000);
        let bytes = nobj.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                8, b'F', b'O', b'O', b'_', 0xe, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            ],
        );
    }

    #[test]
    fn verify_named_string() {
        let nobj = NamedString::new(b"FOO", b"hello");
        let bytes = nobj.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                8, b'F', b'O', b'O', b'_', 0xd, b'h', b'e', b'l', b'l', b'o', 0,
            ],
        );
    }
}
