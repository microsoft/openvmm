// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level functionality to serializing and deserializing mesh messages.
//!
//! Most code won't use this directly but will instead use the
//! [`Protobuf`](derive@Protobuf) derive macro.
//!
//! The encoding used is a superset of
//! [protobuf](https://developers.google.com/protocol-buffers/docs/encoding),
//! allowing protobuf clients and mesh to interoperate for a subset of types.
//! This includes an optional extension to allow external resources such as file
//! descriptors to be referenced by messages.
//!
//! This is used instead of serde in order to serialize objects by value. The
//! serde API takes values by reference, which makes it difficult to support
//! values, such as ports, handles, and file descriptors, whose ownership is to
//! be transferred to the target node.

#![warn(missing_docs)]
// UNSAFETY: Serialization and deserialization of structs directly.
#![expect(unsafe_code)]
#![warn(clippy::std_instead_of_alloc)]
#![warn(clippy::std_instead_of_core)]
#![warn(clippy::alloc_instead_of_core)]
#![no_std]

extern crate alloc;
extern crate self as mesh_protobuf;
#[cfg(feature = "std")]
extern crate std;

pub mod buffer;
mod encode_with;
pub mod encoding;
pub mod inplace;
pub mod message;
pub mod oneof;
#[cfg(feature = "prost")]
pub mod prost;
pub mod protobuf;
pub mod protofile;
pub mod table;
mod time;
pub mod transparent;

pub use encode_with::EncodeAs;
pub use mesh_derive::Protobuf;
pub use time::Timestamp;

use self::table::decode::DecoderEntry;
use self::table::encode::EncoderEntry;
use alloc::boxed::Box;
use alloc::fmt;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::num::Wrapping;
use inplace::InplaceOption;
use protofile::DescribeMessage;
use protofile::MessageDescription;
use protofile::TypeUrl;

/// Associates the default encoder/decoder type for converting an object to/from
/// protobuf format.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be encoded as a mesh message",
    note = "consider deriving the necessary trait on `{Self}` with one of:
    #[derive(MeshPayload)]
    #[derive(Protobuf)]",
    note = "alternatively, consider using an explicit encoder with #[mesh(encoding = \"MyEncoding\")]"
)]
pub trait DefaultEncoding {
    /// The encoding to use for the serialization.
    ///
    /// This type may or may not implement and of the four traits
    /// ([`MessageEncode`], [`MessageDecode`], [`FieldEncode`], [`FieldDecode`],
    /// since a type may only be serializable and not deserializable, for
    /// example.
    type Encoding;
}

/// Trait for types that can be encoded and decoded as a protobuf message.
pub trait Protobuf: DefaultEncoding<Encoding = <Self as Protobuf>::Encoding> + Sized {
    /// The default encoding for `Self`.
    type Encoding: MessageEncode<Self, NoResources>
        + for<'a> MessageDecode<'a, Self, NoResources>
        + FieldEncode<Self, NoResources>
        + for<'a> FieldDecode<'a, Self, NoResources>;
}

impl<T> Protobuf for T
where
    T: DefaultEncoding,
    T::Encoding: MessageEncode<T, NoResources>
        + for<'a> MessageDecode<'a, T, NoResources>
        + FieldEncode<T, NoResources>
        + for<'a> FieldDecode<'a, T, NoResources>,
{
    type Encoding = <T as DefaultEncoding>::Encoding;
}

/// Trait for types implementing [`Protobuf`] and having an associated protobuf
/// message description.
pub trait DescribedProtobuf: Protobuf {
    /// The message description.
    const DESCRIPTION: MessageDescription<'static>;
    /// The type URL for this message.
    const TYPE_URL: TypeUrl<'static> = Self::DESCRIPTION.type_url();
}

impl<T: DefaultEncoding + Protobuf> DescribedProtobuf for T
where
    <T as DefaultEncoding>::Encoding: DescribeMessage<T>,
{
    const DESCRIPTION: MessageDescription<'static> =
        <<T as DefaultEncoding>::Encoding as DescribeMessage<T>>::DESCRIPTION;
}

/// The `MessageEncode` trait provides a message encoder for type `T`.
///
/// `R` is the external resource type, which allows encoding objects with
/// non-protobuf resources such as file descriptors. Most implementors of this
/// trait will be generic over all `R`.
pub trait MessageEncode<T, R>: Sized {
    /// Writes `item` as a message.
    fn write_message(item: T, writer: protobuf::MessageWriter<'_, '_, R>);

    /// Computes the size of `item` as a message.
    ///
    /// Encoding will panic if the `write_message` call writes a different
    /// number of bytes than computed by this call.
    ///
    /// Takes a mut reference to allow mutating/stabilizing the value so that
    /// the subsequent call to `write_message` acts on the same value as this
    /// call.
    fn compute_message_size(item: &mut T, sizer: protobuf::MessageSizer<'_>);
}

/// The `MessageEncode` trait provides a message decoder for type `T`.
///
/// `R` is the external resource type, which allows decoding objects with
/// non-protobuf resources such as file descriptors. Most implementors of this
/// trait will be generic over all `R`.
pub trait MessageDecode<'a, T, R>: Sized {
    /// Reads a message into `item`.
    fn read_message(
        item: &mut InplaceOption<'_, T>,
        reader: protobuf::MessageReader<'a, '_, R>,
    ) -> Result<()>;
}

/// The `FieldEncode` trait provides a field encoder for type `T`.
///
/// `R` is the external resource type, which allows encoding objects with
/// non-protobuf resources such as file descriptors. Most implementors of this
/// trait will be generic over all `R`.
pub trait FieldEncode<T, R>: Sized {
    /// Writes `item` as a field.
    fn write_field(item: T, writer: protobuf::FieldWriter<'_, '_, R>);

