// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protobuf encodings for Rust types.

pub use super::time::DurationEncoding;

use super::CopyExtend;
use super::DecodeError;
use super::DefaultEncoding;
use super::FieldDecode;
use super::FieldEncode;
use super::InplaceOption;
use super::MaybeUninit;
use super::MessageDecode;
use super::MessageEncode;
use super::PackedDecode;
use super::PackedEncode;
use super::Result;
use super::ResultExt;
use super::SerializedMessage;
use super::Wrapping;
use super::inplace_some;
use super::protobuf::FieldReader;
use super::protobuf::FieldSizer;
use super::protobuf::FieldWriter;
use super::protobuf::MessageReader;
use super::protobuf::MessageSizer;
use super::protobuf::MessageWriter;
use super::protobuf::PackedReader;
use super::protobuf::PackedSizer;
use super::protobuf::PackedWriter;
use super::protobuf::decode_with;
use crate::Error;
use crate::inplace_none;
use crate::protobuf::WireType;
use crate::protofile::DescribeField;
use crate::protofile::DescribeMessage;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::marker::PhantomData;
use core::num::NonZeroI8;
use core::num::NonZeroI16;
use core::num::NonZeroI32;
use core::num::NonZeroI64;
use core::num::NonZeroIsize;
use core::num::NonZeroU8;
use core::num::NonZeroU16;
use core::num::NonZeroU32;
use core::num::NonZeroU64;
use core::num::NonZeroUsize;
use core::time::Duration;
use thiserror::Error;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

/// An encoding derived by `mesh_derive` for `T`.
#[derive(Copy, Clone)]
pub struct DerivedEncoding<T>(PhantomData<fn(T) -> T>);

/// A field encoding for message encoding implementations.
///
/// Writes the message as a protobuf message field.
pub struct MessageEncoding<E>(E);

impl<E: DescribeMessage<T>, T> DescribeField<T> for MessageEncoding<E> {
    const FIELD_TYPE: FieldType<'static> = FieldType::message(|| E::DESCRIPTION);
}

impl<E: DescribeMessage<T>, T> DescribeMessage<T> for MessageEncoding<E> {
    const DESCRIPTION: MessageDescription<'static> = E::DESCRIPTION;
}

impl<T, R, E: MessageEncode<T, R>> MessageEncode<T, R> for MessageEncoding<E> {
    fn write_message(item: T, writer: MessageWriter<'_, '_, R>) {
        E::write_message(item, writer)
    }

    fn compute_message_size(item: &mut T, sizer: MessageSizer<'_>) {
        E::compute_message_size(item, sizer)
    }
}

impl<'a, T, R, E: MessageDecode<'a, T, R>> MessageDecode<'a, T, R> for MessageEncoding<E> {
    fn read_message(
        item: &mut InplaceOption<'_, T>,
        reader: MessageReader<'a, '_, R>,
    ) -> Result<()> {
        E::read_message(item, reader)
    }
}

impl<T, R, E: MessageEncode<T, R>> FieldEncode<T, R> for MessageEncoding<E> {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.message(|message| E::write_message(item, message));
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.message(|message| E::compute_message_size(item, message));
    }
}

impl<'a, T, R, E: MessageDecode<'a, T, R>> FieldDecode<'a, T, R> for MessageEncoding<E> {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'a, '_, R>) -> Result<()> {
        E::read_message(item, reader.message().typed::<Self>()?)
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        // Decode using an empty message.
        decode_with::<E, _, _>(item, &[], &mut [])
    }
}

/// A trait for converting a value to a u64 for use in varint encodings.
pub trait ToNumber: Copy {
    /// Convert to a `u64`.
    fn to_u64(self) -> u64;
    /// Convert to an `i64`.
    fn to_i64(self) -> i64;
}

impl<T: ToNumber> ToNumber for &T {
    fn to_u64(self) -> u64 {
        (*self).to_u64()
    }

    fn to_i64(self) -> i64 {
        (*self).to_i64()
    }
}

/// A trait for converting a value to from u64 for use in varint encodings.
///
/// N.B. The protobuf behavior is to truncate integers rather than fail on
/// overflow.
pub trait FromNumber: Copy {
    /// Convert from an `i64`.
    fn from_i64(v: i64) -> Result<Self>;
    /// Convert from a `u64`.
    fn from_u64(v: u64) -> Result<Self>;
}

macro_rules! number {
    ($($ty:ty)*) => {
        $(
        impl ToNumber for $ty {
            fn to_u64(self) -> u64 {
                self as u64
            }
            fn to_i64(self) -> i64 {
                self as i64
            }
        }

        impl FromNumber for $ty {
            fn from_u64(v: u64) -> Result<Self> {
                Ok(v as Self)
            }
            fn from_i64(v: i64) -> Result<Self> {
                Ok(v as Self)
            }
        }
        )*
    };
}

number!(usize u64 u32 u16 u8 isize i64 i32 i16 i8);

#[derive(Debug, Error)]
#[error("value must be non-zero")]
struct MustBeNonZero;

macro_rules! nonzero_number {
    ($($ty:ty)*) => {
        $(
        impl ToNumber for $ty {
            fn to_u64(self) -> u64 {
                self.get() as u64
            }
            fn to_i64(self) -> i64 {
                self.get() as i64
            }
        }

        impl FromNumber for $ty {
            fn from_u64(v: u64) -> Result<Self> {
                Self::new(v as _).ok_or(Error::new(MustBeNonZero))
            }
            fn from_i64(v: i64) -> Result<Self> {
                Self::new(v as _).ok_or(Error::new(MustBeNonZero))
            }
        }
        )*
    };
}

nonzero_number!(NonZeroUsize NonZeroU64 NonZeroU32 NonZeroU16 NonZeroU8 NonZeroIsize NonZeroI64 NonZeroI32 NonZeroI16 NonZeroI8);

impl<T: ToNumber> ToNumber for Wrapping<T> {
    fn to_u64(self) -> u64 {
        self.0.to_u64()
    }

    fn to_i64(self) -> i64 {
        self.0.to_i64()
    }
}

impl<T: FromNumber> FromNumber for Wrapping<T> {
    fn from_u64(v: u64) -> Result<Self> {
        Ok(Self(T::from_u64(v)?))
    }

    fn from_i64(v: i64) -> Result<Self> {
        Ok(Self(T::from_i64(v)?))
    }
}

impl ToNumber for bool {
    fn to_u64(self) -> u64 {
        self as u64
    }

    fn to_i64(self) -> i64 {
        self as i64
    }
}

impl FromNumber for bool {
    fn from_u64(v: u64) -> Result<Self> {
        Ok(v != 0)
    }

    fn from_i64(v: i64) -> Result<Self> {
        Ok(v != 0)
    }
}

impl ToNumber for char {
    fn to_u64(self) -> u64 {
        self as u64
    }

    fn to_i64(self) -> i64 {
        self as i64
    }
}

impl FromNumber for char {
    fn from_u64(v: u64) -> Result<Self> {
        v.try_into()
            .ok()
            .and_then(core::char::from_u32)
            .ok_or_else(|| DecodeError::InvalidUtf32.into())
    }

