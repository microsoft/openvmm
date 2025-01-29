// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tools to encode and decode protobuf messages.

use super::buffer;
use super::buffer::Buf;
use super::buffer::Buffer;
use super::DecodeError;
use super::InplaceOption;
use super::MessageDecode;
use super::MessageEncode;
use super::RefCell;
use super::Result;
use crate::DefaultEncoding;
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ops::Range;

/// Writes a variable-length integer, as defined in the protobuf specification.
fn write_varint(v: &mut Buf<'_>, mut n: u64) {
    while n > 0x7f {
        v.push(0x80 | (n & 0x7f) as u8);
        n >>= 7;
    }
    v.push(n as u8);
}

/// Computes the length of an encoded variable-length integer.
const fn varint_size(n: u64) -> usize {
    if n == 0 {
        1
    } else {
        let bits = 64 - n.leading_zeros() as usize;
        (((bits - 1) / 7) + 1) & 0xff
    }
}

/// Reads a variable-length integer, advancing `v`.
pub(crate) fn read_varint(v: &mut &[u8]) -> Result<u64> {
    let mut shift = 0;
    let mut r = 0;
    loop {
        let (b, rest) = v.split_first().ok_or(DecodeError::EofVarInt)?;
        *v = rest;
        r |= (*b as u64 & 0x7f) << shift;
        if *b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(DecodeError::VarIntTooBig.into());
        }
    }
    Ok(r)
}

/// Zigzag encodes a signed integer, as defined in the protobuf spec.
///
/// This is used when writing a variable-sized signed integer to keep the
/// encoding small.
fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// Reverses the zigzag encoding.
fn unzigzag(n: u64) -> i64 {
    let n = n as i64;
    ((n << 63) >> 63) ^ (n >> 1)
}

/// The protobuf wire type.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WireType {
    /// Variable-length integer.
    Varint = 0,
    /// Fixed 64-bit value.
    Fixed64 = 1,
    /// Variable-length byte buffer.
    Variable = 2,
    /// Fixed 32-bit value.
    Fixed32 = 5,

    /// Mesh extension: just like Variable but prefixed with two varints:
    /// * The number of ports used by the message.
    /// * The number of resources used by the message.
    MeshMessage = 6,

    /// Mesh extension. Consumes the next resource.
    Resource = 7,
}

struct DecodeInner<'a, R> {
    resources: &'a mut [Option<R>],
}

struct DecodeState<'a, R>(RefCell<DecodeInner<'a, R>>);

impl<'a, R> DecodeState<'a, R> {
    fn new(resources: &'a mut [Option<R>]) -> Self {
        Self(RefCell::new(DecodeInner { resources }))
    }

    /// Takes resource `index`.
    fn resource(&self, index: u32) -> Result<R> {
        (|| {
            self.0
                .borrow_mut()
                .resources
                .get_mut(index as usize)?
                .take()
        })()
        .ok_or_else(|| DecodeError::MissingResource.into())
    }
}

struct EncodeState<'a, R> {
    data: Buf<'a>,
    message_sizes: core::slice::Iter<'a, MessageSize>,
    resources: &'a mut Vec<R>,
    field_number: u32,
    in_sequence: bool,
}

impl<'a, R> EncodeState<'a, R> {
    fn new(data: Buf<'a>, message_sizes: &'a [MessageSize], resources: &'a mut Vec<R>) -> Self {
        Self {
            data,
            resources,
            message_sizes: message_sizes.iter(),
            field_number: 0,
            in_sequence: false,
        }
    }
}

/// Type used to write field values.
pub struct FieldWriter<'a, 'buf, R> {
    state: &'a mut EncodeState<'buf, R>,
}

impl<'a, 'buf, R> FieldWriter<'a, 'buf, R> {
    /// Writes the field key.
    fn key(&mut self, ty: WireType) {
        write_varint(
            &mut self.state.data,
            ((self.state.field_number << 3) | ty as u32).into(),
        );
    }