    /// Computes the size of `item` as a field.
    ///
    /// Encoding will panic if the `write_field` call writes a different number
    /// of bytes than computed by this call.
    ///
    /// Takes a mut reference to allow mutating/stabilizing the value so that
    /// the subsequence call to `write_field` acts on the same value as this
    /// call.
    fn compute_field_size(item: &mut T, sizer: protobuf::FieldSizer<'_>);

    /// Returns the encoder for writing multiple instances of this field in a
    /// packed list, or `None` if there is no packed encoding for this type.
    fn packed<'a>() -> Option<&'a dyn PackedEncode<T>>
    where
        T: 'a,
    {
        None
    }

    /// Returns whether this field should be wrapped in a message when encoded
    /// nested in a sequence (such as a repeated field).
    ///
    /// This is necessary to avoid ambiguity between the repeated inner and
    /// outer values.
    fn wrap_in_sequence() -> bool {
        false
    }

    /// Writes this field as part of a sequence, wrapping it in a message if
    /// necessary.
    fn write_field_in_sequence(item: T, writer: &mut protobuf::SequenceWriter<'_, '_, R>) {
        if Self::wrap_in_sequence() {
            WrappedField::<Self>::write_field(item, writer.field())
        } else {
            Self::write_field(item, writer.field())
        }
    }

    /// Computes the size of this field as part of a sequence, including the
    /// size of a wrapping message.
    fn compute_field_size_in_sequence(item: &mut T, sizer: &mut protobuf::SequenceSizer<'_>) {
        if Self::wrap_in_sequence() {
            WrappedField::<Self>::compute_field_size(item, sizer.field())
        } else {
            Self::compute_field_size(item, sizer.field())
        }
    }

    /// The table encoder entry for this type, used in types from
    /// [`table::encode`].
    ///
    /// This should not be overridden by implementations.
    const ENTRY: EncoderEntry<T, R> = EncoderEntry::custom::<Self>();
}

/// Encoder methods for writing packed fields.
pub trait PackedEncode<T> {
    /// Writes a slice of data in packed format.
    fn write_packed(&self, data: &[T], writer: protobuf::PackedWriter<'_, '_>);

    /// Computes the size of the data in packed format.
    fn compute_packed_size(&self, data: &[T], sizer: protobuf::PackedSizer<'_>);

    /// If `true`, when this type is encoded as part of a sequence, it cannot be
    /// encoded with a normal repeated encoding and must be packed. This is used
    /// to determine if a nested repeated sequence needs to be wrapped in a
    /// message to avoid ambiguity.
    fn must_pack(&self) -> bool;
}

/// The `FieldEncode` trait provides a field decoder for type `T`.
///
/// `R` is the external resource type, which allows decoding objects with
/// non-protobuf resources such as file descriptors. Most implementors of this
/// trait will be generic over all `R`.
pub trait FieldDecode<'a, T, R>: Sized {
    /// Reads a field into `item`.
    fn read_field(
        item: &mut InplaceOption<'_, T>,
        reader: protobuf::FieldReader<'a, '_, R>,
    ) -> Result<()>;

    /// Instantiates `item` with its default value, if there is one.
    ///
    /// If an implementation returns `Ok(())`, then it must have set an item.
    /// Callers of this method may panic otherwise.
    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()>;

    /// Unless `packed()::must_pack()` is true, the sequence decoder must detect
    /// the encoding (packed or not) and call the appropriate method.
    fn packed<'p, C: CopyExtend<T>>() -> Option<&'p dyn PackedDecode<'a, T, C>>
    where
        T: 'p,
    {
        None
    }

    /// Returns whether this field is wrapped in a message when encoded nested
    /// in a sequence (such as a repeated field).
    fn wrap_in_sequence() -> bool {
        false
    }

    /// Reads this field that was encoded as part of a sequence, unwrapping it
    /// from a message if necessary.
    fn read_field_in_sequence(
        item: &mut InplaceOption<'_, T>,
        reader: protobuf::FieldReader<'a, '_, R>,
    ) -> Result<()> {
        if Self::wrap_in_sequence() {
            WrappedField::<Self>::read_field(item, reader)
        } else {
            Self::read_field(item, reader)
        }
    }

    /// The table decoder entry for this type, used in types from
    /// [`table::decode`].
    ///
    /// This should not be overridden by implementations.
    const ENTRY: DecoderEntry<'a, T, R> = DecoderEntry::custom::<Self>();
}

/// Methods for decoding a packed field.
pub trait PackedDecode<'a, T, C> {
    /// Reads from the packed format into `data`.
    fn read_packed(&self, data: &mut C, reader: &mut protobuf::PackedReader<'a>) -> Result<()>;

    /// If `true`, when this type is decoded as part of a sequence, it must be
    /// done with `read_packed` and not the field methods.
    fn must_pack(&self) -> bool;
}

/// Trait for collections that can be extended by a slice of `T: Copy`.
pub trait CopyExtend<T> {
    /// Pushes `item` onto the collection.
    fn push(&mut self, item: T)
    where
        T: Copy;

    /// Extends the collection by `items`.
    fn extend_from_slice(&mut self, items: &[T])
    where
        T: Copy;
}

impl<T> CopyExtend<T> for Vec<T> {
    fn push(&mut self, item: T)
    where
        T: Copy,
    {
        self.push(item);
    }

    fn extend_from_slice(&mut self, items: &[T])
    where
        T: Copy,
    {
        self.extend_from_slice(items);
    }
}

/// Encoder for a wrapper message used when a repeated field is directly nested
/// inside another repeated field.
struct WrappedField<E>(pub E);

impl<T, R, E: FieldEncode<T, R>> FieldEncode<T, R> for WrappedField<E> {
    fn write_field(item: T, writer: protobuf::FieldWriter<'_, '_, R>) {
        writer.message(|mut writer| E::write_field(item, writer.field(1)));
    }