    fn from_i64(v: i64) -> Result<Self> {
        Self::from_u64(v as u64)
    }
}

/// A `FixedNumber` can be converted to a u32 or u64 type for use with fixed64
/// and fixed32 fields.
pub trait FixedNumber: Copy {
    /// The target type, `u32` or `u64`.
    type Type;
    /// Converts to the fixed type.
    fn to_fixed(self) -> Self::Type;
    /// Converts from the fixed type.
    fn from_fixed(_: Self::Type) -> Self;
}

impl FixedNumber for u32 {
    type Type = u32;
    fn to_fixed(self) -> u32 {
        self
    }

    fn from_fixed(v: u32) -> Self {
        v
    }
}

impl FixedNumber for i32 {
    type Type = u32;
    fn to_fixed(self) -> u32 {
        self as u32
    }

    fn from_fixed(v: u32) -> Self {
        v as Self
    }
}

impl FixedNumber for f32 {
    type Type = u32;
    fn to_fixed(self) -> u32 {
        self.to_bits()
    }

    fn from_fixed(v: u32) -> Self {
        Self::from_bits(v)
    }
}

impl FixedNumber for u64 {
    type Type = u64;
    fn to_fixed(self) -> u64 {
        self
    }

    fn from_fixed(v: u64) -> Self {
        v
    }
}

impl FixedNumber for i64 {
    type Type = u64;
    fn to_fixed(self) -> u64 {
        self as u64
    }

    fn from_fixed(v: u64) -> Self {
        v as Self
    }
}

impl FixedNumber for f64 {
    type Type = u64;
    fn to_fixed(self) -> u64 {
        self.to_bits()
    }

    fn from_fixed(v: u64) -> Self {
        Self::from_bits(v)
    }
}

// This encodes the address as a u32, with the first octet in the high 8 bits of
// the u32 and so on.
//
// In particular, this is not byte swapped to network byte order on
// little-endian platforms, as you might naively expect, since this would result
// in a different wire format across different architectures.
impl FixedNumber for core::net::Ipv4Addr {
    type Type = u32;

    fn to_fixed(self) -> u32 {
        self.into()
    }

    fn from_fixed(v: u32) -> Self {
        v.into()
    }
}

macro_rules! builtin_field_type {
    ($ty:ty, $encoding:ty, $name:expr) => {
        impl DescribeField<$ty> for $encoding {
            const FIELD_TYPE: FieldType<'static> = FieldType::builtin($name);
        }
    };
}

/// A field encoder for fixed64 fields.
pub struct Fixed64Field;

builtin_field_type!(u64, Fixed64Field, "fixed64");
builtin_field_type!(i64, Fixed64Field, "sfixed64");
builtin_field_type!(f64, Fixed64Field, "double");

impl<T: FixedNumber<Type = u64>, R> FieldEncode<T, R> for Fixed64Field {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.fixed64(item.to_fixed());
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.fixed64(item.to_fixed());
    }

    fn packed<'a>() -> Option<&'a dyn PackedEncode<T>>
    where
        T: 'a,
    {
        Some(&Self)
    }
}

impl<T: FixedNumber<Type = u64>> PackedEncode<T> for Fixed64Field {
    fn write_packed(&self, data: &[T], mut writer: PackedWriter<'_, '_>) {
        for v in data {
            writer.fixed64(v.to_fixed());
        }
    }

    fn compute_packed_size(&self, data: &[T], mut sizer: PackedSizer<'_>) {
        for _ in data {
            sizer.fixed64();
        }
    }

    fn must_pack(&self) -> bool {
        false
    }
}

impl<'a, T: FixedNumber<Type = u64>, R> FieldDecode<'a, T, R> for Fixed64Field {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        item.set(T::from_fixed(reader.fixed64()?));
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(T::from_fixed(0));
        Ok(())
    }

    fn packed<'p, C: CopyExtend<T>>() -> Option<&'p dyn PackedDecode<'a, T, C>>
    where
        T: 'p,
    {
        Some(&Self)
    }
}

impl<T: FixedNumber<Type = u64>, C: CopyExtend<T>> PackedDecode<'_, T, C> for Fixed64Field {
    fn read_packed(&self, data: &mut C, reader: &mut PackedReader<'_>) -> Result<()> {
        while let Some(v) = reader.fixed64()? {
            data.push(T::from_fixed(v));
        }
        Ok(())
    }

    fn must_pack(&self) -> bool {
        false
    }
}

/// A field encoder for fixed32 fields.
pub struct Fixed32Field;

builtin_field_type!(u32, Fixed32Field, "fixed32");
builtin_field_type!(i32, Fixed32Field, "sfixed32");
builtin_field_type!(f32, Fixed32Field, "float");

builtin_field_type!(core::net::Ipv4Addr, Fixed32Field, "fixed32");

impl<T: FixedNumber<Type = u32>, R> FieldEncode<T, R> for Fixed32Field {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.fixed32(item.to_fixed());
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.fixed32(item.to_fixed());
    }

    fn packed<'a>() -> Option<&'a dyn PackedEncode<T>>
    where
        T: 'a,
    {
        Some(&Self)
    }
}

impl<T: FixedNumber<Type = u32>> PackedEncode<T> for Fixed32Field {
    fn write_packed(&self, data: &[T], mut writer: PackedWriter<'_, '_>) {
        for v in data {
            writer.fixed32(v.to_fixed());
        }
    }

    fn compute_packed_size(&self, data: &[T], mut sizer: PackedSizer<'_>) {
        for _ in data {
            sizer.fixed32();
        }
    }

    fn must_pack(&self) -> bool {
        false
    }
}

impl<'a, T: FixedNumber<Type = u32>, R> FieldDecode<'a, T, R> for Fixed32Field {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        item.set(T::from_fixed(reader.fixed32()?));
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(T::from_fixed(0));
        Ok(())
    }

    fn packed<'p, C: CopyExtend<T>>() -> Option<&'p dyn PackedDecode<'a, T, C>>
    where
        T: 'p,
    {
        Some(&Self)
    }
}

impl<T: FixedNumber<Type = u32>, C: CopyExtend<T>> PackedDecode<'_, T, C> for Fixed32Field {
    fn read_packed(&self, data: &mut C, reader: &mut PackedReader<'_>) -> Result<()> {
        while let Some(v) = reader.fixed32()? {
            data.push(T::from_fixed(v));
        }
        Ok(())
    }

    fn must_pack(&self) -> bool {
        false
    }
}

/// A field encoder for u8.
///
/// This is separate from VarintField so that the packed format can be a byte
/// array instead of a varint array.
pub struct ByteField;

impl DescribeField<u8> for ByteField {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("uint32");
    const PACKED_TYPE: Option<&'static str> = Some("bytes");
}

impl<R> FieldEncode<u8, R> for ByteField {
    fn write_field(item: u8, writer: FieldWriter<'_, '_, R>) {
        writer.varint(item.into())
    }

    fn compute_field_size(item: &mut u8, sizer: FieldSizer<'_>) {
        sizer.varint((*item).into())
    }

    fn packed<'a>() -> Option<&'a dyn PackedEncode<u8>> {
        Some(&Self)
    }
}