    fn cached_variable<F>(mut self, f: F)
    where
        F: FnOnce(&mut Self),
    {
        if let Some(expected_len) = self.write_next_cached_message_header() {
            f(&mut self);
            assert_eq!(expected_len, self.state.data.len(), "wrong size");
        }
    }

    /// Returns the expected size of the message, or None if the message is
    /// empty and `skip_empty` is true, and so the message does not need to be
    /// encoded.
    fn write_next_cached_message_header(&mut self) -> Option<usize> {
        let size = self
            .state
            .message_sizes
            .next()
            .expect("not enough messages in size calculation");
        if size.num_resources > 0 {
            self.key(WireType::MeshMessage);
            write_varint(&mut self.state.data, size.num_resources.into());
        } else if size.len > 0 || self.state.in_sequence {
            self.key(WireType::Variable);
        } else {
            return None;
        }
        write_varint(&mut self.state.data, size.len as u64);
        Some(self.state.data.len() + size.len)
    }

    /// Returns a sequence writer for writing the field multiple times.
    ///
    /// Panics if called while already writing a sequence, since this would
    /// result in an invalid protobuf message.
    pub fn sequence(self) -> SequenceWriter<'a, 'buf, R> {
        assert!(!self.state.in_sequence);
        SequenceWriter {
            field_number: self.state.field_number,
            state: self.state,
        }
    }

    /// Returns whether this write is occurring within a sequence.
    pub fn write_empty(&self) -> bool {
        self.state.in_sequence
    }

    /// Calls `f` with a writer for a message.
    pub fn message<F>(self, f: F)
    where
        F: FnOnce(MessageWriter<'_, 'buf, R>),
    {
        self.cached_variable(|this| {
            f(MessageWriter { state: this.state });
        });
    }

    /// Writes a resource.
    pub fn resource(mut self, resource: R) {
        self.key(WireType::Resource);
        self.state.resources.push(resource);
    }

    /// Writes an unsigned variable-sized integer.
    pub fn varint(mut self, n: u64) {
        if n != 0 || self.state.in_sequence {
            self.key(WireType::Varint);
            write_varint(&mut self.state.data, n);
        }
    }

    /// Writes a signed variable-sized integer.
    pub fn svarint(mut self, n: i64) {
        if n != 0 || self.state.in_sequence {
            self.key(WireType::Varint);
            write_varint(&mut self.state.data, zigzag(n));
        }
    }

    /// Writes a fixed 64-bit integer.
    pub fn fixed64(mut self, n: u64) {
        if n != 0 || self.state.in_sequence {
            self.key(WireType::Fixed64);
            self.state.data.append(&n.to_le_bytes());
        }
    }

    /// Writes a fixed 32-bit integer.
    pub fn fixed32(mut self, n: u32) {
        if n != 0 || self.state.in_sequence {
            self.key(WireType::Fixed32);
            self.state.data.append(&n.to_le_bytes());
        }
    }

    /// Writes a byte slice.
    pub fn bytes(mut self, b: &[u8]) {
        if !b.is_empty() || self.state.in_sequence {
            self.key(WireType::Variable);
            write_varint(&mut self.state.data, b.len() as u64);
            self.state.data.append(b);
        }
    }

    /// Calls `f` with a writer for the packed field.
    pub fn packed<F>(self, f: F)
    where
        F: FnOnce(PackedWriter<'_, '_>),
    {
        self.cached_variable(|this| {
            f(PackedWriter {
                data: &mut this.state.data,
            })
        })
    }
}

/// A writer for writing a sequence of fields.
pub struct SequenceWriter<'a, 'buf, R> {
    state: &'a mut EncodeState<'buf, R>,
    field_number: u32,
}

impl<'buf, R> SequenceWriter<'_, 'buf, R> {
    /// Gets a field writer to write the next field in the sequence.
    pub fn field(&mut self) -> FieldWriter<'_, 'buf, R> {
        self.state.field_number = self.field_number;
        self.state.in_sequence = true;
        FieldWriter { state: self.state }
    }
}