    fn compute_field_size(item: &mut T, sizer: protobuf::FieldSizer<'_>) {
        sizer.message(|mut sizer| E::compute_field_size(item, sizer.field(1)));
    }
}

impl<'a, T, R, E: FieldDecode<'a, T, R>> FieldDecode<'a, T, R> for WrappedField<E> {
    fn read_field(
        item: &mut InplaceOption<'_, T>,
        reader: protobuf::FieldReader<'a, '_, R>,
    ) -> Result<()> {
        for field in reader.message()? {
            let (number, reader) = field?;
            if number == 1 {
                E::read_field(item, reader)?;
            }
        }
        if item.is_none() {
            E::default_field(item)?;
        }
        Ok(())
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        E::default_field(item)
    }
}

/// Encodes a message with its default encoding.
pub fn encode<T: DefaultEncoding>(message: T) -> Vec<u8>
where
    T::Encoding: MessageEncode<T, NoResources>,
{
    protobuf::Encoder::new(message).encode().0
}

/// Decodes a message with its default encoding.
pub fn decode<'a, T: DefaultEncoding>(data: &'a [u8]) -> Result<T>
where
    T::Encoding: MessageDecode<'a, T, NoResources>,
{
    inplace_none!(message: T);
    protobuf::decode_with::<T::Encoding, _, _>(&mut message, data, &mut [])?;
    Ok(message.take().expect("should be constructed"))
}

/// Merges message fields into an existing message.
pub fn merge<'a, T: DefaultEncoding>(value: T, data: &'a [u8]) -> Result<T>
where
    T::Encoding: MessageDecode<'a, T, NoResources>,
{
    inplace_some!(value);
    protobuf::decode_with::<T::Encoding, _, _>(&mut value, data, &mut [])?;
    Ok(value.take().expect("should be constructed"))
}

/// An empty resources type, used when an encoding does not require any external
/// resources (such as files or mesh channels).
pub enum NoResources {}

/// A serialized message, consisting of binary data and a list
/// of resources.
#[derive(Debug)]
pub struct SerializedMessage<R = NoResources> {
    /// The message data.
    pub data: Vec<u8>,
    /// The message resources.
    pub resources: Vec<R>,
}

impl<R> Default for SerializedMessage<R> {
    fn default() -> Self {
        Self {
            data: Default::default(),
            resources: Default::default(),
        }
    }
}

impl<R> SerializedMessage<R> {
    /// Serializes a message.
    pub fn from_message<T: DefaultEncoding>(t: T) -> Self
    where
        T::Encoding: MessageEncode<T, R>,
    {
        let (data, resources) = protobuf::Encoder::new(t).encode();
        Self { data, resources }
    }

    /// Deserializes a message.
    pub fn into_message<T: DefaultEncoding>(self) -> Result<T>
    where
        T::Encoding: for<'a> MessageDecode<'a, T, R>,
    {
        let (data, mut resources) = self.prep_decode();
        inplace_none!(message: T);
        protobuf::decode_with::<T::Encoding, _, _>(&mut message, &data, &mut resources)?;
        Ok(message.take().expect("should be constructed"))
    }

    fn prep_decode(self) -> (Vec<u8>, Vec<Option<R>>) {
        let data = self.data;
        let resources = self.resources.into_iter().map(Some).collect();
        (data, resources)
    }
}

/// A decoding error.
#[derive(Debug)]
pub struct Error(Box<ErrorInner>);

#[derive(Debug)]
struct ErrorInner {
    types: Vec<&'static str>,
    err: Box<dyn core::error::Error + Send + Sync>,
}

/// The cause of a decoding error.
#[derive(Debug, thiserror::Error)]
enum DecodeError {
    #[error("expected a message")]
    ExpectedMessage,
    #[error("expected a resource")]
    ExpectedResource,
    #[error("expected a varint")]
    ExpectedVarInt,
    #[error("expected a fixed64")]
    ExpectedFixed64,
    #[error("expected a fixed32")]
    ExpectedFixed32,
    #[error("expected a byte array")]
    ExpectedByteArray,
    #[error("field cannot exist")]
    Unexpected,

    #[error("eof parsing a varint")]
    EofVarInt,
    #[error("eof parsing a fixed64")]
    EofFixed64,
    #[error("eof parsing a fixed32")]
    EofFixed32,
    #[error("eof parsing a byte array")]
    EofByteArray,

    #[error("varint too big")]
    VarIntTooBig,

    #[error("missing resource")]
    MissingResource,
    #[error("invalid resource range")]
    InvalidResourceRange,

    #[error("unknown wire type {0}")]
    UnknownWireType(u32),

    #[error("invalid UTF-32 character")]
    InvalidUtf32,
    #[error("wrong buffer size for u128")]
    BadU128,
    #[error("invalid UTF-8 string")]
    InvalidUtf8(#[source] core::str::Utf8Error),
    #[error("missing required field")]
    MissingRequiredField,
    #[error("wrong packed array length")]
    BadPackedArrayLength,
    #[error("wrong array length")]
    BadArrayLength,

    #[error("duration out of range")]
    DurationRange,
}

impl Error {
    /// Creates a new error.
    pub fn new(error: impl Into<Box<dyn core::error::Error + Send + Sync>>) -> Self {
        Self(Box::new(ErrorInner {
            types: Vec::new(),
            err: error.into(),
        }))
    }