impl PackedEncode<u8> for ByteField {
    fn write_packed(&self, data: &[u8], mut writer: PackedWriter<'_, '_>) {
        writer.bytes(data);
    }

    fn compute_packed_size(&self, data: &[u8], mut sizer: PackedSizer<'_>) {
        sizer.bytes(data.len());
    }

    fn must_pack(&self) -> bool {
        true
    }
}

impl<'a, R> FieldDecode<'a, u8, R> for ByteField {
    fn read_field(item: &mut InplaceOption<'_, u8>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        item.set(reader.varint()? as u8);
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, u8>) -> Result<()> {
        item.set(0);
        Ok(())
    }

    fn packed<'p, C: CopyExtend<u8>>() -> Option<&'p dyn PackedDecode<'a, u8, C>>
    where
        u8: 'p,
    {
        Some(&Self)
    }
}

impl<C: CopyExtend<u8>> PackedDecode<'_, u8, C> for ByteField {
    fn read_packed(&self, data: &mut C, reader: &mut PackedReader<'_>) -> Result<()> {
        data.extend_from_slice(reader.bytes());
        Ok(())
    }

    fn must_pack(&self) -> bool {
        true
    }
}

/// A field encoder for varint fields.
pub struct VarintField;

builtin_field_type!(u64, VarintField, "uint64");
builtin_field_type!(u32, VarintField, "uint32");
builtin_field_type!(u16, VarintField, "uint32");
builtin_field_type!(u8, VarintField, "uint32");
builtin_field_type!(usize, VarintField, "uint64");
builtin_field_type!(i64, VarintField, "int64");
builtin_field_type!(i32, VarintField, "int32");
builtin_field_type!(i16, VarintField, "int32");
builtin_field_type!(i8, VarintField, "int32");
builtin_field_type!(isize, VarintField, "int64");
builtin_field_type!(bool, VarintField, "bool");
builtin_field_type!(NonZeroU64, VarintField, "uint64");
builtin_field_type!(NonZeroU32, VarintField, "uint32");
builtin_field_type!(NonZeroU16, VarintField, "uint32");
builtin_field_type!(NonZeroU8, VarintField, "uint32");
builtin_field_type!(NonZeroUsize, VarintField, "uint64");
builtin_field_type!(NonZeroI64, VarintField, "int64");
builtin_field_type!(NonZeroI32, VarintField, "int32");
builtin_field_type!(NonZeroI16, VarintField, "int32");
builtin_field_type!(NonZeroI8, VarintField, "int32");
builtin_field_type!(NonZeroIsize, VarintField, "int64");

impl<T: ToNumber, R> FieldEncode<T, R> for VarintField {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.varint(item.to_u64())
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.varint(item.to_u64())
    }

    fn packed<'a>() -> Option<&'a dyn PackedEncode<T>>
    where
        T: 'a,
    {
        Some(&Self)
    }
}

impl<T: ToNumber> PackedEncode<T> for VarintField {
    fn write_packed(&self, data: &[T], mut writer: PackedWriter<'_, '_>) {
        for v in data {
            writer.varint(v.to_u64());
        }
    }

    fn compute_packed_size(&self, data: &[T], mut sizer: PackedSizer<'_>) {
        for v in data {
            sizer.varint(v.to_u64());
        }
    }

    fn must_pack(&self) -> bool {
        false
    }
}

impl<'a, T: FromNumber, R> FieldDecode<'a, T, R> for VarintField {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        item.set(T::from_u64(reader.varint()?)?);
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(T::from_u64(0)?);
        Ok(())
    }

    fn packed<'p, C: CopyExtend<T>>() -> Option<&'p dyn PackedDecode<'a, T, C>>
    where
        T: 'p,
    {
        Some(&Self)
    }
}

impl<T: FromNumber, C: CopyExtend<T>> PackedDecode<'_, T, C> for VarintField {
    fn read_packed(&self, data: &mut C, reader: &mut PackedReader<'_>) -> Result<()> {
        while let Some(v) = reader.varint()? {
            data.push(T::from_u64(v)?);
        }
        Ok(())
    }

    fn must_pack(&self) -> bool {
        false
    }
}

/// A field encoder for signed (zigzag encoded) varint fields.
///
/// This is used for protobuf sint32, etc. fields and not for int32, etc.
pub struct SignedVarintField;

builtin_field_type!(i64, SignedVarintField, "sint64");
builtin_field_type!(i32, SignedVarintField, "sint32");
builtin_field_type!(i16, SignedVarintField, "sint32");
builtin_field_type!(i8, SignedVarintField, "sint32");
builtin_field_type!(isize, SignedVarintField, "sint64");
builtin_field_type!(NonZeroI64, SignedVarintField, "sint64");
builtin_field_type!(NonZeroI32, SignedVarintField, "sint32");
builtin_field_type!(NonZeroI16, SignedVarintField, "sint32");
builtin_field_type!(NonZeroI8, SignedVarintField, "sint32");
builtin_field_type!(NonZeroIsize, SignedVarintField, "sint64");

impl<T: ToNumber, R> FieldEncode<T, R> for SignedVarintField {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.svarint(item.to_i64())
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.svarint(item.to_i64())
    }

    fn packed<'a>() -> Option<&'a dyn PackedEncode<T>>
    where
        T: 'a,
    {
        Some(&Self)
    }
}

impl<T: ToNumber> PackedEncode<T> for SignedVarintField {
    fn write_packed(&self, data: &[T], mut writer: PackedWriter<'_, '_>) {
        for v in data {
            writer.svarint(v.to_i64());
        }
    }

    fn compute_packed_size(&self, data: &[T], mut sizer: PackedSizer<'_>) {
        for v in data {
            sizer.svarint(v.to_i64());
        }
    }

    fn must_pack(&self) -> bool {
        false
    }
}

impl<'a, T: FromNumber, R> FieldDecode<'a, T, R> for SignedVarintField {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        item.set(T::from_i64(reader.svarint()?)?);
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(T::from_i64(0)?);
        Ok(())
    }

    fn packed<'p, C: CopyExtend<T>>() -> Option<&'p dyn PackedDecode<'a, T, C>>
    where
        T: 'p,
    {
        Some(&Self)
    }
}

impl<T: FromNumber, C: CopyExtend<T>> PackedDecode<'_, T, C> for SignedVarintField {
    fn read_packed(&self, data: &mut C, reader: &mut PackedReader<'_>) -> Result<()> {
        while let Some(v) = reader.svarint()? {
            data.push(T::from_i64(v)?);
        }
        Ok(())
    }

    fn must_pack(&self) -> bool {
        false
    }
}

/// A field encoder for u128.
///
/// Writes the value as a little-endian-encoded 16-byte byte array.
pub struct U128LittleEndianField;

builtin_field_type!(u128, U128LittleEndianField, "bytes");

impl<T: Copy + Into<u128>, R> FieldEncode<T, R> for U128LittleEndianField {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        let item = item.into();
        if item != 0 || writer.write_empty() {
            writer.bytes(&item.to_le_bytes());
        }
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        let item = (*item).into();
        if item != 0 || sizer.write_empty() {
            sizer.bytes(16);
        }
    }
}

