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
use core::net::Ipv4Addr;
use core::net::Ipv6Addr;
use mesh_protobuf::Protobuf;
use mesh_protobuf::decode;
use mesh_protobuf::encode;
use mesh_protobuf::merge;
use mesh_protobuf::message::ProtobufMessage;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
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
}

fn try_decode<T>(data: &[u8])
where
    T: mesh_protobuf::DefaultEncoding,
    T::Encoding: for<'a> mesh_protobuf::MessageDecode<'a, T, mesh_protobuf::NoResources>,
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
    }
}

fuzz_target!(|action: Action| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(action)
});
