// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::resource::Resource;
use crate::resource::SerializedMessage;
use mesh_protobuf::DefaultEncoding;
use mesh_protobuf::Error;
use mesh_protobuf::MessageDecode;
use mesh_protobuf::MessageEncode;
use mesh_protobuf::Upcast;

/// Converts a message from one type to another, where the destination type's
/// encoding is a superset of the source type's.
pub fn upcast<T, U>(value: T) -> U
where
    T: Upcast<U>,
    T: DefaultEncoding,
    T::Encoding: MessageEncode<T, Resource>,
    U: DefaultEncoding,
    U::Encoding: for<'a> MessageDecode<'a, U, Resource>,
{
    SerializedMessage::from_message(value)
        .into_message()
        .expect("round trip should not fail")
}

/// Converts a message from one type to another, where the destination type's
/// encoding is a subset of the source type's.
pub fn force_downcast<T, U>(value: T) -> Result<U, Error>
where
    U: Upcast<T>,
    T: DefaultEncoding,
    T::Encoding: MessageEncode<T, Resource>,
    U: DefaultEncoding,
    U::Encoding: for<'a> MessageDecode<'a, U, Resource>,
{
    SerializedMessage::from_message(value).into_message()
}