impl<T: From<u128>, R> FieldDecode<'_, T, R> for U128LittleEndianField {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        item.set(
            u128::from_le_bytes(
                reader
                    .bytes()?
                    .try_into()
                    .map_err(|_| DecodeError::BadU128)?,
            )
            .into(),
        );
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(0.into());
        Ok(())
    }
}

/// A field encoder for an IPv6 address.
///
/// Writes the value in octet order as a 16-byte array.
pub struct Ipv6AddrField;

builtin_field_type!(core::net::Ipv6Addr, Ipv6AddrField, "bytes");

impl<R> FieldEncode<core::net::Ipv6Addr, R> for Ipv6AddrField {
    fn write_field(item: core::net::Ipv6Addr, writer: FieldWriter<'_, '_, R>) {
        if !item.is_unspecified() || writer.write_empty() {
            writer.bytes(&item.octets());
        }
    }

    fn compute_field_size(item: &mut core::net::Ipv6Addr, sizer: FieldSizer<'_>) {
        if !item.is_unspecified() || sizer.write_empty() {
            sizer.bytes(16);
        }
    }
}

impl<R> FieldDecode<'_, core::net::Ipv6Addr, R> for Ipv6AddrField {
    fn read_field(
        item: &mut InplaceOption<'_, core::net::Ipv6Addr>,
        reader: FieldReader<'_, '_, R>,
    ) -> Result<()> {
        let v: [u8; 16] = reader
            .bytes()?
            .try_into()
            .map_err(|_| DecodeError::BadIpv6)?;

        item.set(v.into());
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, core::net::Ipv6Addr>) -> Result<()> {
        item.set(core::net::Ipv6Addr::UNSPECIFIED);
        Ok(())
    }
}

/// A field encoder for byte streams.
pub struct BytesField;

impl<T: AsRef<[u8]>> DescribeField<T> for BytesField {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("bytes");
}

impl<T: AsRef<[u8]>, R> FieldEncode<T, R> for BytesField {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_ref())
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.bytes(item.as_ref().len())
    }
}

impl<'a, T: From<&'a [u8]> + Default, R> FieldDecode<'a, T, R> for BytesField {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'a, '_, R>) -> Result<()> {
        item.set(reader.bytes()?.into());
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(Default::default());
        Ok(())
    }
}

/// A field encoder for strings.
pub struct StringField;

impl<T: AsRef<str>> DescribeField<T> for StringField {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("string");
}

impl<T: AsRef<str>, R> FieldEncode<T, R> for StringField {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_ref().as_bytes())
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.bytes(item.as_ref().len())
    }
}

impl<'a, T: From<&'a str> + Default, R> FieldDecode<'a, T, R> for StringField {
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'a, '_, R>) -> Result<()> {
        item.set(
            core::str::from_utf8(reader.bytes()?)
                .map_err(DecodeError::InvalidUtf8)?
                .into(),
        );
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(Default::default());
        Ok(())
    }
}

/// An encoder for `Cow<'a, str>` or `Cow<'a, [u8]>` that creates
/// [`Cow::Borrowed`] on read.
pub struct BorrowedCowField;

impl DescribeField<Cow<'_, str>> for BorrowedCowField {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("string");
}

impl<'a, R> FieldEncode<Cow<'a, str>, R> for BorrowedCowField {
    fn write_field(item: Cow<'a, str>, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_bytes())
    }

    fn compute_field_size(item: &mut Cow<'a, str>, sizer: FieldSizer<'_>) {
        sizer.bytes(item.len())
    }
}

impl<'a, R> FieldDecode<'a, Cow<'a, str>, R> for BorrowedCowField {
    fn read_field(
        item: &mut InplaceOption<'_, Cow<'a, str>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        item.set(Cow::Borrowed(
            core::str::from_utf8(reader.bytes()?).map_err(DecodeError::InvalidUtf8)?,
        ));
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, Cow<'a, str>>) -> Result<()> {
        item.set(Cow::Borrowed(""));
        Ok(())
    }
}

impl<'a, R> FieldEncode<Cow<'a, [u8]>, R> for BorrowedCowField {
    fn write_field(item: Cow<'a, [u8]>, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_ref())
    }

    fn compute_field_size(item: &mut Cow<'a, [u8]>, sizer: FieldSizer<'_>) {
        sizer.bytes(item.len())
    }
}

impl<'a, R> FieldDecode<'a, Cow<'a, [u8]>, R> for BorrowedCowField {
    fn read_field(
        item: &mut InplaceOption<'_, Cow<'a, [u8]>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        let bytes = reader.bytes()?;
        let item = item.get_or_insert(Cow::Borrowed(&[]));
        if item.is_empty() {
            *item = Cow::Borrowed(bytes);
        } else {
            // Extend the bytes instead of replacing them to behave like protobuf's
            // bytes fields.
            item.to_mut().extend_from_slice(bytes);
        }
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, Cow<'a, [u8]>>) -> Result<()> {
        item.set(Cow::Borrowed(&[]));
        Ok(())
    }
}

/// An encoder for `Cow<'a, str>` or `Cow<'a, [u8]>` that creates [`Cow::Owned`]
/// on read.
pub struct OwningCowField;

impl DescribeField<Cow<'_, str>> for OwningCowField {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("string");
}

impl<'a, R> FieldEncode<Cow<'a, str>, R> for OwningCowField {
    fn write_field(item: Cow<'a, str>, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_bytes())
    }

    fn compute_field_size(item: &mut Cow<'a, str>, sizer: FieldSizer<'_>) {
        sizer.bytes(item.len())
    }
}

impl<'a, 'b, R> FieldDecode<'a, Cow<'b, str>, R> for OwningCowField {
    fn read_field(
        item: &mut InplaceOption<'_, Cow<'b, str>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        item.set(Cow::Owned(
            core::str::from_utf8(reader.bytes()?)
                .map_err(DecodeError::InvalidUtf8)?
                .into(),
        ));
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, Cow<'b, str>>) -> Result<()> {
        item.set(Cow::Borrowed(""));
        Ok(())
    }
}

impl<'a, R> FieldEncode<Cow<'a, [u8]>, R> for OwningCowField {
    fn write_field(item: Cow<'a, [u8]>, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_ref())
    }

    fn compute_field_size(item: &mut Cow<'a, [u8]>, sizer: FieldSizer<'_>) {
        sizer.bytes(item.len())
    }
}

impl<'a, 'b, R> FieldDecode<'a, Cow<'b, [u8]>, R> for OwningCowField {
    fn read_field(
        item: &mut InplaceOption<'_, Cow<'b, [u8]>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        let bytes = reader.bytes()?;
        item.get_or_insert(Cow::Borrowed(&[]))
            .to_mut()
            .extend_from_slice(bytes);
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, Cow<'b, [u8]>>) -> Result<()> {
        item.set(Cow::Borrowed(&[]));
        Ok(())
    }
}

/// A field encoder for `Option`.
pub struct OptionField<E>(E);

impl<T, E: DescribeField<T>> DescribeField<Option<T>> for OptionField<E> {
    const FIELD_TYPE: FieldType<'static> = {
        if E::FIELD_TYPE.is_sequence() {
            FieldType::tuple(&[E::FIELD_TYPE]).optional()
        } else {
            E::FIELD_TYPE.optional()
        }
    };
}