/// A writer for a message.
pub struct MessageWriter<'a, 'buf, R> {
    state: &'a mut EncodeState<'buf, R>,
}

impl<'buf, R> MessageWriter<'_, 'buf, R> {
    /// Returns a field writer for field number `n`.
    ///
    /// It's legal to write fields in any order and to write fields that
    /// duplicate previous fields. By convention, later fields overwrite
    /// previous ones (or append, in the case of sequences).
    pub fn field(&mut self, n: u32) -> FieldWriter<'_, 'buf, R> {
        self.state.field_number = n;
        self.state.in_sequence = false;
        FieldWriter { state: self.state }
    }

    /// Writes a raw message from bytes.
    pub fn bytes(&mut self, data: &[u8]) {
        self.state.data.append(data);
    }

    /// Writes a raw message.
    pub fn raw_message(&mut self, data: &[u8], resources: impl IntoIterator<Item = R>) {
        self.state.data.append(data);
        self.state.resources.extend(resources);
    }
}

#[derive(Copy, Clone, Default)]
struct MessageSize {
    len: usize,
    num_resources: u32,
}

struct SizeState {
    message_sizes: Vec<MessageSize>,
    index: usize,
    tag_size: u8,
    in_sequence: bool,
}

impl SizeState {
    fn new() -> Self {
        Self {
            message_sizes: vec![MessageSize::default()],
            index: 0,
            tag_size: 0,
            in_sequence: false,
        }
    }
}

/// Type used to compute the size of field values.
pub struct FieldSizer<'a> {
    state: &'a mut SizeState,
}

struct PreviousSizeParams {
    index: u32,
    tag_size: u8,
    in_sequence: bool,
}

impl<'a> FieldSizer<'a> {
    fn add(&mut self, size: usize) {
        // Add room for the field tag.
        self.state.message_sizes[self.state.index].len += self.state.tag_size as usize + size;
    }

