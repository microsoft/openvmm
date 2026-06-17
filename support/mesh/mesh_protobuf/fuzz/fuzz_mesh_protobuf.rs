// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for the `mesh_protobuf` encode/decode engine.
//!
//! This target exercises three things:
//!
//! 1. **Decode** of arbitrary attacker-controlled bytes into a variety of
//!    target types, covering the major encoding shapes.
//! 2. **Round-trip** of arbitrary instances of those types
//!    (encode → decode → compare).
//! 3. **Merge** of arbitrary bytes into an existing value, which exercises the
//!    "decode into existing" path used by `SerializedMessage::into_message`
//!    and protobuf field-merge semantics.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use core::convert::Infallible;
use core::marker::PhantomData;
use core::net::Ipv4Addr;
use core::net::Ipv6Addr;
use core::ops::Range;
use mesh_protobuf::EncodeAs;
use mesh_protobuf::NoResources;
use mesh_protobuf::Protobuf;
use mesh_protobuf::SerializedMessage;
use mesh_protobuf::Timestamp;
use mesh_protobuf::decode;
use mesh_protobuf::encode;
use mesh_protobuf::merge;
use mesh_protobuf::message::ProtobufAny;
use mesh_protobuf::message::ProtobufMessage;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroI8;
use std::num::NonZeroI16;
use std::num::NonZeroI32;
use std::num::NonZeroI64;
use std::num::NonZeroIsize;
use std::num::NonZeroU8;
use std::num::NonZeroU16;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::num::Wrapping;
use std::sync::Arc;
use std::time::Duration;
use xtask_fuzz::fuzz_target;

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct Simple {
    a: u32,
    b: i64,
    c: bool,
    d: String,
    e: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithVecs {
    nums: Vec<u32>,
    bytes: Vec<u8>,
    nested: Vec<Simple>,
    packed_nested: Vec<Vec<u32>>,
    strings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithOptions {
    a: Option<u32>,
    b: Option<Simple>,
    c: Option<String>,
    d: Option<Vec<u8>>,
    e: Option<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
enum Choice {
    Empty,
    Number(u32),
    Text(String),
    Pair(i64, bool),
    Inner(Simple),
    Many(Vec<u32>),
    Struct { x: u32, y: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct Outer {
    simple: Simple,
    vecs: WithVecs,
    opts: WithOptions,
    choices: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithMaps {
    by_str: BTreeMap<String, u32>,
    by_id: BTreeMap<i64, Simple>,
    nested: BTreeMap<u32, BTreeMap<u32, u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithExotic {
    c: char,
    nz_a: NonZeroU32,
    nz_b: Option<NonZeroU64>,
    nz_vec: Vec<NonZeroU32>,
    addr4: Ipv4Addr,
    addr6: Ipv6Addr,
    arr_u32: [u32; 4],
    arr_bytes: [u8; 16],
    arr_nested: [Simple; 2],
    boxed: Box<u32>,
    boxed_simple: Box<Simple>,
    arc_simple: Arc<Simple>,
    result: Result<u32, String>,
}

// Intentionally NOT `Arbitrary`
#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
struct Recursive {
    inner: Option<Box<Recursive>>,
    data: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
enum TransparentEnum {
    #[mesh(transparent)]
    Str(String),
    #[mesh(transparent)]
    Num(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
enum TransparentMix {
    #[mesh(transparent)]
    Num(u32),
    #[mesh(transparent)]
    Text(String),
    #[mesh(transparent)]
    List(Vec<u32>),
    #[mesh(transparent)]
    Inner(Box<Simple>),
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
#[mesh(transparent)]
struct TransparentId(u64);

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithTransparent {
    id: TransparentId,
    name: String,
    ids: Vec<TransparentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct Numbered {
    #[mesh(3)]
    a: u32,
    #[mesh(1)]
    b: String,
    #[mesh(7)]
    c: Option<Simple>,
    #[mesh(100)]
    d: Vec<u32>,
    #[mesh(2)]
    e: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct AllInts {
    field_u8: u8,
    field_u16: u16,
    field_u32: u32,
    field_u64: u64,
    field_u128: u128,
    field_usize: usize,
    field_i8: i8,
    field_i16: i16,
    field_i32: i32,
    field_i64: i64,
    field_isize: isize,
    field_wrapping: Wrapping<u64>,
    field_bool: bool,
    field_char: char,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct AllNonZero {
    a: NonZeroU8,
    b: NonZeroU16,
    c: NonZeroU32,
    d: NonZeroU64,
    e: NonZeroUsize,
    f: NonZeroI8,
    g: NonZeroI16,
    h: NonZeroI32,
    i: NonZeroI64,
    j: NonZeroIsize,
    opt: Option<NonZeroU32>,
    packed: Vec<NonZeroI64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithPhantom {
    a: u32,
    p1: PhantomData<u64>,
    b: String,
    p2: PhantomData<Simple>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithRange {
    unsigned: Range<u32>,
    signed: Range<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
struct WithInfallible {
    a: u32,
    never: Option<Infallible>,
    b: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithZeroCopy {
    #[mesh(1, encoding = "mesh_protobuf::encoding::ZeroCopyEncoding")]
    scalar: u32,
    #[mesh(2, encoding = "mesh_protobuf::encoding::ZeroCopyEncoding")]
    bytes: [u8; 8],
    #[mesh(3)]
    tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
struct WithCow {
    #[mesh(1, encoding = "mesh_protobuf::encoding::OwningCowField")]
    text: Cow<'static, str>,
    #[mesh(2, encoding = "mesh_protobuf::encoding::OwningCowField")]
    bytes: Cow<'static, [u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary)]
struct Celsius {
    whole: i32,
    frac: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct CelsiusEncoded {
    #[mesh(1)]
    whole: i32,
    #[mesh(2)]
    frac: u32,
}

impl From<Celsius> for CelsiusEncoded {
    fn from(value: Celsius) -> Self {
        Self {
            whole: value.whole,
            frac: value.frac,
        }
    }
}

impl From<CelsiusEncoded> for Celsius {
    fn from(value: CelsiusEncoded) -> Self {
        Self {
            whole: value.whole,
            frac: value.frac,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
#[mesh(package = "fuzz.mesh")]
struct DescribedSimple {
    #[mesh(1)]
    a: u32,
    #[mesh(2)]
    b: String,
    #[mesh(3)]
    c: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
#[mesh(package = "fuzz.mesh")]
enum DescribedChoice {
    #[mesh(1)]
    Empty,
    #[mesh(2)]
    Value(u32),
    #[mesh(3)]
    Named {
        #[mesh(1)]
        x: u32,
        #[mesh(2)]
        y: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
#[mesh(transparent)]
struct TransparentMessage(Simple);

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithSigned {
    #[mesh(1, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    a: i32,
    #[mesh(2, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    b: i64,
    #[mesh(3, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    c: i16,
    #[mesh(4, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    d: i8,
    #[mesh(5, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    e: isize,
    #[mesh(6, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    nz_a: NonZeroI32,
    #[mesh(7, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    nz_b: NonZeroI64,
    #[mesh(8, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    flag: bool,
    #[mesh(9, encoding = "mesh_protobuf::encoding::SignedVarintField")]
    ch: char,
    #[mesh(
        10,
        encoding = "mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::SignedVarintField>"
    )]
    packed: Vec<i64>,
    #[mesh(
        11,
        encoding = "mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::SignedVarintField>>"
    )]
    nested: Vec<Vec<i64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithFixedInts {
    #[mesh(1, encoding = "mesh_protobuf::encoding::Fixed32Field")]
    a: u32,
    #[mesh(2, encoding = "mesh_protobuf::encoding::Fixed32Field")]
    b: i32,
    #[mesh(3, encoding = "mesh_protobuf::encoding::Fixed64Field")]
    c: u64,
    #[mesh(4, encoding = "mesh_protobuf::encoding::Fixed64Field")]
    d: i64,
    #[mesh(
        5,
        encoding = "mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::Fixed32Field>"
    )]
    packed32: Vec<u32>,
    #[mesh(
        6,
        encoding = "mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::Fixed64Field>"
    )]
    packed64: Vec<u64>,
    #[mesh(
        7,
        encoding = "mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::Fixed32Field>>"
    )]
    nested32: Vec<Vec<u32>>,
    #[mesh(
        8,
        encoding = "mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::VecField<mesh_protobuf::encoding::Fixed64Field>>"
    )]
    nested64: Vec<Vec<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Arbitrary, Protobuf)]
struct WithBytesField {
    #[mesh(1, encoding = "mesh_protobuf::encoding::BytesField")]
    raw: Vec<u8>,
    #[mesh(2)]
    tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
struct WithBorrowedCow<'a> {
    #[mesh(1, encoding = "mesh_protobuf::encoding::BorrowedCowField")]
    text: Cow<'a, str>,
    #[mesh(2, encoding = "mesh_protobuf::encoding::BorrowedCowField")]
    bytes: Cow<'a, [u8]>,
}

#[derive(Debug, Arbitrary)]
enum TargetType {
    /// Tuple of a single varint primitive.
    U32,
    /// Tuple of a single signed varint primitive (zigzag).
    I64,
    /// Tuple of a single 128-bit little-endian primitive.
    U128,
    /// Tuple of a single bool.
    Bool,
    /// Tuple of a single string.
    StringT,
    /// Tuple of a single byte vector (raw bytes field).
    BytesVec,
    /// Tuple of a packed varint vector.
    U32Vec,
    /// Tuple of a vector of vectors (exercises wrap-in-sequence).
    NestedVec,
    /// Three-field tuple exercising mixed wire types.
    Triple,
    /// Tuple of an optional nested message.
    OptionalNested,
    /// Derived `Protobuf` struct of primitives.
    Simple,
    /// Derived `Protobuf` struct with vectors (packed and unpacked).
    WithVecs,
    /// Derived `Protobuf` struct with optional fields.
    WithOptions,
    /// Derived `Protobuf` enum (oneof) with mixed variants.
    Choice,
    /// Deeply nested derived `Protobuf` struct.
    Outer,
    /// `Fixed32Field` — bit-pattern decoded via `f32::from_bits`.
    F32,
    /// `Fixed64Field` — bit-pattern decoded via `f64::from_bits`.
    F64,
    /// Packed `Fixed32` sequence; decoder enforces length % 4 == 0.
    F32Vec,
    /// Packed `Fixed64` sequence; decoder enforces length % 8 == 0.
    F64Vec,
    /// Packed signed varint vector (zigzag decoded).
    PackedI32Vec,
    /// Packed signed varint vector (zigzag decoded, 64-bit).
    PackedI64Vec,
    /// `Duration` — range-validated `(secs, nanos)` message.
    DurationT,
    /// `ProtobufMessage` — opaque byte container.
    ProtobufMsg,
    /// Recursive type — stresses decoder recursion limits.
    RecursiveT,
    /// `BTreeMap` field exercises.
    WithMapsT,
    /// Validation-heavy primitives.
    WithExoticT,
    /// Transparent enum (oneof).
    TransparentEnumT,
    /// Transparent enum with mixed heap-owning variants.
    TransparentMixT,
    /// Struct embedding transparent newtypes (field and repeated).
    WithTransparentT,
    /// Struct with explicit, sparse field numbers.
    NumberedT,
    /// Every integer width (varint, fixed, byte, 128-bit, `Wrapping`).
    AllIntsT,
    /// Every `NonZero` width
    AllNonZeroT,
    /// `PhantomData` fields via the `IgnoreField` encoding.
    WithPhantomT,
    /// `Range` fields via the derived `RangeAsPayload` encoding.
    WithRangeT,
    /// `Option<Infallible>` via the `ImpossibleField` encoding.
    WithInfallibleT,
    /// Explicit `ZeroCopyEncoding` fields.
    WithZeroCopyT,
    /// Explicit `OwningCowField` `Cow` fields.
    WithCowT,
    /// `EncodeAs<Celsius, CelsiusEncoded>` translation encoding.
    EncodeAsT,
    /// `Timestamp` message.
    TimestampT,
    /// `SerializedMessage` — opaque, resource-carrying byte container.
    SerializedMessageT,
    /// `ProtobufAny` — `google.protobuf.Any`-compatible container.
    ProtobufAnyT,
    /// Described message (`#[mesh(package = ...)]`).
    DescribedSimpleT,
    /// Described `oneof` message (`#[mesh(package = ...)]`).
    DescribedChoiceT,
    /// Transparent newtype wrapping a message (`MessageDecode` path).
    TransparentMessageT,
    /// Struct of zigzag (`SignedVarintField`) integer fields.
    WithSignedT,
    /// Struct of `Fixed32`/`Fixed64`-encoded integer fields.
    WithFixedIntsT,
    /// Struct with a `BytesField`-encoded `Vec<u8>`.
    WithBytesFieldT,
    /// `Box<Simple>` — exercises the `Box` message decoder.
    BoxSimpleT,
    /// `Arc<Simple>` — exercises the `Arc` message decoder.
    ArcSimpleT,
}

#[derive(Debug, Arbitrary)]
enum Action {
    /// Attempt to decode arbitrary bytes into the chosen target type.
    Decode {
        target: TargetType,
        data: Vec<u8>,
    },

    /// Encode a typed instance, then decode and compare for equality.
    RoundtripSimple(Simple),
    RoundtripWithVecs(WithVecs),
    RoundtripWithOptions(WithOptions),
    RoundtripChoice(Choice),
    RoundtripOuter(Outer),
    RoundtripMaps(WithMaps),
    RoundtripExotic(WithExotic),
    RoundtripTransparentEnum(TransparentEnum),
    RoundtripTransparentMix(TransparentMix),
    RoundtripWithTransparent(WithTransparent),
    RoundtripNumbered(Numbered),
    RoundtripAllInts(AllInts),
    RoundtripAllNonZero(AllNonZero),
    RoundtripWithPhantom(WithPhantom),
    RoundtripWithRange(WithRange),
    RoundtripWithZeroCopy(WithZeroCopy),
    RoundtripDescribedSimple(DescribedSimple),
    RoundtripDescribedChoice(DescribedChoice),
    RoundtripWithCow {
        text: String,
        bytes: Vec<u8>,
    },
    RoundtripEncodeAs(Celsius),
    RoundtripTimestamp {
        seconds: i64,
        nanos: i32,
    },
    RoundtripF32(f32),
    RoundtripF64(f64),
    RoundtripF32Vec(Vec<f32>),
    RoundtripF64Vec(Vec<f64>),
    RoundtripSerialized(Vec<u8>),
    RoundtripTransparentMessage(TransparentMessage),
    RoundtripWithSigned(WithSigned),
    RoundtripWithFixedInts(WithFixedInts),
    RoundtripWithBytesField(WithBytesField),
    RoundtripBoxSimple(Simple),
    RoundtripArcSimple(Simple),
    RoundtripDuration {
        secs: u64,
        nanos: u32,
    },
    RoundtripProtobufMessage(Simple),
    RoundtripBorrowedCow {
        text: String,
        bytes: Vec<u8>,
    },

    /// Decode arbitrary bytes and merge them into an existing value. Merge
    /// semantics differ per encoding: scalars overwrite, repeated fields
    /// append, oneofs replace the active variant, and `Arc`-wrapped values
    /// invoke `Arc::make_mut`.
    MergeSimple {
        initial: Simple,
        data: Vec<u8>,
    },
    MergeWithVecs {
        initial: WithVecs,
        data: Vec<u8>,
    },
    MergeWithOptions {
        initial: WithOptions,
        data: Vec<u8>,
    },
    MergeChoice {
        initial: Choice,
        data: Vec<u8>,
    },
    MergeOuter {
        initial: Outer,
        data: Vec<u8>,
    },
    MergeMaps {
        initial: WithMaps,
        data: Vec<u8>,
    },
    MergeExotic {
        initial: WithExotic,
        data: Vec<u8>,
    },
    MergeTransparentEnum {
        initial: TransparentEnum,
        data: Vec<u8>,
    },
    MergeTransparentMix {
        initial: TransparentMix,
        data: Vec<u8>,
    },
    MergeWithTransparent {
        initial: WithTransparent,
        data: Vec<u8>,
    },
    MergeNumbered {
        initial: Numbered,
        data: Vec<u8>,
    },
    MergeAllInts {
        initial: AllInts,
        data: Vec<u8>,
    },
    MergeDescribedSimple {
        initial: DescribedSimple,
        data: Vec<u8>,
    },
    MergeSerialized {
        initial: Vec<u8>,
        data: Vec<u8>,
    },
    MergeEncodeAs {
        initial: Celsius,
        data: Vec<u8>,
    },
    MergeTransparentMessage {
        initial: TransparentMessage,
        data: Vec<u8>,
    },
    MergeWithSigned {
        initial: WithSigned,
        data: Vec<u8>,
    },
    MergeWithFixedInts {
        initial: WithFixedInts,
        data: Vec<u8>,
    },

    DecodeBorrowedCow(Vec<u8>),

    /// Serialize a message via [`SerializedMessage::from_message`] and decode it
    /// back via [`SerializedMessage::into_message`].
    SerializedFromInto(Simple),

    /// Wrap a described message in [`ProtobufAny`] and exercise the type-URL
    /// `parse`/`is_message` machinery.
    ExerciseAny(DescribedSimple),
}

fn try_decode<T>(data: &[u8])
where
    T: mesh_protobuf::DefaultEncoding,
    T::Encoding: for<'a> mesh_protobuf::MessageDecode<'a, T, NoResources>,
{
    let _ = decode::<T>(data);
}

fn try_decode_target(target: TargetType, data: &[u8]) {
    match target {
        TargetType::U32 => try_decode::<(u32,)>(data),
        TargetType::I64 => try_decode::<(i64,)>(data),
        TargetType::U128 => try_decode::<(u128,)>(data),
        TargetType::Bool => try_decode::<(bool,)>(data),
        TargetType::StringT => try_decode::<(String,)>(data),
        TargetType::BytesVec => try_decode::<(Vec<u8>,)>(data),
        TargetType::U32Vec => try_decode::<(Vec<u32>,)>(data),
        TargetType::NestedVec => try_decode::<(Vec<Vec<u32>>,)>(data),
        TargetType::Triple => try_decode::<(u32, String, Vec<u8>)>(data),
        TargetType::OptionalNested => try_decode::<(Option<Simple>,)>(data),
        TargetType::Simple => try_decode::<Simple>(data),
        TargetType::WithVecs => try_decode::<WithVecs>(data),
        TargetType::WithOptions => try_decode::<WithOptions>(data),
        TargetType::Choice => try_decode::<Choice>(data),
        TargetType::Outer => try_decode::<Outer>(data),
        TargetType::F32 => try_decode::<(f32,)>(data),
        TargetType::F64 => try_decode::<(f64,)>(data),
        TargetType::F32Vec => try_decode::<(Vec<f32>,)>(data),
        TargetType::F64Vec => try_decode::<(Vec<f64>,)>(data),
        TargetType::PackedI32Vec => try_decode::<(Vec<i32>,)>(data),
        TargetType::PackedI64Vec => try_decode::<(Vec<i64>,)>(data),
        TargetType::DurationT => try_decode::<Duration>(data),
        TargetType::ProtobufMsg => try_decode::<ProtobufMessage>(data),
        TargetType::RecursiveT => try_decode::<Recursive>(data),
        TargetType::WithMapsT => try_decode::<WithMaps>(data),
        TargetType::WithExoticT => try_decode::<WithExotic>(data),
        TargetType::TransparentEnumT => try_decode::<TransparentEnum>(data),
        TargetType::TransparentMixT => try_decode::<TransparentMix>(data),
        TargetType::WithTransparentT => try_decode::<WithTransparent>(data),
        TargetType::NumberedT => try_decode::<Numbered>(data),
        TargetType::AllIntsT => try_decode::<AllInts>(data),
        TargetType::AllNonZeroT => try_decode::<AllNonZero>(data),
        TargetType::WithPhantomT => try_decode::<WithPhantom>(data),
        TargetType::WithRangeT => try_decode::<WithRange>(data),
        TargetType::WithInfallibleT => try_decode::<WithInfallible>(data),
        TargetType::WithZeroCopyT => try_decode::<WithZeroCopy>(data),
        TargetType::WithCowT => try_decode::<WithCow>(data),
        TargetType::EncodeAsT => try_decode::<EncodeAs<Celsius, CelsiusEncoded>>(data),
        TargetType::TimestampT => try_decode::<Timestamp>(data),
        TargetType::SerializedMessageT => try_decode::<SerializedMessage>(data),
        TargetType::ProtobufAnyT => try_decode::<ProtobufAny>(data),
        TargetType::DescribedSimpleT => try_decode::<DescribedSimple>(data),
        TargetType::DescribedChoiceT => try_decode::<DescribedChoice>(data),
        TargetType::TransparentMessageT => try_decode::<TransparentMessage>(data),
        TargetType::WithSignedT => try_decode::<WithSigned>(data),
        TargetType::WithFixedIntsT => try_decode::<WithFixedInts>(data),
        TargetType::WithBytesFieldT => try_decode::<WithBytesField>(data),
        TargetType::BoxSimpleT => try_decode::<Box<Simple>>(data),
        TargetType::ArcSimpleT => try_decode::<Arc<Simple>>(data),
    }
}

/// Encode `value`, decode it back, and assert the result is equal.
fn roundtrip<T>(value: T)
where
    T: Protobuf + Clone + std::fmt::Debug + PartialEq,
{
    let bytes = encode(value.clone());
    let decoded =
        decode::<T>(&bytes).expect("a value produced by encode() must decode without error");
    assert_eq!(value, decoded, "round-trip must preserve the value");
}

/// Returns true if two `f32`s have identical bit patterns, treating `+0.0` and
/// `-0.0` as equal (the sign of zero need not survive a round-trip).
fn f32_bit_eq(a: f32, b: f32) -> bool {
    a.to_bits() == b.to_bits() || (a == 0.0 && b == 0.0)
}

/// Returns true if two `f64`s have identical bit patterns, treating `+0.0` and
/// `-0.0` as equal (the sign of zero need not survive a round-trip).
fn f64_bit_eq(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a == 0.0 && b == 0.0)
}

/// Round-trips a single `f32`, comparing bit patterns so that `NaN` is handled.
fn roundtrip_f32(value: f32) {
    let bytes = encode((value,));
    let (decoded,) =
        decode::<(f32,)>(&bytes).expect("a value produced by encode() must decode without error");
    assert!(
        f32_bit_eq(value, decoded),
        "f32 round-trip must preserve the bit pattern"
    );
}

/// Round-trips a single `f64`, comparing bit patterns so that `NaN` is handled.
fn roundtrip_f64(value: f64) {
    let bytes = encode((value,));
    let (decoded,) =
        decode::<(f64,)>(&bytes).expect("a value produced by encode() must decode without error");
    assert!(
        f64_bit_eq(value, decoded),
        "f64 round-trip must preserve the bit pattern"
    );
}

/// Round-trips a packed `Vec<f32>`, comparing element bit patterns.
fn roundtrip_f32_vec(value: Vec<f32>) {
    let bytes = encode((value.clone(),));
    let (decoded,) = decode::<(Vec<f32>,)>(&bytes)
        .expect("a value produced by encode() must decode without error");
    assert_eq!(
        value.len(),
        decoded.len(),
        "packed length must be preserved"
    );
    for (a, b) in value.iter().zip(&decoded) {
        assert!(f32_bit_eq(*a, *b), "f32 element must round-trip");
    }
}

/// Round-trips a packed `Vec<f64>`, comparing element bit patterns.
fn roundtrip_f64_vec(value: Vec<f64>) {
    let bytes = encode((value.clone(),));
    let (decoded,) = decode::<(Vec<f64>,)>(&bytes)
        .expect("a value produced by encode() must decode without error");
    assert_eq!(
        value.len(),
        decoded.len(),
        "packed length must be preserved"
    );
    for (a, b) in value.iter().zip(&decoded) {
        assert!(f64_bit_eq(*a, *b), "f64 element must round-trip");
    }
}

fn roundtrip_encode_as(value: Celsius) {
    let wrapper: EncodeAs<Celsius, CelsiusEncoded> = value.clone().into();
    let mut cloned = wrapper.clone();
    assert_eq!(*cloned, value, "EncodeAs must deref to the inner value");
    // Touch the value through `DerefMut` and `Debug` so those impls are covered.
    std::hint::black_box(&mut *cloned);
    std::hint::black_box(format!("{cloned:?}"));
    let bytes = encode(wrapper);
    let decoded = decode::<EncodeAs<Celsius, CelsiusEncoded>>(&bytes)
        .expect("a value produced by encode() must decode without error");
    assert_eq!(
        value,
        decoded.into_inner(),
        "EncodeAs round-trip must preserve the value"
    );
}

fn roundtrip_duration(secs: u64, nanos: u32) {
    let value = Duration::new(secs & i64::MAX as u64, nanos % 1_000_000_000);
    let bytes = encode(value);
    let decoded =
        decode::<Duration>(&bytes).expect("a value produced by encode() must decode without error");
    assert_eq!(
        value, decoded,
        "Duration round-trip must preserve the value"
    );
}

fn roundtrip_protobuf_message(value: Simple) {
    let message = ProtobufMessage::new(value.clone());
    let bytes = encode(message);
    let decoded = decode::<ProtobufMessage>(&bytes)
        .expect("a value produced by encode() must decode without error");
    let parsed = decoded
        .parse::<Simple>()
        .expect("the inner message must decode back into its original type");
    assert_eq!(
        value, parsed,
        "ProtobufMessage round-trip must preserve the value"
    );
}

fn roundtrip_borrowed_cow(text: String, bytes: Vec<u8>) {
    let value = WithBorrowedCow {
        text: Cow::Owned(text),
        bytes: Cow::Owned(bytes),
    };
    let encoded = encode(value.clone());
    let decoded = decode::<WithBorrowedCow<'_>>(&encoded)
        .expect("a value produced by encode() must decode without error");
    assert_eq!(
        value, decoded,
        "BorrowedCow round-trip must preserve the value"
    );
}

/// Round-trips a [`SerializedMessage`] containing arbitrary raw data. The raw
/// bytes are preserved verbatim, so re-encoding the decoded message must yield
/// identical bytes.
fn roundtrip_serialized(data: Vec<u8>) {
    let message = SerializedMessage::<NoResources> {
        data,
        resources: Vec::new(),
    };
    let bytes = encode(message);
    let decoded = decode::<SerializedMessage>(&bytes)
        .expect("a value produced by encode() must decode without error");
    let reencoded = encode(decoded);
    assert_eq!(
        bytes, reencoded,
        "SerializedMessage round-trip must preserve the raw bytes"
    );
}

/// Wraps a described message in [`ProtobufAny`] and exercises the type-URL
/// `parse`/`is_message` paths, including a deliberate type mismatch.
fn exercise_any(value: DescribedSimple) {
    let any = ProtobufAny::new(value.clone());
    assert!(
        any.is_message::<DescribedSimple>(),
        "ProtobufAny must report its own type"
    );
    assert!(
        !any.is_message::<DescribedChoice>(),
        "ProtobufAny must not report a different type"
    );
    let parsed = any
        .parse::<DescribedSimple>()
        .expect("ProtobufAny must parse back into its own type");
    assert_eq!(value, parsed, "ProtobufAny round-trip must preserve value");
    // Parsing as a different type must fail with a type mismatch rather than
    // panicking.
    assert!(
        any.parse::<DescribedChoice>().is_err(),
        "ProtobufAny must reject a type-URL mismatch"
    );
    // Exercise both the compact and the alternate `Debug` representations.
    std::hint::black_box(format!("{any:?}"));
    std::hint::black_box(format!("{any:#?}"));
}

fn do_fuzz(action: Action) {
    match action {
        Action::Decode { target, data } => try_decode_target(target, &data),
        Action::RoundtripSimple(v) => roundtrip(v),
        Action::RoundtripWithVecs(v) => roundtrip(v),
        Action::RoundtripWithOptions(v) => roundtrip(v),
        Action::RoundtripChoice(v) => roundtrip(v),
        Action::RoundtripOuter(v) => roundtrip(v),
        Action::RoundtripMaps(v) => roundtrip(v),
        Action::RoundtripExotic(v) => roundtrip(v),
        Action::RoundtripTransparentEnum(v) => roundtrip(v),
        Action::RoundtripTransparentMix(v) => roundtrip(v),
        Action::RoundtripWithTransparent(v) => roundtrip(v),
        Action::RoundtripNumbered(v) => roundtrip(v),
        Action::MergeSimple { initial, data } => {
            let _ = merge::<Simple>(initial, &data);
        }
        Action::MergeWithVecs { initial, data } => {
            let _ = merge::<WithVecs>(initial, &data);
        }
        Action::MergeWithOptions { initial, data } => {
            let _ = merge::<WithOptions>(initial, &data);
        }
        Action::MergeChoice { initial, data } => {
            let _ = merge::<Choice>(initial, &data);
        }
        Action::MergeOuter { initial, data } => {
            let _ = merge::<Outer>(initial, &data);
        }
        Action::MergeMaps { initial, data } => {
            let _ = merge::<WithMaps>(initial, &data);
        }
        Action::MergeExotic { initial, data } => {
            let _ = merge::<WithExotic>(initial, &data);
        }
        Action::MergeTransparentEnum { initial, data } => {
            let _ = merge::<TransparentEnum>(initial, &data);
        }
        Action::MergeTransparentMix { initial, data } => {
            let _ = merge::<TransparentMix>(initial, &data);
        }
        Action::MergeWithTransparent { initial, data } => {
            let _ = merge::<WithTransparent>(initial, &data);
        }
        Action::MergeNumbered { initial, data } => {
            let _ = merge::<Numbered>(initial, &data);
        }
        Action::RoundtripAllInts(v) => roundtrip(v),
        Action::RoundtripAllNonZero(v) => roundtrip(v),
        Action::RoundtripWithPhantom(v) => roundtrip(v),
        Action::RoundtripWithRange(v) => roundtrip(v),
        Action::RoundtripWithZeroCopy(v) => roundtrip(v),
        Action::RoundtripDescribedSimple(v) => roundtrip(v),
        Action::RoundtripDescribedChoice(v) => roundtrip(v),
        Action::RoundtripWithCow { text, bytes } => roundtrip(WithCow {
            text: Cow::Owned(text),
            bytes: Cow::Owned(bytes),
        }),
        Action::RoundtripEncodeAs(v) => roundtrip_encode_as(v),
        Action::RoundtripTimestamp { seconds, nanos } => roundtrip(Timestamp { seconds, nanos }),
        Action::RoundtripF32(v) => roundtrip_f32(v),
        Action::RoundtripF64(v) => roundtrip_f64(v),
        Action::RoundtripF32Vec(v) => roundtrip_f32_vec(v),
        Action::RoundtripF64Vec(v) => roundtrip_f64_vec(v),
        Action::RoundtripSerialized(data) => roundtrip_serialized(data),
        Action::SerializedFromInto(v) => {
            let serialized = SerializedMessage::<NoResources>::from_message(v.clone());
            let decoded = serialized
                .into_message::<Simple>()
                .expect("from_message/into_message must round-trip");
            assert_eq!(v, decoded, "from_message/into_message must preserve value");
        }
        Action::ExerciseAny(v) => exercise_any(v),
        Action::MergeAllInts { initial, data } => {
            let _ = merge::<AllInts>(initial, &data);
        }
        Action::MergeDescribedSimple { initial, data } => {
            let _ = merge::<DescribedSimple>(initial, &data);
        }
        Action::MergeSerialized { initial, data } => {
            let message = SerializedMessage::<NoResources> {
                data: initial,
                resources: Vec::new(),
            };
            let _ = merge::<SerializedMessage>(message, &data);
        }
        Action::MergeEncodeAs { initial, data } => {
            let _ = merge::<EncodeAs<Celsius, CelsiusEncoded>>(
                EncodeAs::<Celsius, CelsiusEncoded>::new(initial),
                &data,
            );
        }
        Action::MergeTransparentMessage { initial, data } => {
            let _ = merge::<TransparentMessage>(initial, &data);
        }
        Action::MergeWithSigned { initial, data } => {
            let _ = merge::<WithSigned>(initial, &data);
        }
        Action::MergeWithFixedInts { initial, data } => {
            let _ = merge::<WithFixedInts>(initial, &data);
        }
        Action::RoundtripTransparentMessage(v) => roundtrip(v),
        Action::RoundtripWithSigned(v) => roundtrip(v),
        Action::RoundtripWithFixedInts(v) => roundtrip(v),
        Action::RoundtripWithBytesField(v) => roundtrip(v),
        Action::RoundtripBoxSimple(v) => roundtrip(Box::new(v)),
        Action::RoundtripArcSimple(v) => roundtrip(Arc::new(v)),
        Action::RoundtripDuration { secs, nanos } => roundtrip_duration(secs, nanos),
        Action::RoundtripProtobufMessage(v) => roundtrip_protobuf_message(v),
        Action::RoundtripBorrowedCow { text, bytes } => roundtrip_borrowed_cow(text, bytes),
        Action::DecodeBorrowedCow(data) => {
            let _ = decode::<WithBorrowedCow<'_>>(&data);
        }
    }
}

fuzz_target!(|action: Action| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(action)
});