impl<T, R, E: FieldEncode<T, R>> FieldEncode<Option<T>, R> for OptionField<E> {
    fn write_field(item: Option<T>, writer: FieldWriter<'_, '_, R>) {
        if let Some(v) = item {
            E::write_field_in_sequence(v, &mut writer.sequence())
        }
    }

    fn compute_field_size(item: &mut Option<T>, sizer: FieldSizer<'_>) {
        if let Some(v) = item {
            E::compute_field_size_in_sequence(v, &mut sizer.sequence())
        }
    }

    fn wrap_in_sequence() -> bool {
        true
    }
}

impl<'a, T, R, E: FieldDecode<'a, T, R>> FieldDecode<'a, Option<T>, R> for OptionField<E> {
    fn read_field(
        item: &mut InplaceOption<'_, Option<T>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        let v = item.take().flatten();
        crate::inplace!(v);
        E::read_field_in_sequence(&mut v, reader)?;
        item.set(Some(
            v.take().expect("read_field should have set the value"),
        ));
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, Option<T>>) -> Result<()> {
        item.set(None);
        Ok(())
    }

    fn wrap_in_sequence() -> bool {
        true
    }
}

/// A field encoder for `Vec`.
pub struct VecField<E>(E);

impl<T, E: DescribeField<T>> DescribeField<Vec<T>> for VecField<E> {
    const FIELD_TYPE: FieldType<'static> = {
        if let Some(packed) = E::PACKED_TYPE {
            FieldType::builtin(packed)
        } else if E::FIELD_TYPE.is_sequence() {
            FieldType::tuple(&[E::FIELD_TYPE]).repeated()
        } else {
            E::FIELD_TYPE.repeated()
        }
    };
}

impl<T, R, E: FieldEncode<T, R>> FieldEncode<Vec<T>, R> for VecField<E> {
    fn write_field(item: Vec<T>, writer: FieldWriter<'_, '_, R>) {
        // Write a packed encoding if possible
        if let Some(packed_encode) = E::packed() {
            writer.packed(|packed| packed_encode.write_packed(item.as_slice(), packed));
        } else {
            let mut writer = writer.sequence();
            for item in item {
                E::write_field_in_sequence(item, &mut writer);
            }
        }
    }

    fn compute_field_size(item: &mut Vec<T>, sizer: FieldSizer<'_>) {
        if let Some(packed_encode) = E::packed() {
            sizer.packed(|packed| packed_encode.compute_packed_size(item.as_slice(), packed));
        } else {
            let mut sizer = sizer.sequence();
            for item in item {
                E::compute_field_size_in_sequence(item, &mut sizer);
            }
        }
    }

    fn wrap_in_sequence() -> bool {
        // `Vec<u8>` is encoded as a bytes value and not a repeated sequence.
        // Other packed sequences may still get a bytes value at runtime, but
        // they also support non-packed encodings and so must be wrapped when
        // they're nested in another sequence.
        let bytes = E::packed().is_some_and(|p| p.must_pack());
        !bytes
    }
}

impl<'a, T, R, E: FieldDecode<'a, T, R>> FieldDecode<'a, Vec<T>, R> for VecField<E> {
    fn read_field(
        item: &mut InplaceOption<'_, Vec<T>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        let vec = item.get_or_insert(Vec::new());

        // Try to read the packed encoding if possible/required.
        if let Some(packed_decode) = E::packed() {
            if packed_decode.must_pack() || reader.wire_type() == WireType::Variable {
                packed_decode.read_packed(vec, &mut reader.packed()?)?;
                return Ok(());
            }
        }

        // Construct the element in the vector in place.
        vec.reserve(1);
        // SAFETY: at least one value is allocated beyond len() due to the
        // reserve above and is safe to initialize.
        let v = unsafe { &mut *vec.as_mut_ptr().add(vec.len()).cast::<MaybeUninit<T>>() };
        let mut v = InplaceOption::uninit(v);
        E::read_field_in_sequence(&mut v, reader)?;
        assert!(v.forget(), "value should be constructed");
        // SAFETY: the element at vec.len() is now initialized.
        unsafe {
            vec.set_len(vec.len() + 1);
        }
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, Vec<T>>) -> Result<()> {
        item.set(Vec::new());
        Ok(())
    }

    fn wrap_in_sequence() -> bool {
        // `Vec<u8>` is encoded as a bytes value and not a repeated sequence.
        // Other packed sequences may still get a bytes value at runtime, but
        // they also support non-packed encodings and so must be wrapped when
        // they're nested in another sequence.
        let bytes = E::packed::<Vec<T>>().is_some_and(|p| p.must_pack());
        !bytes
    }
}

/// A field encoder for maps from `K` to `V`, using encoders `EK` and `EV`.
pub struct MapField<K, V, EK, EV>(EK, EV, PhantomData<fn(K, V) -> (K, V)>);

impl<T, K, V, EK: DescribeField<K>, EV: DescribeField<V>> DescribeField<T>
    for MapField<K, V, EK, EV>
{
    const FIELD_TYPE: FieldType<'static> = FieldType::map(&[EK::FIELD_TYPE, EV::FIELD_TYPE]);
}

/// Encoder for pairs.
///
/// This is separate from the standard tuple encoder so that we can specify the
/// precise encoder to use.
///
/// FUTURE: replat this on top of table-based encoding.
struct PairEncoder<E, F>(E, F);

impl<T, U, E, F, R> FieldEncode<(T, U), R> for PairEncoder<E, F>
where
    E: FieldEncode<T, R>,
    F: FieldEncode<U, R>,
{
    fn write_field(item: (T, U), writer: FieldWriter<'_, '_, R>) {
        writer.message(|mut writer| {
            E::write_field(item.0, writer.field(1));
            F::write_field(item.1, writer.field(2));
        })
    }

    fn compute_field_size(item: &mut (T, U), sizer: FieldSizer<'_>) {
        sizer.message(|mut sizer| {
            E::compute_field_size(&mut item.0, sizer.field(1));
            F::compute_field_size(&mut item.1, sizer.field(2));
        })
    }
}

impl<'a, T, U, E, F, R> FieldDecode<'a, (T, U), R> for PairEncoder<E, F>
where
    E: FieldDecode<'a, T, R>,
    F: FieldDecode<'a, U, R>,
{
    fn read_field(
        item: &mut InplaceOption<'_, (T, U)>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        inplace_none!(t);
        inplace_none!(u);
        for field in reader.message()? {
            let (number, reader) = field?;
            match number {
                1 => {
                    E::read_field(&mut t, reader)?;
                }
                2 => {
                    F::read_field(&mut u, reader)?;
                }
                _ => {}
            }
        }
        if t.is_none() {
            E::default_field(&mut t)?;
        }
        if u.is_none() {
            F::default_field(&mut u)?;
        }
        item.set((t.take().unwrap(), u.take().unwrap()));
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, (T, U)>) -> Result<()> {
        inplace_none!(t);
        E::default_field(&mut t)?;
        inplace_none!(u);
        F::default_field(&mut u)?;
        item.set((t.take().unwrap(), u.take().unwrap()));
        Ok(())
    }
}