    /// Makes and returns a writer for a message.
    fn cached_variable<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Self),
    {
        // Cache the size for use when writing the message.
        let prev = self.reserve_cached_message_size_entry();
        f(self);
        self.set_cached_message_size(prev);
    }

    fn reserve_cached_message_size_entry(&mut self) -> PreviousSizeParams {
        let index = self.state.message_sizes.len();
        self.state.message_sizes.push(MessageSize::default());
        PreviousSizeParams {
            index: core::mem::replace(&mut self.state.index, index) as u32,
            tag_size: self.state.tag_size,
            in_sequence: self.state.in_sequence,
        }
    }

    fn set_cached_message_size(&mut self, prev: PreviousSizeParams) {
        let size = self.state.message_sizes[self.state.index];
        let index = core::mem::replace(&mut self.state.index, prev.index as usize);
        let parent_size = &mut self.state.message_sizes[self.state.index];
        let mut len = varint_size(size.len as u64) + size.len;
        if size.num_resources > 0 {
            // This will be a MeshMessage field.
            len += varint_size(size.num_resources as u64);
            parent_size.num_resources += size.num_resources;
        } else if !prev.in_sequence && size.len == 0 {
            // This message is empty, so skip it and any nested messages.
            self.state.message_sizes[index] = Default::default();
            self.state.message_sizes.truncate(index + 1);
            return;
        }
        parent_size.len += prev.tag_size as usize + len;
    }

    /// Returns a sequence sizer for sizing the field multiple times.
    ///
    /// Panics if called while already sizing a sequence, since this would
    /// result in an invalid protobuf message.
    pub fn sequence(self) -> SequenceSizer<'a> {
        SequenceSizer {
            tag_size: self.state.tag_size,
            state: self.state,
        }
    }

    /// If true, encoders must write their fields even if they are empty.
    pub fn write_empty(&self) -> bool {
        self.state.in_sequence
    }

    /// Computes the size for a message. Calls `f` with a [`MessageSizer`] to
    /// calculate the size of each field.
    pub fn message<F>(mut self, f: F)
    where
        F: FnOnce(MessageSizer<'_>),
    {
        self.cached_variable(|this| {
            f(MessageSizer::new(this.state));
        })
    }

    /// Computes the size for a resource.
    pub fn resource(mut self) {
        self.state.message_sizes[self.state.index].num_resources += 1;
        self.add(0);
    }

    /// Computes the size for an unsigned variable-sized integer.
    pub fn varint(mut self, n: u64) {
        if n != 0 || self.state.in_sequence {
            self.add(varint_size(n));
        }
    }

    /// Computes the size for a signed variable-sized integer.
    pub fn svarint(mut self, n: i64) {
        if n != 0 || self.state.in_sequence {
            self.add(varint_size(zigzag(n)));
        }
    }

    /// Computes the size for a fixed 64-bit integer.
    pub fn fixed64(mut self, n: u64) {
        if n != 0 || self.state.in_sequence {
            self.add(8);
        }
    }

    /// Computes the size for a fixed 32-bit integer.
    pub fn fixed32(mut self, n: u32) {
        if n != 0 || self.state.in_sequence {
            self.add(4);
        }
    }

    /// Computes the size for a byte slice.
    pub fn bytes(mut self, len: usize) {
        if len != 0 || self.state.in_sequence {
            self.add(varint_size(len as u64) + len);
        }
    }

    /// Computes the size of a packed value. Calls `f` with a [`PackedSizer`] to
    /// sum the size of each element.
    pub fn packed<F>(mut self, f: F)
    where
        F: FnOnce(PackedSizer<'_>),
    {
        self.cached_variable(|this| {
            f(PackedSizer {
                size: &mut this.state.message_sizes[this.state.index].len,
            });
        })
    }
}

/// A sizer for computing the size of a sequence of fields.
pub struct SequenceSizer<'a> {
    state: &'a mut SizeState,
    tag_size: u8,
}

impl SequenceSizer<'_> {
    /// Gets a field sizer for the next field in the sequence.
    pub fn field(&mut self) -> FieldSizer<'_> {
        self.state.tag_size = self.tag_size;
        self.state.in_sequence = true;
        FieldSizer { state: self.state }
    }
}

/// A type to compute the size of a message.
pub struct MessageSizer<'a> {
    state: &'a mut SizeState,
}

impl<'a> MessageSizer<'a> {
    fn new(state: &'a mut SizeState) -> Self {
        Self { state }
    }

    /// Returns a field sizer for field number `n`.
    pub fn field(&mut self, n: u32) -> FieldSizer<'_> {
        self.state.tag_size = varint_size((n as u64) << 3) as u8;
        self.state.in_sequence = false;
        FieldSizer { state: self.state }
    }

    /// Sizes the message as `n` bytes.
    pub fn bytes(&mut self, n: usize) {
        self.state.message_sizes[self.state.index] = MessageSize {
            len: n,
            ..Default::default()
        };
    }

    /// Sizes the message as `n` bytes plus `num_resources` resources.
    pub fn raw_message(&mut self, len: usize, num_resources: u32) {
        self.state.message_sizes[self.state.index] = MessageSize { len, num_resources }
    }
}

/// A parsed protobuf value.
#[derive(Debug, Clone)]
enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Variable(&'a [u8]),
    Fixed32(u32),
    Resource(u32),
    MeshMessage {
        data: &'a [u8],
        resources: Range<u32>,
    },
}

/// A reader for a payload field.
pub struct FieldReader<'a, 'b, R> {
    field: Value<'a>,
    state: &'b DecodeState<'b, R>,
}

