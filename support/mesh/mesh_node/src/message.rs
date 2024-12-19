// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the `Message` type.

// UNSAFETY: Needed to define, implement, and call the unsafe extract function.
#![allow(unsafe_code)]

use crate::resource::Resource;
use crate::resource::SerializedMessage;
use mesh_protobuf;
use mesh_protobuf::encoding::SerializedMessageEncoder;
use mesh_protobuf::inplace;
use mesh_protobuf::protobuf::Encoder;
use mesh_protobuf::protobuf::MessageSizer;
use mesh_protobuf::protobuf::MessageWriter;
use mesh_protobuf::MessageEncode;
use std::any::Any;
use std::any::TypeId;
use std::fmt;
use std::fmt::Debug;
use std::mem::MaybeUninit;

/// A message for sending over a channel.
#[derive(Default)]
pub struct Message(MessageInner);

enum MessageInner {
    Unserialized(Box<dyn DynSerializeMessage>),
    Serialized(SerializedMessage),
}

impl Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Message")
    }
}

impl Default for MessageInner {
    fn default() -> Self {
        Self::Serialized(Default::default())
    }
}

impl Message {
    /// Serializes the message and returns it.
    pub fn serialize(self) -> SerializedMessage {
        match self.0 {
            MessageInner::Unserialized(_) => SerializedMessage::from_message(self),
            MessageInner::Serialized(message) => message,
        }
    }
}

/// Trait for types that can be constructed as a [`Message`].
///
/// This does not include scalar types such as `u32`, which are encoded as
/// non-message types.
pub trait MeshPayload:
    mesh_protobuf::DefaultEncoding<Encoding = <Self as MeshPayload>::Encoding> + Send + 'static + Sized
{
    type Encoding: MessageEncode<Self, Resource>
        + for<'a> mesh_protobuf::MessageDecode<'a, Self, Resource>
        + mesh_protobuf::FieldEncode<Self, Resource>
        + for<'a> mesh_protobuf::FieldDecode<'a, Self, Resource>
        + Send
        + Sync;
}

impl<T> MeshPayload for T
where
    T: mesh_protobuf::DefaultEncoding + Any + Send + 'static,
    T::Encoding: MessageEncode<T, Resource>
        + for<'a> mesh_protobuf::MessageDecode<'a, T, Resource>
        + mesh_protobuf::FieldEncode<T, Resource>
        + for<'a> mesh_protobuf::FieldDecode<'a, T, Resource>
        + Send
        + Sync,
{
    type Encoding = T::Encoding;
}

/// Trait for types that can be a field in a mesh message, including both scalar
/// types and types that implement [`MeshPayload`].
pub trait MeshField:
    mesh_protobuf::DefaultEncoding<Encoding = <Self as MeshField>::Encoding> + Send + 'static + Sized
{
    type Encoding: mesh_protobuf::FieldEncode<Self, Resource>
        + for<'a> mesh_protobuf::FieldDecode<'a, Self, Resource>
        + Send
        + Sync;
}

impl<T> MeshField for T
where
    T: mesh_protobuf::DefaultEncoding + Any + Send + 'static,
    T::Encoding: mesh_protobuf::FieldEncode<T, Resource>
        + for<'a> mesh_protobuf::FieldDecode<'a, T, Resource>
        + Send
        + Sync,
{
    type Encoding = T::Encoding;
}

/// Trait implemented by concrete messages that can be extracted or serialized
/// into [`SerializedMessage`].
pub trait SerializeMessage: 'static + Send {
    /// The underlying concrete message type.
    type Concrete: Any;

    /// Computes the message size, as in [`MessageEncode::compute_message_size`].
    fn compute_message_size(&mut self, sizer: MessageSizer<'_>);

    /// Writes the message, as in [`MessageEncode::write_message`].
    fn write_message(self, writer: MessageWriter<'_, '_, Resource>);

    /// Extract the concrete message.
    fn extract(self) -> Self::Concrete;
}

/// # Safety
///
/// The implementor must ensure that `extract_or_serialize` initializes the
/// pointer if it returns `Ok(())`.
unsafe trait DynSerializeMessage: Send {
    fn compute_message_size(&mut self, sizer: MessageSizer<'_>);
    fn write_message(self: Box<Self>, writer: MessageWriter<'_, '_, Resource>);

    /// # Safety
    ///
    /// The caller must ensure that `ptr` points to storage whose type matches
    /// `type_id`.
    unsafe fn extract(
        self: Box<Self>,
        type_id: TypeId,
        ptr: *mut (),
    ) -> Result<(), Box<dyn DynSerializeMessage>>;
}

// SAFETY: extract_or_serialize satisfies implementation requirements.
unsafe impl<T: SerializeMessage> DynSerializeMessage for T {
    fn compute_message_size(&mut self, sizer: MessageSizer<'_>) {
        self.compute_message_size(sizer)
    }