impl<K, V, T, EK, EV, R> FieldEncode<T, R> for MapField<K, V, EK, EV>
where
    T: IntoIterator<Item = (K, V)>,
    for<'a> &'a mut T: IntoIterator<Item = (&'a K, &'a mut V)>,
    for<'a> EK: FieldEncode<&'a K, R>,
    EV: FieldEncode<V, R>,
{
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        let mut writer = writer.sequence();
        for (k, v) in item {
            PairEncoder::<EK, EV>::write_field_in_sequence((&k, v), &mut writer);
        }
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        let mut sizer = sizer.sequence();
        for (mut k, v) in item {
            sizer.field().message(|mut sizer| {
                EK::compute_field_size(&mut k, sizer.field(1));
                EV::compute_field_size(v, sizer.field(2));
            });
        }
    }

    fn wrap_in_sequence() -> bool {
        true
    }
}

impl<'a, K, V, T, EK, EV, R> FieldDecode<'a, T, R> for MapField<K, V, EK, EV>
where
    T: Default + Extend<(K, V)>,
    EK: FieldDecode<'a, K, R>,
    EV: FieldDecode<'a, V, R>,
{
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'a, '_, R>) -> Result<()> {
        inplace_none!(v);
        PairEncoder::<EK, EV>::read_field(&mut v, reader)?;
        item.get_or_insert_with(Default::default).extend(v.take());
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.get_or_insert_with(Default::default);
        Ok(())
    }

    fn wrap_in_sequence() -> bool {
        true
    }
}

/// A field encoder for fields that should be ignored.
pub struct IgnoreField;

impl<T: Default, R> FieldEncode<T, R> for IgnoreField {
    fn write_field(_item: T, writer: FieldWriter<'_, '_, R>) {
        // No-op if not in a sequence.
        writer.message(|_| ());
    }

    fn compute_field_size(_item: &mut T, sizer: FieldSizer<'_>) {
        // No-op if not in a sequence.
        sizer.message(|_| ());
    }
}

impl<T: Default, R> FieldDecode<'_, T, R> for IgnoreField {
    fn read_field(_item: &mut InplaceOption<'_, T>, _reader: FieldReader<'_, '_, R>) -> Result<()> {
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(Default::default());
        Ok(())
    }
}

/// A field and message encoder for fields that cannot be instantiated.
pub struct ImpossibleField;

impl<T> DescribeMessage<T> for ImpossibleField {
    const DESCRIPTION: MessageDescription<'static> = MessageDescription::External {
        name: "google.protobuf.Empty",
        import_path: "google/protobuf/empty.proto",
    };
}

impl<T, R> FieldEncode<T, R> for ImpossibleField {
    fn write_field(_item: T, _writer: FieldWriter<'_, '_, R>) {
        unreachable!()
    }

    fn compute_field_size(_item: &mut T, _sizer: FieldSizer<'_>) {
        unreachable!()
    }
}

impl<T, R> FieldDecode<'_, T, R> for ImpossibleField {
    fn read_field(_item: &mut InplaceOption<'_, T>, _reader: FieldReader<'_, '_, R>) -> Result<()> {
        Err(DecodeError::Unexpected.into())
    }

    fn default_field(_item: &mut InplaceOption<'_, T>) -> Result<()> {
        Err(DecodeError::Unexpected.into())
    }
}

impl<T, R> MessageEncode<T, R> for ImpossibleField {
    fn write_message(_item: T, _writer: MessageWriter<'_, '_, R>) {
        unreachable!()
    }

    fn compute_message_size(_item: &mut T, _sizer: MessageSizer<'_>) {
        unreachable!()
    }
}

impl<T, R> MessageDecode<'_, T, R> for ImpossibleField {
    fn read_message(
        _item: &mut InplaceOption<'_, T>,
        _reader: MessageReader<'_, '_, R>,
    ) -> Result<()> {
        Err(DecodeError::Unexpected.into())
    }
}

/// A field encoder for fixed-sized arrays.
pub struct ArrayField<E>(E);

impl<T, E: DescribeField<T>, const N: usize> DescribeField<[T; N]> for ArrayField<E> {
    const FIELD_TYPE: FieldType<'static> = {
        if let Some(packed) = E::PACKED_TYPE {
            FieldType::builtin(packed)
        } else if E::FIELD_TYPE.can_pack() {
            // BUGBUG: this is not protobuf compatible because protobuf allows
            // the unpacked encoding, which is not supported by `ArrayField`.
            // Change this once existing users are updated to use a different
            // type with a compatible encoding.
            E::FIELD_TYPE.repeated().annotate("packed repr only")
        } else {
            // Wrap in a message.
            FieldType::tuple(
                const {
                    if E::FIELD_TYPE.is_sequence() {
                        &[FieldType::tuple(&[E::FIELD_TYPE]).repeated()]
                    } else {
                        &[E::FIELD_TYPE.repeated()]
                    }
                },
            )
        }
    };
}

impl<T, R, E: FieldEncode<T, R>, const N: usize> FieldEncode<[T; N], R> for ArrayField<E> {
    fn write_field(item: [T; N], writer: FieldWriter<'_, '_, R>) {
        if let Some(packed_encode) = E::packed() {
            writer.packed(|packed| packed_encode.write_packed(&item, packed));
        } else {
            // The fixed array has to be wrapped in an object to support
            // reading.
            writer.message(|mut message| {
                let mut writer = message.field(1).sequence();
                for v in item {
                    E::write_field_in_sequence(v, &mut writer);
                }
            });
        }
    }

    fn compute_field_size(item: &mut [T; N], sizer: FieldSizer<'_>) {
        if let Some(packed_encode) = E::packed() {
            sizer.packed(|packed| packed_encode.compute_packed_size(item, packed));
        } else {
            sizer.message(|mut message| {
                let mut sizer = message.field(1).sequence();
                for v in item {
                    E::compute_field_size_in_sequence(v, &mut sizer);
                }
            });
        }
    }
}

impl<'a, T, R, E: FieldDecode<'a, T, R>, const N: usize> FieldDecode<'a, [T; N], R>
    for ArrayField<E>
{
    fn read_field(
        item: &mut InplaceOption<'_, [T; N]>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        if let Some(packed_decode) = E::packed() {
            let mut vec = Vec::with_capacity(N); // TODO: use an in-place mechanism
            packed_decode.read_packed(&mut vec, &mut reader.packed()?)?;
            item.set(
                vec.try_into()
                    .map_err(|_| DecodeError::BadPackedArrayLength)?,
            );
        } else {
            let vec = Vec::with_capacity(N); // TODO: use an in-place mechanism
            inplace_some!(vec);
            VecField::<E>::read_field_in_sequence(&mut vec, reader)?;
            item.set(
                vec.take()
                    .expect("should still be set")
                    .try_into()
                    .map_err(|_| DecodeError::BadArrayLength)?,
            );
        }
        Ok(())
    }

    fn default_field(_item: &mut InplaceOption<'_, [T; N]>) -> Result<()> {
        // TODO
        Err(DecodeError::MissingRequiredField.into())
    }
}