impl<'a, 'b, R> FieldReader<'a, 'b, R> {
    /// Gets the wire type for the field.
    pub fn wire_type(&self) -> WireType {
        match &self.field {
            Value::Varint(_) => WireType::Varint,
            Value::Fixed64(_) => WireType::Fixed64,
            Value::Variable(_) => WireType::Variable,
            Value::Fixed32(_) => WireType::Fixed32,
            Value::MeshMessage { .. } => WireType::MeshMessage,
            Value::Resource { .. } => WireType::Resource,
        }
    }

    /// Makes and returns an message reader.
    pub fn message(self) -> Result<MessageReader<'a, 'b, R>> {
        if let Value::Variable(data) = self.field {
            Ok(MessageReader {
                data,
                state: self.state,
                resources: 0..0,
            })
        } else if let Value::MeshMessage { data, resources } = self.field {
            Ok(MessageReader {
                data,
                state: self.state,
                resources,
            })
        } else {
            Err(DecodeError::ExpectedMessage.into())
        }
    }

    /// Reads a resource.
    pub fn resource(self) -> Result<R> {
        if let Value::Resource(index) = self.field {
            self.state.resource(index)
        } else {
            Err(DecodeError::ExpectedResource.into())
        }
    }

    /// Reads an unsigned variable-sized integer.
    pub fn varint(self) -> Result<u64> {
        if let Value::Varint(n) = self.field {
            Ok(n)
        } else {
            Err(DecodeError::ExpectedVarInt.into())
        }
    }

    /// Reads a signed variable-sized integer.
    pub fn svarint(self) -> Result<i64> {
        Ok(unzigzag(self.varint()?))
    }

    /// Reads a fixed 64-bit integer.
    pub fn fixed64(self) -> Result<u64> {
        if let Value::Fixed64(n) = self.field {
            Ok(n)
        } else {
            Err(DecodeError::ExpectedFixed64.into())
        }
    }

    /// Reads a fixed 32-bit integer.
    pub fn fixed32(self) -> Result<u32> {
        if let Value::Fixed32(n) = self.field {
            Ok(n)
        } else {
            Err(DecodeError::ExpectedFixed32.into())
        }
    }

    /// Reads a byte slice.
    pub fn bytes(self) -> Result<&'a [u8]> {
        if let Value::Variable(data) = self.field {
            Ok(data)
        } else {
            Err(DecodeError::ExpectedByteArray.into())
        }
    }

    /// Gets a reader for a packed field.
    pub fn packed(self) -> Result<PackedReader<'a>> {
        Ok(PackedReader {
            data: self.bytes()?,
        })
    }
}

/// Reader for an message.
///
/// Implements [`Iterator`] to return (field number, [`FieldReader`]) pairs.
/// Users must be prepared to handle fields in any order, allowing unknown and
/// duplicate fields.
pub struct MessageReader<'a, 'b, R> {
    data: &'a [u8],
    resources: Range<u32>,
    state: &'b DecodeState<'b, R>,
}

impl<'a, 'b, R> IntoIterator for MessageReader<'a, 'b, R> {
    type Item = Result<(u32, FieldReader<'a, 'b, R>)>;
    type IntoIter = FieldIterator<'a, 'b, R>;

    fn into_iter(self) -> Self::IntoIter {
        FieldIterator(self)
    }
}

impl<'a, 'b, R> MessageReader<'a, 'b, R> {
    fn new(data: &'a [u8], state: &'b DecodeState<'b, R>) -> Self {
        let num_resources = state.0.borrow().resources.len() as u32;
        Self {
            data,
            state,
            resources: 0..num_resources,
        }
    }