    /// Returns a new error with an additional type context added.
    pub fn typed<T>(mut self) -> Self {
        self.0.types.push(core::any::type_name::<T>());
        self
    }
}

impl From<DecodeError> for Error {
    fn from(kind: DecodeError) -> Self {
        Self(Box::new(ErrorInner {
            types: Vec::new(),
            err: kind.into(),
        }))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(&ty) = self.0.types.last() {
            write!(f, "decoding failed in {}", ty)?;
            for &ty in self.0.types.iter().rev().skip(1) {
                write!(f, "/{}", ty)?;
            }
            Ok(())
        } else {
            write!(f, "decoding failed")
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(self.0.err.as_ref())
    }
}

/// Extension trait to add type context to [`Error`].
pub trait ResultExt {
    /// Add type `T`'s name to the error.
    fn typed<T>(self) -> Self;
}

impl<T> ResultExt for Result<T> {
    fn typed<U>(self) -> Self {
        self.map_err(Error::typed::<U>)
    }
}

/// A decoding result.
pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    extern crate std;

    use super::encode;
    use super::SerializedMessage;
    use crate::decode;
    use crate::encoding::BorrowedCowField;
    use crate::encoding::OwningCowField;
    use crate::encoding::VecField;
    use crate::protobuf::read_varint;
    use crate::DecodeError;
    use crate::FieldDecode;
    use crate::FieldEncode;
    use crate::NoResources;
    use alloc::borrow::Cow;
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use core::convert::Infallible;
    use core::error::Error;
    use core::fmt::Write;
    use core::num::NonZeroU32;
    use core::time::Duration;
    use expect_test::expect;
    use expect_test::Expect;
    use mesh_derive::Protobuf;
    use std::prelude::rust_2021::*;
    use std::println;

    pub(crate) fn as_expect_str(v: &[u8]) -> String {
        let cooked = parsed_expect_str(v).unwrap_or_else(|e| alloc::format!("PARSE ERROR: {e}\n"));
        let raw = hex_str(v);
        alloc::format!("{cooked}raw: {raw}")
    }