/// A field encoding for C-format structs.
///
/// Messages will be encoded as `bytes` fields whose size must exactly match the
/// struct's size.
///
/// Missing fields will be zero-initialized during message decode.
pub struct ZeroCopyEncoding;

impl<T> DescribeField<T> for ZeroCopyEncoding {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("bytes");
}

#[derive(Debug, Error)]
#[error("invalid byte size for type")]
struct InvalidZeroCopySize;

impl<T: zerocopy::IntoBytes + Immutable + KnownLayout, R> FieldEncode<T, R> for ZeroCopyEncoding {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.bytes(item.as_bytes());
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        sizer.bytes(item.as_bytes().len());
    }
}

impl<'a, T: zerocopy::FromBytes + Immutable + KnownLayout, R> FieldDecode<'a, T, R>
    for ZeroCopyEncoding
{
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'a, '_, R>) -> Result<()> {
        item.set(T::read_from_bytes(reader.bytes()?).map_err(|_| Error::new(InvalidZeroCopySize))?); // TODO: zerocopy: better use error here (https://github.com/microsoft/openvmm/issues/759)
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        item.set(T::new_zeroed());
        Ok(())
    }
}

/// A wrapper encoding for boxed data.
///
/// Messages and fields are handled as if they are not boxed, except that they
/// are never encoded in packed format.
pub struct BoxEncoding<E>(E);

impl<E: DescribeField<T>, T> DescribeField<T> for BoxEncoding<E> {
    const FIELD_TYPE: FieldType<'static> = E::FIELD_TYPE;
}

impl<E: DescribeMessage<T>, T> DescribeMessage<T> for BoxEncoding<E> {
    const DESCRIPTION: MessageDescription<'static> = E::DESCRIPTION;
}

impl<T, R, E: MessageEncode<T, R>> MessageEncode<Box<T>, R> for BoxEncoding<E> {
    fn write_message(item: Box<T>, writer: MessageWriter<'_, '_, R>) {
        E::write_message(*item, writer)
    }

    fn compute_message_size(item: &mut Box<T>, sizer: MessageSizer<'_>) {
        E::compute_message_size(&mut *item, sizer)
    }
}

impl<'a, T, R, E: MessageDecode<'a, T, R>> MessageDecode<'a, Box<T>, R> for BoxEncoding<E> {
    fn read_message(
        item: &mut InplaceOption<'_, Box<T>>,
        reader: MessageReader<'a, '_, R>,
    ) -> Result<()> {
        item.update_box(|item| E::read_message(item, reader))
    }
}

impl<T, R, E: FieldEncode<T, R>> FieldEncode<Box<T>, R> for BoxEncoding<E> {
    fn write_field(item: Box<T>, writer: FieldWriter<'_, '_, R>) {
        E::write_field(*item, writer)
    }

    fn compute_field_size(item: &mut Box<T>, sizer: FieldSizer<'_>) {
        E::compute_field_size(&mut *item, sizer)
    }

    fn wrap_in_sequence() -> bool {
        E::wrap_in_sequence()
    }
}

impl<'a, T, R, E: FieldDecode<'a, T, R>> FieldDecode<'a, Box<T>, R> for BoxEncoding<E> {
    fn read_field(
        item: &mut InplaceOption<'_, Box<T>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        item.update_box(|item| E::read_field(item, reader))
    }

    fn default_field(item: &mut InplaceOption<'_, Box<T>>) -> Result<()> {
        item.update_box(|item| E::default_field(item))
    }

    fn wrap_in_sequence() -> bool {
        E::wrap_in_sequence()
    }
}

/// A wrapper encoding for reference-counted data.
///
/// If the `Arc<T>` is the sole owner of T, then T is serialized as-is. If there
/// are other references, then the T is first cloned so that there is a single
/// instance.
///
/// Messages and fields are handled as if they are not reference counted, except
/// that they are never encoded in packed format.
pub struct ArcEncoding<E>(E);

impl<E: DescribeField<T>, T> DescribeField<T> for ArcEncoding<E> {
    const FIELD_TYPE: FieldType<'static> = E::FIELD_TYPE;
}

impl<E: DescribeMessage<T>, T> DescribeMessage<T> for ArcEncoding<E> {
    const DESCRIPTION: MessageDescription<'static> = E::DESCRIPTION;
}

impl<T: Clone, R, E: MessageEncode<T, R>> MessageEncode<Arc<T>, R> for ArcEncoding<E> {
    fn write_message(item: Arc<T>, writer: MessageWriter<'_, '_, R>) {
        E::write_message(
            Arc::try_unwrap(item)
                .ok()
                .expect("compute_message_size ensured single instance"),
            writer,
        )
    }

    fn compute_message_size(item: &mut Arc<T>, sizer: MessageSizer<'_>) {
        E::compute_message_size(Arc::make_mut(item), sizer)
    }
}

impl<'a, T: Clone, R, E: MessageDecode<'a, T, R>> MessageDecode<'a, Arc<T>, R> for ArcEncoding<E> {
    fn read_message(
        item: &mut InplaceOption<'_, Arc<T>>,
        reader: MessageReader<'a, '_, R>,
    ) -> Result<()> {
        item.update_arc(|item| E::read_message(item, reader))
    }
}

impl<T: Clone, R, E: FieldEncode<T, R>> FieldEncode<Arc<T>, R> for ArcEncoding<E> {
    fn write_field(item: Arc<T>, writer: FieldWriter<'_, '_, R>) {
        E::write_field(
            Arc::try_unwrap(item)
                .ok()
                .expect("compute_field_size ensured single instance"),
            writer,
        )
    }

    fn compute_field_size(item: &mut Arc<T>, sizer: FieldSizer<'_>) {
        E::compute_field_size(Arc::make_mut(item), sizer)
    }

    fn wrap_in_sequence() -> bool {
        E::wrap_in_sequence()
    }
}

impl<'a, T: Clone, R, E: FieldDecode<'a, T, R>> FieldDecode<'a, Arc<T>, R> for ArcEncoding<E> {
    fn read_field(
        item: &mut InplaceOption<'_, Arc<T>>,
        reader: FieldReader<'a, '_, R>,
    ) -> Result<()> {
        item.update_arc(|item| E::read_field(item, reader))
    }

    fn default_field(item: &mut InplaceOption<'_, Arc<T>>) -> Result<()> {
        item.update_arc(|item| E::default_field(item))
    }

    fn wrap_in_sequence() -> bool {
        E::wrap_in_sequence()
    }
}

