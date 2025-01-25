// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Table-based message encoding and decoding.
//!
//! Instead of directly implementing message encode and decode, this code
//! implements encoding and decoding by walking a table of field metadata
//! (offsets, field numbers, and function pointers).
//!
//! This has more compact code generation than the direct implementation.

pub mod decode;
pub mod encode;
mod tuple;

use crate::protofile::DescribeField;
use crate::protofile::DescribeMessage;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;

/// A message encoder/decoder that uses tables associated with the message type.
pub struct TableEncoder;

/// A table-encoded type that has a protobuf message description.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a stable protobuf type",
    label = "`{Self}` does not have a mesh package name",
    note = "consider adding `#[mesh(package = \"my.package.name\")]` to the type"
)]
pub trait DescribeTable {
    /// The protobuf message description for this type.
    const DESCRIPTION: MessageDescription<'static>;
}

impl<T: DescribeTable> DescribeMessage<T> for TableEncoder {
    const DESCRIPTION: MessageDescription<'static> = T::DESCRIPTION;
}

impl<T: DescribeTable> DescribeField<T> for TableEncoder {
    const FIELD_TYPE: FieldType<'static> = FieldType::message(|| T::DESCRIPTION);
}

/// Trait for types that can be encoded using [`TableEncoder`].
///
/// # Safety
///
/// The implementor must ensure that this metadata describes all fields within
/// `Self`, and that `OFFSETS` are the correct byte offsets to the fields.
pub unsafe trait StructMetadata {
    /// The field numbers for each field.
    const NUMBERS: &'static [u32]; // TODO: make a compact version of this, perhaps
    /// The byte offset to each field within the struct.
    const OFFSETS: &'static [usize]; // TODO: u32 (or even u16?)
}

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks)]
mod tests {
    use super::decode::ErasedDecoderEntry;
    use super::decode::StructDecodeMetadata;
    use super::encode::ErasedEncoderEntry;
    use super::encode::StructEncodeMetadata;
    use super::StructMetadata;
    use super::TableEncoder;
    use crate::encoding::StringField;
    use crate::encoding::VarintField;
    use crate::tests::as_expect_str;
    use crate::FieldDecode;
    use crate::FieldEncode;
    use core::mem::offset_of;
    use expect_test::expect;

    #[derive(PartialEq, Eq, Debug)]
    struct Foo<'a> {
        a: u32,
        b: u64,
        x: &'a str,
    }

    unsafe impl<'a> StructMetadata for Foo<'a> {
        const NUMBERS: &'static [u32] = &[1, 2, 3];
        const OFFSETS: &'static [usize] = &[
            offset_of!(Foo<'a>, a),
            offset_of!(Foo<'a>, b),
            offset_of!(Foo<'a>, x),
        ];
    }
    unsafe impl<'a, R> StructEncodeMetadata<R> for Foo<'a> {
        const ENCODERS: &'static [ErasedEncoderEntry] = &[
            <VarintField as FieldEncode<u32, R>>::ENTRY.erase(),
            <VarintField as FieldEncode<u64, R>>::ENTRY.erase(),
            <StringField as FieldEncode<&'a str, R>>::ENTRY.erase(),
        ];
    }
    unsafe impl<'de, R> StructDecodeMetadata<'de, R> for Foo<'de> {
        const DECODERS: &'static [ErasedDecoderEntry] = &[
            <VarintField as FieldDecode<'de, u32, R>>::ENTRY.erase(),
            <VarintField as FieldDecode<'de, u64, R>>::ENTRY.erase(),
            <StringField as FieldDecode<'de, &'de str, R>>::ENTRY.erase(),
        ];
    }
    impl crate::DefaultEncoding for Foo<'_> {
        type Encoding = TableEncoder;
    }

    #[test]
    fn test_derived_macro() {
        let data = crate::encode(Foo {
            a: 1,
            b: 2,
            x: "hi",
        });
        let expected = expect!([r#"
            1: varint 1
            2: varint 2
            3: string "hi"
            raw: 080110021a026869"#]);
        expected.assert_eq(&as_expect_str(&data));
        let foo = crate::decode::<Foo<'_>>(&data).unwrap();
        assert_eq!(foo.a, 1);
        assert_eq!(foo.b, 2);
        assert_eq!(foo.x, "hi");
    }
}