    fn hex_str(v: &[u8]) -> String {
        v.iter()
            .map(|x| alloc::format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn parsed_expect_str(mut v: &[u8]) -> Result<String, crate::Error> {
        let mut s = String::new();
        while !v.is_empty() {
            let key = read_varint(&mut v)?;
            let wire_type = (key & 7) as u32;
            let field_number = (key >> 3) as u32;
            write!(s, "{field_number}: ").ok();
            match wire_type {
                0 => {
                    let n = read_varint(&mut v)?;
                    writeln!(s, "varint {n}").ok();
                }
                1 => {
                    let n = u64::from_le_bytes(
                        v.get(..8)
                            .ok_or(DecodeError::EofFixed64)?
                            .try_into()
                            .unwrap(),
                    );
                    writeln!(s, "fixed64 {n}").ok();
                    v = &v[8..];
                }
                2 => {
                    let len = read_varint(&mut v)? as usize;
                    let data = v.get(..len).ok_or(DecodeError::EofByteArray)?;
                    if !data.is_empty() && data.iter().all(|&x| matches!(x, 0x20..=0x7e)) {
                        let data = core::str::from_utf8(data).unwrap();
                        writeln!(s, "string \"{data}\"").ok();
                    } else {
                        let data = hex_str(data);
                        writeln!(s, "bytes <{data}>").ok();
                    }
                    v = &v[len..];
                }
                5 => {
                    let n = u32::from_le_bytes(
                        v.get(..4)
                            .ok_or(DecodeError::EofFixed32)?
                            .try_into()
                            .unwrap(),
                    );
                    writeln!(s, "fixed32 {n}").ok();
                    v = &v[4..];
                }
                n => Err(DecodeError::UnknownWireType(n))?,
            }
        }
        if s.is_empty() {
            writeln!(s, "empty").ok();
        }
        Ok(s)
    }

    /// Asserts that a type roundtrips through encoding and decoding without
    /// verifying the actual contents. This is useful for types that have
    /// a non-deterministic order (e.g., `HashMap`).
    #[track_caller]
    fn assert_roundtrips_nondeterministic<T>(t: T) -> Vec<u8>
    where
        T: crate::DefaultEncoding + Clone + Eq + core::fmt::Debug,
        T::Encoding:
            crate::MessageEncode<T, NoResources> + for<'a> crate::MessageDecode<'a, T, NoResources>,
    {
        println!("{t:?}");
        let v = encode(t.clone());
        println!("{v:x?}");
        let t2 = decode::<T>(&v).unwrap();
        assert_eq!(t, t2);
        v
    }

    #[track_caller]
    fn assert_roundtrips<T>(t: T, expect: Expect)
    where
        T: crate::DefaultEncoding + Clone + Eq + core::fmt::Debug,
        T::Encoding:
            crate::MessageEncode<T, NoResources> + for<'a> crate::MessageDecode<'a, T, NoResources>,
    {
        let v = assert_roundtrips_nondeterministic(t);
        expect.assert_eq(&as_expect_str(&v));
    }

    #[track_caller]
    fn assert_field_roundtrips<T>(t: T, expect: Expect)
    where
        T: crate::DefaultEncoding + Clone + Eq + core::fmt::Debug,
        T::Encoding: FieldEncode<T, NoResources> + for<'a> FieldDecode<'a, T, NoResources>,
    {
        assert_roundtrips((t,), expect);
    }

    #[test]
    fn test_field() {
        assert_field_roundtrips(
            5u32,
            expect!([r#"
                1: varint 5
                raw: 0805"#]),
        );
        assert_field_roundtrips(
            true,
            expect!([r#"
                1: varint 1
                raw: 0801"#]),
        );
        assert_field_roundtrips(
            "hi".to_string(),
            expect!([r#"
                1: string "hi"
                raw: 0a026869"#]),
        );
        assert_field_roundtrips(
            5u128,
            expect!([r#"
                1: bytes <05000000000000000000000000000000>
                raw: 0a1005000000000000000000000000000000"#]),
        );
        assert_field_roundtrips(
            (),
            expect!([r#"
                empty
                raw: "#]),
        );
        assert_field_roundtrips(
            (1, 2),
            expect!([r#"
                1: bytes <08021004>
                raw: 0a0408021004"#]),
        );
        assert_field_roundtrips(
            ("foo".to_string(), "bar".to_string()),
            expect!([r#"
                1: bytes <0a03666f6f1203626172>
                raw: 0a0a0a03666f6f1203626172"#]),
        );
        assert_field_roundtrips(
            [1, 2, 3],
            expect!([r#"
                1: bytes <020406>
                raw: 0a03020406"#]),
        );
        assert_field_roundtrips(
            ["abc".to_string(), "def".to_string()],
            expect!([r#"
                1: bytes <0a036162630a03646566>
                raw: 0a0a0a036162630a03646566"#]),
        );
        assert_field_roundtrips(
            Some(5),
            expect!([r#"
                1: varint 10
                raw: 080a"#]),
        );
        assert_field_roundtrips(
            Option::<u32>::None,
            expect!([r#"
                empty
                raw: "#]),
        );
        assert_field_roundtrips(
            vec![1, 2, 3],
            expect!([r#"
                1: bytes <020406>
                raw: 0a03020406"#]),
        );
        assert_field_roundtrips(
            vec!["abc".to_string(), "def".to_string()],
            expect!([r#"
                1: string "abc"
                1: string "def"
                raw: 0a036162630a03646566"#]),
        );
        assert_field_roundtrips(
            Some(Some(true)),
            expect!([r#"
                1: bytes <0801>
                raw: 0a020801"#]),
        );
        assert_field_roundtrips(
            Some(Option::<bool>::None),
            expect!([r#"
                1: bytes <>
                raw: 0a00"#]),
        );
        assert_field_roundtrips(
            vec![None, Some(true), None],
            expect!([r#"
                1: bytes <>
                1: bytes <0801>
                1: bytes <>
                raw: 0a000a0208010a00"#]),
        );
        #[cfg(feature = "std")]
        assert_roundtrips_nondeterministic((std::collections::HashMap::from_iter([
            (5u32, 6u32),
            (4, 2),
        ]),));
        assert_field_roundtrips(
            BTreeMap::from_iter([("hi".to_owned(), 6u32), ("hmm".to_owned(), 2)]),
            expect!([r#"
                1: bytes <0a0268691006>
                1: bytes <0a03686d6d1002>
                raw: 0a060a02686910060a070a03686d6d1002"#]),
        );
    }

    #[test]
    fn test_nonzero() {
        assert_field_roundtrips(
            NonZeroU32::new(1).unwrap(),
            expect!([r#"
                1: varint 1
                raw: 0801"#]),
        );
        assert_eq!(encode((5u32,)), encode((NonZeroU32::new(5).unwrap(),)));
        assert_eq!(
            decode::<(NonZeroU32,)>(&encode((Some(0u32),)))
                .unwrap_err()
                .source()
                .unwrap()
                .to_string(),
            "value must be non-zero"
        )
    }

    #[test]
    fn test_derive_struct() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Foo {
            x: u32,
            y: u32,
            z: String,
            w: Option<bool>,
        }

        let foo = Foo {
            x: 5,
            y: 104824,
            z: "alphabet".to_owned(),
            w: None,
        };
        assert_roundtrips(
            foo,
            expect!([r#"
                1: varint 5
                2: varint 104824
                3: string "alphabet"
                raw: 080510f8b2061a08616c706861626574"#]),
        );
    }

    #[test]
    fn test_nested_derive_struct() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Foo {
            x: u32,
            y: u32,
            b: Option<Bar>,
        }

        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Bar {
            a: Option<bool>,
            b: u32,
        }

        let foo = Foo {
            x: 5,
            y: 104824,
            b: Some(Bar {
                a: Some(true),
                b: 5,
            }),
        };
        assert_roundtrips(
            foo,
            expect!([r#"
                1: varint 5
                2: varint 104824
                3: bytes <08011005>
                raw: 080510f8b2061a0408011005"#]),
        );
    }

    #[test]
    fn test_derive_enum() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        enum Foo {
            A,
            B(u32, String),
            C { x: bool, y: u32 },
        }

        assert_roundtrips(
            Foo::A,
            expect!([r#"
                1: bytes <>
                raw: 0a00"#]),
        );
        assert_roundtrips(
            Foo::B(12, "hi".to_owned()),
            expect!([r#"
                2: bytes <080c12026869>
                raw: 1206080c12026869"#]),
        );
        assert_roundtrips(
            Foo::C { x: true, y: 0 },
            expect!([r#"
                3: bytes <0801>
                raw: 1a020801"#]),
        );
        assert_roundtrips(
            Foo::C { x: false, y: 0 },
            expect!([r#"
                3: bytes <>
                raw: 1a00"#]),
        );
    }

    #[test]
    fn test_vec() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Foo {
            u32: Vec<u32>,
            u8: Vec<u8>,
            vec_no_pack: Vec<(u32,)>,
            vec_of_vec8: Vec<Vec<u8>>,
            vec_of_vec32: Vec<Vec<u32>>,
            vec_of_vec_no_pack: Vec<Vec<(u32,)>>,
        }

        let foo = Foo {
            u32: vec![1, 2, 3, 4, 5],
            u8: b"abcdefg".to_vec(),
            vec_no_pack: vec![(1,), (2,), (3,), (4,), (5,)],
            vec_of_vec8: vec![b"abc".to_vec(), b"def".to_vec()],
            vec_of_vec32: vec![vec![1, 2, 3], vec![4, 5, 6]],
            vec_of_vec_no_pack: vec![vec![(64,), (65,)], vec![(66,), (67,)]],
        };
        assert_roundtrips(
            foo,
            expect!([r#"
                1: bytes <0102030405>
                2: string "abcdefg"
                3: bytes <0801>
                3: bytes <0802>
                3: bytes <0803>
                3: bytes <0804>
                3: bytes <0805>
                4: string "abc"
                4: string "def"
                5: bytes <0a03010203>
                5: bytes <0a03040506>
                6: bytes <0a0208400a020841>
                6: bytes <0a0208420a020843>
                raw: 0a0501020304051207616263646566671a0208011a0208021a0208031a0208041a020805220361626322036465662a050a030102032a050a0304050632080a0208400a02084132080a0208420a020843"#]),
        );
    }

    struct NoPackU32;

    impl<R> FieldEncode<u32, R> for NoPackU32 {
        fn write_field(item: u32, writer: crate::protobuf::FieldWriter<'_, '_, R>) {
            writer.varint(item.into())
        }

        fn compute_field_size(item: &mut u32, sizer: crate::protobuf::FieldSizer<'_>) {
            sizer.varint((*item).into())
        }
    }

    impl<R> FieldDecode<'_, u32, R> for NoPackU32 {
        fn read_field(
            _item: &mut crate::inplace::InplaceOption<'_, u32>,
            _reader: crate::protobuf::FieldReader<'_, '_, R>,
        ) -> crate::Result<()> {
            unimplemented!()
        }

        fn default_field(_item: &mut crate::inplace::InplaceOption<'_, u32>) -> crate::Result<()> {
            unimplemented!()
        }
    }

    #[test]
    fn test_vec_alt() {
        {
            #[derive(Protobuf, Clone)]
            struct NoPack {
                #[mesh(encoding = "VecField<NoPackU32>")]
                v: Vec<u32>,
            }

            #[derive(Protobuf)]
            struct CanPack {
                v: Vec<u32>,
            }

            let no_pack = NoPack { v: vec![1, 2, 3] };
            let v = encode(no_pack.clone());
            println!("{v:x?}");
            let can_pack = decode::<CanPack>(&v).unwrap();
            assert_eq!(no_pack.v, can_pack.v);
        }

        {
            #[derive(Protobuf, Clone)]
            struct NoPackNest {
                #[mesh(encoding = "VecField<VecField<NoPackU32>>")]
                v: Vec<Vec<u32>>,
            }

            #[derive(Protobuf)]
            struct CanPackNest {
                v: Vec<Vec<u32>>,
            }

            let no_pack = NoPackNest {
                v: vec![vec![1, 2, 3], vec![4, 5, 6]],
            };
            let v = encode(no_pack.clone());
            println!("{v:x?}");
            let can_pack = decode::<CanPackNest>(&v).unwrap();
            assert_eq!(no_pack.v, can_pack.v);
        }
    }

    #[test]
    fn test_merge() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Bar(u32);

        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        enum Enum {
            A(u32),
            B(Option<u32>, Vec<u8>),
        }

        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Foo {
            x: u32,
            y: u32,
            z: String,
            w: Option<bool>,
            v: Vec<u32>,
            v8: Vec<u8>,
            vb: Vec<Bar>,
            e: Enum,
        }

        let foo = Foo {
            x: 1,
            y: 2,
            z: "abc".to_string(),
            w: Some(true),
            v: vec![1, 2, 3],
            v8: b"xyz".to_vec(),
            vb: vec![Bar(1), Bar(2)],
            e: Enum::B(Some(1), b"abc".to_vec()),
        };
        assert_roundtrips(
            foo.clone(),
            expect!([r#"
                1: varint 1
                2: varint 2
                3: string "abc"
                4: varint 1
                5: bytes <010203>
                6: string "xyz"
                7: bytes <0801>
                7: bytes <0802>
                8: bytes <120708011203616263>
                raw: 080110021a0361626320012a03010203320378797a3a0208013a0208024209120708011203616263"#]),
        );
        let foo2 = Foo {
            x: 3,
            y: 4,
            z: "def".to_string(),
            w: None,
            v: vec![4, 5, 6],
            v8: b"uvw".to_vec(),
            vb: vec![Bar(3), Bar(4), Bar(5)],
            e: Enum::B(None, b"def".to_vec()),
        };
        assert_roundtrips(
            foo2.clone(),
            expect!([r#"
                1: varint 3
                2: varint 4
                3: string "def"
                5: bytes <040506>
                6: string "uvw"
                7: bytes <0803>
                7: bytes <0804>
                7: bytes <0805>
                8: bytes <12051203646566>
                raw: 080310041a036465662a0304050632037576773a0208033a0208043a020805420712051203646566"#]),
        );
        let foo3 = Foo {
            x: 3,
            y: 4,
            z: "def".to_string(),
            w: Some(true),
            v: vec![1, 2, 3, 4, 5, 6],
            v8: b"xyzuvw".to_vec(),
            vb: vec![Bar(1), Bar(2), Bar(3), Bar(4), Bar(5)],
            e: Enum::B(Some(1), b"abcdef".to_vec()),
        };
        assert_roundtrips(
            foo3.clone(),
            expect!([r#"
                1: varint 3
                2: varint 4
                3: string "def"
                4: varint 1
                5: bytes <010203040506>
                6: string "xyzuvw"
                7: bytes <0801>
                7: bytes <0802>
                7: bytes <0803>
                7: bytes <0804>
                7: bytes <0805>
                8: bytes <120a08011206616263646566>
                raw: 080310041a0364656620012a06010203040506320678797a7576773a0208013a0208023a0208033a0208043a020805420c120a08011206616263646566"#]),
        );
        let foo = super::merge(foo, &<SerializedMessage>::from_message(foo2).data).unwrap();
        assert_eq!(foo, foo3);
    }

    #[test]
    fn test_alternate_encoding() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Foo {
            sint32: i32,
            #[mesh(encoding = "mesh_protobuf::encoding::VarintField")]
            int32: i32,
        }
        assert_roundtrips(
            Foo {
                int32: -1,
                sint32: -1,
            },
            expect!([r#"
                1: varint 1
                2: varint 18446744073709551615
                raw: 080110ffffffffffffffffff01"#]),
        );
        assert_eq!(
            &encode(Foo {
                sint32: -1,
                int32: -1,
            }),
            &[8, 1, 16, 255, 255, 255, 255, 255, 255, 255, 255, 255, 1]
        );
    }

    #[test]
    fn test_array() {
        assert_field_roundtrips(
            [1, 2, 3],
            expect!([r#"
                1: bytes <020406>
                raw: 0a03020406"#]),
        );
        assert_field_roundtrips(
            ["a".to_string(), "b".to_string(), "c".to_string()],
            expect!([r#"
                1: bytes <0a01610a01620a0163>
                raw: 0a090a01610a01620a0163"#]),
        );
        assert_field_roundtrips(
            [vec![1, 2, 3], vec![4, 5, 6]],
            expect!([r#"
                1: bytes <0a050a030204060a050a03080a0c>
                raw: 0a0e0a050a030204060a050a03080a0c"#]),
        );
        assert_field_roundtrips(
            [vec![1u8, 2]],
            expect!([r#"
                1: bytes <0a020102>
                raw: 0a040a020102"#]),
        );
        assert_field_roundtrips(
            [[0_u8, 1], [2, 3]],
            expect!([r#"
                1: bytes <0a0200010a020203>
                raw: 0a080a0200010a020203"#]),
        );
        assert_field_roundtrips(
            [Vec::<()>::new()],
            expect!([r#"
                1: bytes <0a00>
                raw: 0a020a00"#]),
        );
        assert_field_roundtrips(
            [vec!["abc".to_string()]],
            expect!([r#"
                1: bytes <0a050a03616263>
                raw: 0a070a050a03616263"#]),
        );
    }

    #[test]
    fn test_nested() {
        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Nested<T> {
            pub n: u32,
            pub foo: T,
        }

        #[derive(Protobuf, Debug, Clone, PartialEq, Eq)]
        struct Foo {
            x: u32,
            y: u32,
            z: String,
            w: Option<bool>,
        }

        let t = Nested {
            n: 5,
            foo: Foo {
                x: 5,
                y: 104824,
                z: "alphabet".to_owned(),
                w: None,
            },
        };
        let t2: Nested<SerializedMessage> = SerializedMessage::from_message(t.clone())
            .into_message()
            .unwrap();
        let t3: Nested<Foo> = SerializedMessage::from_message(t2).into_message().unwrap();
        assert_eq!(t, t3);
    }

    #[test]
    fn test_lifetime() {
        #[derive(Protobuf)]
        struct Foo<'a>(&'a str);

        let s = String::from("foo");
        let v = encode(Foo(&s));
        let foo: Foo<'_> = decode(&v).unwrap();
        assert_eq!(foo.0, &s);
    }

    #[test]
    fn test_generic_lifetime() {
        #[derive(Protobuf)]
        struct Foo<T>(T);

        let s = String::from("foo");
        let v = encode(Foo(s.as_str()));
        let foo: Foo<&str> = decode(&v).unwrap();
        assert_eq!(foo.0, &s);
    }

    #[test]
    fn test_infallible() {
        assert!(matches!(
            decode::<Infallible>(&[])
                .unwrap_err()
                .source()
                .unwrap()
                .downcast_ref::<DecodeError>(),
            Some(DecodeError::Unexpected)
        ));
    }

    #[test]
    fn test_empty_message() {
        #[derive(Protobuf)]
        struct Message(u32);

        let v = encode(((Message(0),),));
        assert_eq!(&v, b"");

        let _message: ((Message,),) = decode(&[]).unwrap();
    }

    #[test]
    fn test_nested_empty_message() {
        #[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
        struct Message(Outer, Inner);

        #[derive(Debug, Default, Clone, PartialEq, Eq, Protobuf)]
        struct Outer(Inner);

        #[derive(Debug, Default, Clone, PartialEq, Eq, Protobuf)]
        struct Inner(u32);

        assert_roundtrips(
            Message(Default::default(), Inner(1)),
            expect!([r#"
                2: bytes <0801>
                raw: 12020801"#]),
        );
    }

    #[test]
    fn test_transparent_message() {
        #[derive(Protobuf, Copy, Clone, PartialEq, Eq, Debug)]
        struct Inner(u32);

        #[derive(Protobuf, Copy, Clone, PartialEq, Eq, Debug)]
        #[mesh(transparent)]
        struct TupleStruct(Inner);

        #[derive(Protobuf, Copy, Clone, PartialEq, Eq, Debug)]
        #[mesh(transparent)]
        struct NamedStruct {
            x: Inner,
        }

        #[derive(Protobuf, Copy, Clone, PartialEq, Eq, Debug)]
        #[mesh(transparent)]
        struct GenericStruct<T>(T);

        assert_roundtrips(
            TupleStruct(Inner(5)),
            expect!([r#"
                1: varint 5
                raw: 0805"#]),
        );
        assert_eq!(encode(TupleStruct(Inner(5))), encode(Inner(5)));
        assert_eq!(encode(NamedStruct { x: Inner(5) }), encode(Inner(5)));
        assert_eq!(encode(GenericStruct(Inner(5))), encode(Inner(5)));
    }

    #[test]
    fn test_transparent_field() {
        #[derive(Protobuf, Copy, Clone, PartialEq, Eq, Debug)]
        #[mesh(transparent)]
        struct Inner(u32);

        #[derive(Protobuf, Copy, Clone, PartialEq, Eq, Debug)]
        struct Outer<T>(T);

        assert_roundtrips(
            Outer(Inner(5)),
            expect!([r#"
                1: varint 5
                raw: 0805"#]),
        );
        assert_eq!(encode(Outer(Inner(5))), encode(Outer(5u32)));
    }

    #[test]
    fn test_transparent_enum() {
        #[derive(Protobuf, Clone, PartialEq, Eq, Debug)]
        enum Foo {
            #[mesh(transparent)]
            Bar(u32),
            #[mesh(transparent)]
            Option(Option<u32>),
            #[mesh(transparent)]
            Vec(Vec<u32>),
            #[mesh(transparent)]
            VecNoPack(Vec<(u32,)>),
        }

        assert_roundtrips(
            Foo::Bar(0),
            expect!([r#"
                1: varint 0
                raw: 0800"#]),
        );
        assert_eq!(encode(Foo::Bar(0)), encode((Some(0),)));
        assert_roundtrips(
            Foo::Option(Some(5)),
            expect!([r#"
                2: bytes <0805>
                raw: 12020805"#]),
        );
        assert_roundtrips(
            Foo::Option(None),
            expect!([r#"
                2: bytes <>
                raw: 1200"#]),
        );
        assert_roundtrips(
            Foo::Vec(vec![]),
            expect!([r#"
                3: bytes <>
                raw: 1a00"#]),
        );
        assert_roundtrips(
            Foo::Vec(vec![5]),
            expect!([r#"
                3: bytes <0a0105>
                raw: 1a030a0105"#]),
        );
        assert_roundtrips(
            Foo::VecNoPack(vec![(5,)]),
            expect!([r#"
                4: bytes <0a020805>
                raw: 22040a020805"#]),
        );
    }

    #[test]
    fn test_cow() {
        #[derive(Protobuf)]
        struct OwnedString<'a>(#[mesh(encoding = "OwningCowField")] Cow<'a, str>);
        #[derive(Protobuf)]
        struct BorrowedString<'a>(#[mesh(encoding = "BorrowedCowField")] Cow<'a, str>);
        #[derive(Protobuf)]
        struct OwnedBytes<'a>(#[mesh(encoding = "OwningCowField")] Cow<'a, [u8]>);
        #[derive(Protobuf)]
        struct BorrowedBytes<'a>(#[mesh(encoding = "BorrowedCowField")] Cow<'a, [u8]>);

        let s_owning: OwnedString<'_>;
        let v_owning: OwnedBytes<'_>;

        {
            let b = encode(("abc",));
            let mut b2 = b.clone();
            b2.extend(encode(("def",)));

            let s_borrowed: BorrowedString<'_>;
            let v_borrowed: BorrowedBytes<'_>;
            let v_borrowed2: BorrowedBytes<'_>;
            {
                let (s,): (String,) = decode(&b2).unwrap();
                assert_eq!(&s, "def");
                let (v,): (Vec<u8>,) = decode(&b2).unwrap();
                assert_eq!(&v, b"abcdef");

                s_owning = decode(&b2).unwrap();
                let s_owning = s_owning.0;
                assert!(matches!(s_owning, Cow::Owned(_)));
                assert_eq!(s_owning.as_ref(), "def");

                s_borrowed = decode(&b2).unwrap();
                let s_borrowed = s_borrowed.0;
                assert!(matches!(s_borrowed, Cow::Borrowed(_)));
                assert_eq!(s_borrowed.as_ref(), "def");

                v_owning = decode(&b2).unwrap();
                let v_owning = v_owning.0;
                assert!(matches!(v_owning, Cow::Owned(_)));
                assert_eq!(v_owning.as_ref(), b"abcdef");

                v_borrowed = decode(&b).unwrap();
                let v_borrowed = v_borrowed.0;
                assert!(matches!(v_borrowed, Cow::Borrowed(_)));
                assert_eq!(v_borrowed.as_ref(), b"abc");

                // This one is owned because it has to append more data.
                v_borrowed2 = decode(&b2).unwrap();
                let v_borrowed2 = v_borrowed2.0;
                assert!(matches!(v_borrowed2, Cow::Owned(_)));
                assert_eq!(v_borrowed2.as_ref(), b"abcdef");
            }
        }
    }

    #[test]
    fn test_duration() {
        assert_roundtrips(
            Duration::ZERO,
            expect!([r#"
                empty
                raw: "#]),
        );
        assert_roundtrips(
            Duration::from_secs(1),
            expect!([r#"
                1: varint 1
                raw: 0801"#]),
        );
        assert_roundtrips(
            Duration::from_secs(1) + Duration::from_nanos(10000),
            expect!([r#"
                1: varint 1
                2: varint 10000
                raw: 080110904e"#]),
        );
        assert_roundtrips(
            Duration::from_secs(1) - Duration::from_nanos(10000),
            expect!([r#"
                2: varint 999990000
                raw: 10f0c5eadc03"#]),
        );
        decode::<Duration>(&encode((-1i64 as u64, 0u32))).unwrap_err();
        assert_eq!(
            decode::<Duration>(&encode((1u64, 1u32))).unwrap(),
            Duration::from_secs(1) + Duration::from_nanos(1)
        );
    }

    #[test]
    fn test_failure_recovery() {
        let m = encode(("foo", 2, 3));
        decode::<(String, String, String)>(&m).unwrap_err();
    }
}
