// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper type for mesh-encoding a type that must first be translated to
//! another type.

use super::DefaultEncoding;
use super::InplaceOption;
use super::MessageDecode;
use super::MessageEncode;
use super::Result;
use super::encoding::MessageEncoding;
use super::fmt;
use super::protobuf::MessageReader;
use super::protobuf::MessageSizer;
use super::protobuf::MessageWriter;
use crate::inplace;
use core::ops::Deref;
use core::ops::DerefMut;

/// Wrapper type to easily support custom mesh encoding.
///
/// This type acts as `T` but encodes on a mesh channel as `U`. This is useful
/// when `T` cannot be encoded directly via the derive macro but can be
/// converted to a type that can be encoded directly.
pub struct EncodeAs<T, U>(Inner<T, U>);

pub struct EncodedMessage<E>(E);

#[derive(Copy, Clone)]
enum Inner<T, U> {
    Unencoded(T),
    Encoded(U),
    Invalid,
}

impl<T, U> Deref for EncodeAs<T, U> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            Inner::Unencoded(v) => v,
            _ => unreachable!(),
        }
    }
}

impl<T, U> DerefMut for EncodeAs<T, U> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.0 {
            Inner::Unencoded(v) => v,
            _ => unreachable!(),
        }
    }
}

impl<T, U> EncodeAs<T, U> {
    /// Extracts the inner `T`.
    pub fn into_inner(self) -> T {
        match self.0 {
            Inner::Unencoded(t) => t,
            _ => unreachable!(),
        }
    }
}

impl<T, U: From<T>> EncodeAs<T, U> {
    /// Constructs a new `EncodeAs` wrapping `t`.
    pub fn new(t: T) -> Self {
        Self(Inner::Unencoded(t))
    }

    fn encode(&mut self) -> &mut U {
        match core::mem::replace(&mut self.0, Inner::Invalid) {
            Inner::Unencoded(t) => {
                self.0 = Inner::Encoded(t.into());
            }
            _ => unreachable!("already encoded"),
        }
        match &mut self.0 {
            Inner::Encoded(u) => u,
            _ => unreachable!(),
        }
    }
}

impl<T, U: From<T>> From<T> for EncodeAs<T, U> {
    fn from(t: T) -> Self {
        Self::new(t)
    }
}

impl<T: Clone, U> Clone for EncodeAs<T, U> {
    fn clone(&self) -> Self {
        match &self.0 {
            Inner::Unencoded(v) => Self(Inner::Unencoded(v.clone())),
            _ => unreachable!(),
        }
    }
}

impl<T: Copy, U: Copy> Copy for EncodeAs<T, U> {}

impl<T: Default, U> Default for Inner<T, U> {
    fn default() -> Self {
        Inner::Unencoded(Default::default())
    }
}

impl<T: fmt::Display, U> fmt::Display for EncodeAs<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.deref(), f)
    }
}

impl<T: fmt::Debug, U> fmt::Debug for EncodeAs<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.deref(), f)
    }
}

impl<T, U: From<T>, R, E: MessageEncode<U, R>> MessageEncode<EncodeAs<T, U>, R>
    for EncodedMessage<E>
{
    fn write_message(item: EncodeAs<T, U>, writer: MessageWriter<'_, '_, R>) {
        match item.0 {
            Inner::Encoded(err) => E::write_message(err, writer),
            _ => unreachable!("compute_message_size has not been called"),
        }
    }

    fn compute_message_size(item: &mut EncodeAs<T, U>, sizer: MessageSizer<'_>) {
        E::compute_message_size(item.encode(), sizer);
    }
}

impl<'a, T, U: From<T> + Into<T>, R, E: MessageDecode<'a, U, R>>
    MessageDecode<'a, EncodeAs<T, U>, R> for EncodedMessage<E>
{
    fn read_message(
        item: &mut InplaceOption<'_, EncodeAs<T, U>>,
        reader: MessageReader<'a, '_, R>,
    ) -> Result<()> {
        let encoded = item.take().map(|v| v.into_inner().into());
        inplace!(encoded);
        E::read_message(&mut encoded, reader)?;
        item.set(EncodeAs(Inner::Unencoded(
            encoded.take().expect("should be constructed").into(),
        )));
        Ok(())
    }
}

impl<T, U: From<T> + Into<T> + DefaultEncoding> DefaultEncoding for EncodeAs<T, U> {
    type Encoding = MessageEncoding<EncodedMessage<U::Encoding>>;
}