    fn write_message(self: Box<Self>, writer: MessageWriter<'_, '_, Resource>) {
        (*self).write_message(writer)
    }

    unsafe fn extract(
        self: Box<Self>,
        type_id: TypeId,
        ptr: *mut (),
    ) -> Result<(), Box<dyn DynSerializeMessage>> {
        if type_id == TypeId::of::<T::Concrete>() {
            // SAFETY: ptr is guaranteed to be T::Concrete by caller.
            unsafe { ptr.cast::<T::Concrete>().write((*self).extract()) };
            Ok(())
        } else {
            Err(self)
        }
    }
}

fn serialize_dyn_message(message: Box<dyn DynSerializeMessage>) -> SerializedMessage {
    let (data, resources) = Encoder::<_, MessageEncoder, _>::with_encoding(message).encode();
    SerializedMessage { data, resources }
}

impl<T: MeshPayload> SerializeMessage for T {
    type Concrete = Self;

    fn compute_message_size(&mut self, sizer: MessageSizer<'_>) {
        <T as MeshPayload>::Encoding::compute_message_size(self, sizer)
    }

    fn write_message(self, writer: MessageWriter<'_, '_, Resource>) {
        <T as MeshPayload>::Encoding::write_message(self, writer)
    }

    fn extract(self) -> Self::Concrete {
        self
    }
}

impl Message {
    /// Creates a new message wrapping `data`, which will be lazily serialized
    /// when needed.
    #[inline]
    pub fn new<T: SerializeMessage>(data: T) -> Self {
        Self(MessageInner::Unserialized(Box::new(data)))
    }

    /// Creates a new message from already-serialized data in `s`.
    pub fn serialized(s: SerializedMessage) -> Self {
        Self(MessageInner::Serialized(s))
    }

    /// Parses the message into a value of type `T`.
    ///
    /// If the message was constructed with `new<T>`, then the round trip
    /// serialization/deserialization is skipped.
    pub fn parse<T: MeshPayload>(self) -> Result<T, mesh_protobuf::Error> {
        self.try_parse().or_else(|m| m.into_message())
    }

    pub fn try_parse<T: 'static + Send>(self) -> Result<T, SerializedMessage> {
        match self.0 {
            MessageInner::Unserialized(m) => {
                let mut message = MaybeUninit::<T>::uninit();
                // SAFETY: calling with appropriately sized and aligned buffer
                // for writing T.
                unsafe {
                    match m.extract(TypeId::of::<T>(), message.as_mut_ptr().cast()) {
                        Ok(()) => Ok(message.assume_init()),
                        Err(message) => Err(serialize_dyn_message(message)),
                    }
                }
            }
            MessageInner::Serialized(m) => Err(m),
        }
    }
}

impl mesh_protobuf::DefaultEncoding for Message {
    type Encoding = mesh_protobuf::encoding::MessageEncoding<MessageEncoder>;
}

pub struct MessageEncoder;

impl MessageEncode<Box<dyn DynSerializeMessage>, Resource> for MessageEncoder {
    fn write_message(item: Box<dyn DynSerializeMessage>, writer: MessageWriter<'_, '_, Resource>) {
        item.write_message(writer);
    }

    fn compute_message_size(item: &mut Box<dyn DynSerializeMessage>, sizer: MessageSizer<'_>) {
        item.compute_message_size(sizer);
    }
}

impl MessageEncode<Message, Resource> for MessageEncoder {
    fn write_message(item: Message, writer: MessageWriter<'_, '_, Resource>) {
        match item.0 {
            MessageInner::Unserialized(message) => Self::write_message(message, writer),
            MessageInner::Serialized(message) => {
                SerializedMessageEncoder::write_message(message, writer)
            }
        }
    }

    fn compute_message_size(item: &mut Message, sizer: MessageSizer<'_>) {
        match &mut item.0 {
            MessageInner::Unserialized(message) => Self::compute_message_size(message, sizer),
            MessageInner::Serialized(message) => {
                SerializedMessageEncoder::compute_message_size(message, sizer)
            }
        }
    }
}

impl mesh_protobuf::MessageDecode<'_, Message, Resource> for MessageEncoder {
    fn read_message(
        item: &mut inplace::InplaceOption<'_, Message>,
        reader: mesh_protobuf::protobuf::MessageReader<'_, '_, Resource>,
    ) -> mesh_protobuf::Result<()> {
        let message = item.take().map(Message::serialize);
        inplace!(message);
        SerializedMessageEncoder::read_message(&mut message, reader)?;
        item.set(Message::serialized(message.take().unwrap()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Message;
    use mesh_protobuf::encoding::ImpossibleField;

    #[test]
    fn roundtrip_without_serialize() {
        #[derive(Debug, Default)]
        struct CantSerialize;
        impl mesh_protobuf::DefaultEncoding for CantSerialize {
            type Encoding = ImpossibleField;
        }

        Message::new(CantSerialize)
            .parse::<CantSerialize>()
            .unwrap();
    }
}