    /// Gets the message data as a byte slice.
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    /// Returns an iterator to consume the resources for this message.
    pub fn take_resources(&mut self) -> impl ExactSizeIterator<Item = Result<R>> + use<'b, R> {
        let state = self.state;
        self.resources.clone().map(move |i| {
            state
                .0
                .borrow_mut()
                .resources
                .get_mut(i as usize)
                .and_then(|x| x.take())
                .ok_or_else(|| DecodeError::MissingResource.into())
        })
    }

    fn parse_field(&mut self) -> Result<(u32, FieldReader<'a, 'b, R>)> {
        let key = read_varint(&mut self.data)?;
        let wire_type = (key & 7) as u32;
        let field_number = (key >> 3) as u32;
        let field = match wire_type {
            0 => Value::Varint(read_varint(&mut self.data)?),
            1 => {
                if self.data.len() < 8 {
                    return Err(DecodeError::EofFixed64.into());
                }
                let (n, rest) = self.data.split_at(8);
                self.data = rest;
                Value::Fixed64(u64::from_le_bytes(n.try_into().unwrap()))
            }
            2 => {
                let len = read_varint(&mut self.data)?;
                if (self.data.len() as u64) < len {
                    return Err(DecodeError::EofByteArray.into());
                }
                let (data, rest) = self.data.split_at(len as usize);
                self.data = rest;
                Value::Variable(data)
            }
            5 => {
                if self.data.len() < 4 {
                    return Err(DecodeError::EofFixed32.into());
                }
                let (n, rest) = self.data.split_at(4);
                self.data = rest;
                Value::Fixed32(u32::from_le_bytes(n.try_into().unwrap()))
            }
            6 => {
                let num_resources = read_varint(&mut self.data)? as u32;
                let len = read_varint(&mut self.data)?;

                if self.resources.len() < num_resources as usize {
                    return Err(DecodeError::InvalidResourceRange.into());
                }
                if (self.data.len() as u64) < len {
                    return Err(DecodeError::EofByteArray.into());
                }

                let (data, rest) = self.data.split_at(len as usize);
                self.data = rest;

                let resources = self.resources.start..self.resources.start + num_resources;
                self.resources = resources.end..self.resources.end;

                Value::MeshMessage { data, resources }
            }
            7 => {
                let resource = self.resources.next().ok_or(DecodeError::MissingResource)?;
                Value::Resource(resource)
            }
            n => return Err(DecodeError::UnknownWireType(n).into()),
        };
        Ok((
            field_number,
            FieldReader {
                field,
                state: self.state,
            },
        ))
    }
}

/// An iterator over message fields.
///
/// Returned by [`MessageReader::into_iter()`].
pub struct FieldIterator<'a, 'b, R>(MessageReader<'a, 'b, R>);

impl<'a, 'b, R> Iterator for FieldIterator<'a, 'b, R> {
    type Item = Result<(u32, FieldReader<'a, 'b, R>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.data.is_empty() {
            return None;
        }
        Some(self.0.parse_field())
    }
}

/// A writer for a packed field.
pub struct PackedWriter<'a, 'buf> {
    data: &'a mut Buf<'buf>,
}

impl PackedWriter<'_, '_> {
    /// Appends `bytes`.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.data.append(bytes);
    }

    /// Appends varint `v`.
    pub fn varint(&mut self, v: u64) {
        write_varint(self.data, v);
    }

    /// Appends signed (zigzag-encoded) varint `v`.
    pub fn svarint(&mut self, v: i64) {
        write_varint(self.data, zigzag(v));
    }

    /// Appends fixed 64-bit value `v`.
    pub fn fixed64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    /// Appends fixed 32-bit value `v`.
    pub fn fixed32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
}

/// A type to help compute the size of a packed field.
pub struct PackedSizer<'a> {
    size: &'a mut usize,
}

impl PackedSizer<'_> {
    /// Adds the size of `len` bytes.
    pub fn bytes(&mut self, len: usize) {
        *self.size += len;
    }

    /// Adds the size of a varint value `v`.
    pub fn varint(&mut self, v: u64) {
        *self.size += varint_size(v);
    }

    /// Adds the size of a signed (zigzag-encoded) varint value `v`.
    pub fn svarint(&mut self, v: i64) {
        *self.size += varint_size(zigzag(v));
    }

    /// Adds the size of a fixed 64-bit value.
    pub fn fixed64(&mut self) {
        *self.size += 8;
    }

    /// Adds the size of a fixed 32-bit value.
    pub fn fixed32(&mut self) {
        *self.size += 4;
    }
}

