// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Encoding support for protobuf `oneof` fields, which are derived from Rust
//! enums.

use crate::Error;
use crate::FieldDecode;
use crate::FieldEncode;
use crate::MessageDecode;
use crate::MessageEncode;
use crate::Result;
use crate::ResultExt;
use crate::inplace::InplaceOption;
use crate::protobuf::FieldReader;
use crate::protobuf::MessageReader;
use crate::protobuf::MessageSizer;
use crate::protobuf::MessageWriter;
use crate::protofile::DescribeField;
use crate::protofile::DescribeMessage;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;
use thiserror::Error;

/// An encoder type for `oneof` fields derived from Rust enums.
///
/// Encoding and decoding are implemented on types that implement
/// [`OneofEncode`] and [`OneofDecode`].
pub struct OneofEncoder;

#[derive(Debug, Error)]
#[error("missing enum variant")]
struct UnassignedEnum;

/// A trait for encoding a `oneof` field.
pub trait OneofEncode<R> {
    /// Write the variant to the writer.
    fn write_variant(self, writer: MessageWriter<'_, '_, R>);
    /// Compute the size of the variant.
    fn compute_variant_size(&mut self, sizer: MessageSizer<'_>);
}

/// A oneof-encoded type that has a protobuf message description.
pub trait DescribeOneof {
    /// The protobuf message description for this type.
    const DESCRIPTION: MessageDescription<'static>;
}

impl<T: DescribeOneof> DescribeMessage<T> for OneofEncoder {
    const DESCRIPTION: MessageDescription<'static> = T::DESCRIPTION;
}

impl<T: DescribeOneof> DescribeField<T> for OneofEncoder {
    const FIELD_TYPE: FieldType<'static> = FieldType::message(|| T::DESCRIPTION);
}

impl<T: OneofEncode<R>, R> MessageEncode<T, R> for OneofEncoder {
    fn write_message(item: T, writer: MessageWriter<'_, '_, R>) {
        item.write_variant(writer)
    }

    fn compute_message_size(item: &mut T, sizer: MessageSizer<'_>) {
        item.compute_variant_size(sizer)
    }
}

impl<T: OneofEncode<R>, R> FieldEncode<T, R> for OneofEncoder {
    fn write_field(item: T, writer: crate::protobuf::FieldWriter<'_, '_, R>) {
        writer.message(|writer| item.write_variant(writer))
    }

    fn compute_field_size(item: &mut T, sizer: crate::protobuf::FieldSizer<'_>) {
        sizer.message(|sizer| item.compute_variant_size(sizer))
    }
}

/// A trait for decoding a `oneof` field.
pub trait OneofDecode<'de, R>: Sized {
    /// Read the specified variant from the reader.
    fn read_variant(
        this: &mut InplaceOption<'_, Self>,
        number: u32,
        reader: FieldReader<'de, '_, R>,
    ) -> Result<()>;
}

impl<'de, T: OneofDecode<'de, R>, R> MessageDecode<'de, T, R> for OneofEncoder {
    fn read_message(
        item: &mut InplaceOption<'_, T>,
        reader: MessageReader<'de, '_, R>,
    ) -> Result<()> {
        for field in reader {
            let (n, field) = field.typed::<T>()?;
            T::read_variant(item, n, field)?;
        }
        if item.is_none() {
            return Err(Error::new(UnassignedEnum).typed::<T>());
        }
        Ok(())
    }
}

// Manually implement this instead of using `MessageEncoding` so that we can
// provide a simple implementation for `default_field`. This saves some
// generated code.
impl<'de, T: OneofDecode<'de, R>, R> FieldDecode<'de, T, R> for OneofEncoder {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'de, '_, R>) -> Result<()> {
        Self::read_message(item, reader.message()?)
    }

    fn default_field(_item: &mut InplaceOption<'_, T>) -> Result<()> {
        Err(Error::new(UnassignedEnum).typed::<T>())
    }
}