macro_rules! default_encodings {
    ($($(#[$attr:meta])* $ty:ty: $mp:ty),* $(,)?) => {
        $(
            $(
                #[$attr]
            )*
            impl $crate::DefaultEncoding for $ty {
                type Encoding = $mp;
            }
        )*
    };
}

// Set the default encodings for common Rust types.
default_encodings! {
    u8: ByteField,
    u16: VarintField,
    u32: VarintField,
    u64: VarintField,
    u128: U128LittleEndianField,
    Wrapping<u64>: VarintField,
    usize: VarintField,
    bool: VarintField,
    char: VarintField,

    i8: SignedVarintField,
    i16: SignedVarintField,
    i32: SignedVarintField,
    i64: SignedVarintField,
    isize: SignedVarintField,

    f64: Fixed64Field,
    f32: Fixed32Field,

    NonZeroU8: VarintField,
    NonZeroU16: VarintField,
    NonZeroU32: VarintField,
    NonZeroU64: VarintField,
    NonZeroUsize: VarintField,
    NonZeroI8: SignedVarintField,
    NonZeroI16: SignedVarintField,
    NonZeroI32: SignedVarintField,
    NonZeroI64: SignedVarintField,
    NonZeroIsize: SignedVarintField,

    String: StringField,

    Duration: MessageEncoding<DurationEncoding>,

    Infallible: ImpossibleField,

    core::net::Ipv4Addr: Fixed32Field,
    core::net::Ipv6Addr: Ipv6AddrField,
}

impl DefaultEncoding for &str {
    type Encoding = StringField;
}

impl DefaultEncoding for &[u8] {
    type Encoding = BytesField;
}

impl DefaultEncoding for Cow<'_, str> {
    type Encoding = StringField;
}

impl<T: DefaultEncoding> DefaultEncoding for Option<T> {
    type Encoding = OptionField<T::Encoding>;
}

impl<T: DefaultEncoding> DefaultEncoding for Vec<T> {
    type Encoding = VecField<T::Encoding>;
}

#[cfg(feature = "std")]
impl<K: DefaultEncoding, V: DefaultEncoding> DefaultEncoding for std::collections::HashMap<K, V> {
    type Encoding = MapField<K, V, K::Encoding, V::Encoding>;
}

impl<K: DefaultEncoding, V: DefaultEncoding> DefaultEncoding for BTreeMap<K, V> {
    type Encoding = MapField<K, V, K::Encoding, V::Encoding>;
}

impl<T> DefaultEncoding for PhantomData<T> {
    type Encoding = IgnoreField;
}

impl<T: DefaultEncoding, const N: usize> DefaultEncoding for [T; N] {
    type Encoding = ArrayField<T::Encoding>;
}

impl<T: DefaultEncoding> DefaultEncoding for Box<T> {
    type Encoding = BoxEncoding<T::Encoding>;
}

impl<T: DefaultEncoding + Clone> DefaultEncoding for Arc<T> {
    type Encoding = ArcEncoding<T::Encoding>;
}

// Derive an encoding for `Result`.
#[derive(mesh_derive::Protobuf)]
#[mesh(impl_for = "::core::result::Result")]
#[expect(dead_code)]
enum ResultAsPayload<T, U> {
    #[mesh(transparent)]
    Ok(T),
    #[mesh(transparent)]
    Err(U),
}

// Derive an encoding for `Range`.
#[derive(mesh_derive::Protobuf)]
#[mesh(impl_for = "::core::ops::Range")]
#[expect(dead_code)]
struct RangeAsPayload<T> {
    start: T,
    end: T,
}

/// An encoder for [`SerializedMessage`].
pub struct SerializedMessageEncoder;

impl<R> MessageEncode<SerializedMessage<R>, R> for SerializedMessageEncoder {
    fn write_message(item: SerializedMessage<R>, mut writer: MessageWriter<'_, '_, R>) {
        writer.raw_message(&item.data, item.resources);
    }

    fn compute_message_size(item: &mut SerializedMessage<R>, mut sizer: MessageSizer<'_>) {
        sizer.raw_message(item.data.len(), item.resources.len() as u32);
    }
}

impl<R> MessageDecode<'_, SerializedMessage<R>, R> for SerializedMessageEncoder {
    fn read_message(
        item: &mut InplaceOption<'_, SerializedMessage<R>>,
        mut reader: MessageReader<'_, '_, R>,
    ) -> Result<()> {
        let resources = reader.take_resources();
        match item.as_mut() {
            Some(message) => {
                // Protobuf messages are merged just by adding in the extra
                // fields to the end. This works even with ports and resources
                // present.
                message.data.extend(reader.bytes());
                for resource in reader.take_resources() {
                    message.resources.push(resource?);
                }
            }
            None => {
                item.set(SerializedMessage {
                    data: reader.bytes().to_vec(),
                    resources: resources.collect::<Result<_>>()?,
                });
            }
        }
        Ok(())
    }
}

impl<R> DefaultEncoding for SerializedMessage<R> {
    type Encoding = MessageEncoding<SerializedMessageEncoder>;
}

/// A field encoder for types that can be converted to and from OS resource type
/// `T`.
pub struct ResourceField<T>(PhantomData<T>);

impl<T> Default for ResourceField<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T, U, R> FieldEncode<T, R> for ResourceField<U>
where
    T: Into<U>,
    U: Into<R>,
{
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        writer.resource(item.into().into());
    }

    fn compute_field_size(_item: &mut T, sizer: FieldSizer<'_>) {
        sizer.resource();
    }
}

impl<T, U, R> FieldDecode<'_, T, R> for ResourceField<U>
where
    T: From<U>,
    U: TryFrom<R>,
    U::Error: 'static + core::error::Error + Send + Sync,
{
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'_, '_, R>) -> Result<()> {
        let resource = T::from(reader.resource()?.try_into().map_err(Error::new)?);
        item.set(resource);
        Ok(())
    }

    fn default_field(_item: &mut InplaceOption<'_, T>) -> Result<()> {
        Err(DecodeError::MissingResource.into())
    }
}

/// Implements [`DefaultEncoding`] for OS resources.
///
/// The specified type must implement `From<R>` and `Into<R>`, where `R` is the
/// underlying OS resource type (such as `OwnedFd` or `OwnedHandle`).
#[macro_export]
macro_rules! os_resource {
    ($ty:ty, $resource_ty:ty) => {
        impl $crate::DefaultEncoding for $ty {
            type Encoding = $crate::encoding::ResourceField<$resource_ty>;
        }
    };
}

#[cfg(all(feature = "std", windows))]
mod windows {
    use std::os::windows::prelude::*;

    os_resource!(OwnedHandle, OwnedHandle);
    os_resource!(std::fs::File, OwnedHandle);

    os_resource!(OwnedSocket, OwnedSocket);
    os_resource!(std::net::TcpListener, OwnedSocket);
    os_resource!(std::net::TcpStream, OwnedSocket);
    os_resource!(std::net::UdpSocket, OwnedSocket);

    #[cfg(feature = "socket2")]
    os_resource!(socket2::Socket, OwnedSocket);
}

#[cfg(all(feature = "std", unix))]
mod unix {
    use std::os::unix::prelude::*;

    os_resource!(OwnedFd, OwnedFd);
    os_resource!(std::fs::File, OwnedFd);
    os_resource!(std::os::unix::net::UnixListener, OwnedFd);
    os_resource!(std::os::unix::net::UnixStream, OwnedFd);
    os_resource!(std::net::TcpListener, OwnedFd);
    os_resource!(std::net::TcpStream, OwnedFd);
    os_resource!(std::net::UdpSocket, OwnedFd);

    #[cfg(feature = "socket2")]
    os_resource!(socket2::Socket, OwnedFd);
}