/// Reader for packed fields.
pub struct PackedReader<'a> {
    data: &'a [u8],
}

impl<'a> PackedReader<'a> {
    /// Reads the remaining bytes.
    pub fn bytes(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.data)
    }

    /// Reads a varint.
    ///
    /// Returns `Ok(None)` if there are no more values.
    pub fn varint(&mut self) -> Result<Option<u64>> {
        if self.data.is_empty() {
            Ok(None)
        } else {
            read_varint(&mut self.data).map(Some)
        }
    }

    /// Reads a signed (zigzag-encoded) varint.
    ///
    /// Returns `Ok(None)` if there are no more values.
    pub fn svarint(&mut self) -> Result<Option<i64>> {
        if self.data.is_empty() {
            Ok(None)
        } else {
            read_varint(&mut self.data).map(|n| Some(unzigzag(n)))
        }
    }

    /// Reads a fixed 64-bit value.
    ///
    /// Returns `Ok(None)` if there are no more values.
    pub fn fixed64(&mut self) -> Result<Option<u64>> {
        if self.data.is_empty() {
            Ok(None)
        } else if self.data.len() < 8 {
            Err(DecodeError::EofFixed64.into())
        } else {
            let (b, data) = self.data.split_at(8);
            self.data = data;
            Ok(Some(u64::from_le_bytes(b.try_into().unwrap())))
        }
    }

    /// Reads a fixed 32-bit value.
    ///
    /// Returns `Ok(None)` if there are no more values.
    pub fn fixed32(&mut self) -> Result<Option<u32>> {
        if self.data.is_empty() {
            Ok(None)
        } else if self.data.len() < 4 {
            Err(DecodeError::EofFixed32.into())
        } else {
            let (b, data) = self.data.split_at(4);
            self.data = data;
            Ok(Some(u32::from_le_bytes(b.try_into().unwrap())))
        }
    }
}

/// An encoder for a single message of type `T`, using the messaging encoding
/// `E`.
pub struct Encoder<T, E, R> {
    message: T,
    message_sizes: Vec<MessageSize>,
    _phantom: PhantomData<(fn() -> R, E)>,
}

impl<R, T: DefaultEncoding> Encoder<T, T::Encoding, R>
where
    T::Encoding: MessageEncode<T, R>,
{
    /// Creates an encoder for `message`.F
    pub fn new(message: T) -> Self {
        Encoder::with_encoding(message)
    }
}

impl<T, R, E: MessageEncode<T, R>> Encoder<T, E, R> {
    /// Creates an encoder for `message` with a specific encoder.
    pub fn with_encoding(mut message: T) -> Self {
        let mut state = SizeState::new();
        E::compute_message_size(&mut message, MessageSizer::new(&mut state));
        Self {
            message,
            message_sizes: state.message_sizes,
            _phantom: PhantomData,
        }
    }

    /// Returns the length of the message in bytes.
    pub fn len(&self) -> usize {
        self.message_sizes[0].len
    }

    /// Returns the number of resources in the message.
    pub fn resource_count(&self) -> usize {
        self.message_sizes[0].num_resources as usize
    }

    /// Encodes the message into `buffer`.
    pub fn encode_into(self, buffer: &mut dyn Buffer, resources: &mut Vec<R>) {
        buffer::write_with(buffer, |buf| {
            let capacity = buf.remaining();
            let init_resources = resources.len();
            let mut state = EncodeState::new(buf, &self.message_sizes, resources);
            let size = state.message_sizes.next().unwrap();
            E::write_message(self.message, MessageWriter { state: &mut state });
            assert_eq!(capacity - state.data.remaining(), size.len);
            assert_eq!(
                state.resources.len() - init_resources,
                size.num_resources as usize
            );
            assert!(state.message_sizes.next().is_none());
        })
    }

    /// Encodes the message.
    pub fn encode(self) -> (Vec<u8>, Vec<R>) {
        let mut data = Vec::with_capacity(self.len());
        let mut resources = Vec::with_capacity(self.resource_count());
        self.encode_into(&mut data, &mut resources);
        (data, resources)
    }
}

/// Decodes a protobuf message into `message` using encoding `T`.
///
/// If `message` already exists, then the fields are merged according to
/// protobuf rules.
pub fn decode_with<'a, E: MessageDecode<'a, T, R>, T, R>(
    message: &mut InplaceOption<'_, T>,
    data: &'a [u8],
    resources: &mut [Option<R>],
) -> Result<()> {
    let state = DecodeState::new(resources);
    let reader = MessageReader::new(data, &state);
    E::read_message(message, reader)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::buffer;
    use std::eprintln;

    #[test]
    fn test_zigzag() {
        let cases: &[(i64, u64)] = &[
            (0, 0),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (2147483647, 4294967294),
            (-2147483648, 4294967295),
        ];
        for (a, b) in cases.iter().copied() {
            assert_eq!(zigzag(a), b);
            assert_eq!(a, unzigzag(b));
        }
    }

    #[test]
    fn test_varint() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0]),
            (1, &[1]),
            (0x80, &[0x80, 1]),
            (
                -1i64 as u64,
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x1],
            ),
        ];
        for (a, mut b) in cases.iter().copied() {
            eprintln!("{:#x}, {:#x?}", a, b);
            assert_eq!(varint_size(a), b.len());
            let mut v = Vec::with_capacity(10);
            buffer::write_with(&mut v, |mut buf| write_varint(&mut buf, a));
            assert_eq!(&v, b);
            assert_eq!(a, read_varint(&mut b).unwrap());
            assert!(b.is_empty());
        }
    }

    #[test]
    fn test_resource() {
        let mut state = SizeState::new();
        let mut sizer = MessageSizer::new(&mut state);
        sizer.field(1).resource();
        sizer.field(2).resource();
        sizer.field(3).message(|mut sizer| {
            sizer.field(1).resource();
            sizer.field(1).resource();
            sizer.field(1).resource();
        });
        let size = state.message_sizes.remove(0);
        assert_eq!(size.num_resources, 5);

        let mut data = Vec::with_capacity(size.len);
        let mut resources = Vec::with_capacity(size.num_resources as usize);
        buffer::write_with(&mut data, |buf| {
            let mut state = EncodeState::new(buf, &state.message_sizes, &mut resources);
            let mut writer = MessageWriter { state: &mut state };
            writer.field(1).resource(());
            writer.field(2).resource(());
            writer.field(3).message(|mut writer| {
                writer.field(1).resource(());
                writer.field(1).resource(());
                writer.field(1).resource(());
            });
        });

        let mut resources: Vec<_> = resources.into_iter().map(Some).collect();
        let state = DecodeState(RefCell::new(DecodeInner {
            resources: &mut resources,
        }));
        let reader = MessageReader {
            data: &data,
            state: &state,
            resources: 0..5,
        };

        let mut it = reader.into_iter();
        let (n, r) = it.next().unwrap().unwrap();
        assert_eq!(n, 1);
        r.resource().unwrap();
        let (n, r) = it.next().unwrap().unwrap();
        assert_eq!(n, 2);
        r.resource().unwrap();
        let (n, r) = it.next().unwrap().unwrap();
        assert_eq!(n, 3);
        let message = r.message().unwrap();
        assert!(it.next().is_none());

        let mut it = message.into_iter();
        let (n, r) = it.next().unwrap().unwrap();
        assert_eq!(n, 1);
        r.resource().unwrap();
        let (n, r) = it.next().unwrap().unwrap();
        assert_eq!(n, 1);
        r.resource().unwrap();
        let (n, r) = it.next().unwrap().unwrap();
        assert_eq!(n, 1);
        r.resource().unwrap();
        assert!(it.next().is_none());
    }
}
