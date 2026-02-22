// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type-erased protobuf message support.

use crate::DefaultEncoding;
use crate::DescribedProtobuf;
use crate::Error;
use crate::MessageDecode;
use crate::MessageEncode;
use crate::Protobuf;
use crate::decode;
use crate::encode;
use crate::encoding::MessageEncoding;
use crate::inplace::InplaceOption;
use crate::protobuf::MessageReader;
use crate::protobuf::MessageSizer;
use crate::protobuf::MessageWriter;
use crate::protofile::DescribeField;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;
use crate::table::DescribeTable;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use thiserror::Error;

/// An opaque protobuf message.
//
// TODO: delay encoding like in mesh::Message. This requires splitting some of
// the encoding traits up to remove the resource type.
#[derive(Debug)]
pub struct ProtobufMessage(Vec<u8>);

impl ProtobufMessage {
    /// Encodes `data` as a protobuf message.
    pub fn new(data: impl Protobuf) -> Self {
        Self(encode(data))
    }

    /// Decodes the protobuf message into `T`.
    pub fn parse<T: Protobuf>(&self) -> Result<T, Error> {
        decode(&self.0)
    }
}

impl DefaultEncoding for ProtobufMessage {
    type Encoding = MessageEncoding<ProtobufMessageEncoding>;
}

impl DescribeField<ProtobufMessage> for MessageEncoding<ProtobufMessageEncoding> {
    const FIELD_TYPE: FieldType<'static> = FieldType::builtin("bytes");
}

/// Encoder for [`ProtobufMessage`].
#[derive(Debug)]
pub struct ProtobufMessageEncoding;

impl<R> MessageEncode<ProtobufMessage, R> for ProtobufMessageEncoding {
    fn write_message(item: ProtobufMessage, mut writer: MessageWriter<'_, '_, R>) {
        writer.bytes(&item.0);
    }

    fn compute_message_size(item: &mut ProtobufMessage, mut sizer: MessageSizer<'_>) {
        sizer.bytes(item.0.len());
    }
}

impl<R> MessageDecode<'_, ProtobufMessage, R> for ProtobufMessageEncoding {
    fn read_message(
        item: &mut InplaceOption<'_, ProtobufMessage>,
        reader: MessageReader<'_, '_, R>,
    ) -> crate::Result<()> {
        item.get_or_insert_with(|| ProtobufMessage(Vec::new()))
            .0
            .extend(reader.bytes());
        Ok(())
    }
}

/// A protobuf message and the associated protobuf type URL.
///
/// This has the encoding of `google.protobuf.Any`.
#[derive(Protobuf)]
pub struct ProtobufAny {
    #[mesh(1)]
    type_url: String, // FUTURE: avoid allocation here
    #[mesh(2)]
    value: ProtobufMessage,
}

impl core::fmt::Debug for ProtobufAny {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if f.alternate() {
            // Full debug output like derive would produce
            f.debug_struct("ProtobufAny")
                .field("type_url", &self.type_url)
                .field("value", &self.value)
                .finish()
        } else {
            // Compact output with just type_url and data length
            f.debug_struct("ProtobufAny")
                .field("type_url", &self.type_url)
                .field("value_len", &self.value.0.len())
                .finish()
        }
    }
}

#[derive(Debug, Error)]
#[error("protobuf type mismatch, expected {expected}, got {actual}")]
struct TypeMismatch {
    expected: String,
    actual: String,
}

impl DescribeTable for ProtobufAny {
    const DESCRIPTION: MessageDescription<'static> = MessageDescription::External {
        name: "google.protobuf.Any",
        import_path: "google/protobuf/any.proto",
    };
}

impl ProtobufAny {
    /// Encodes `data` as a protobuf message.
    pub fn new<T: DescribedProtobuf>(data: T) -> Self {
        Self {
            type_url: T::TYPE_URL.to_string(),
            value: ProtobufMessage::new(data),
        }
    }

    /// Decodes the protobuf message into `T`.
    ///
    /// Fails if this message is an encoding of a different type.
    pub fn parse<T: DescribedProtobuf>(&self) -> Result<T, Error> {
        if &T::TYPE_URL != self.type_url.as_str() {
            return Err(Error::new(TypeMismatch {
                expected: T::TYPE_URL.to_string(),
                actual: self.type_url.clone(),
            }));
        }
        self.value.parse()
    }

    /// Returns `true` if this message is an encoding of `T`.
    pub fn is_message<T: DescribedProtobuf>(&self) -> bool {
        &T::TYPE_URL == self.type_url.as_str()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use crate::Protobuf;
    use crate::encode;
    use crate::message::ProtobufAny;
    use crate::message::ProtobufMessage;
    use crate::tests::as_expect_str;
    use expect_test::expect;
    use std::println;

    #[test]
    fn test_message() {
        let message = (5u32,);

        // Round trips.
        assert_eq!(
            ProtobufMessage::new(message).parse::<(u32,)>().unwrap(),
            message
        );

        let expected = expect!([r#"
            1: varint 5
            raw: 0805"#]);
        let actual = encode(ProtobufMessage::new(message));
        expected.assert_eq(&as_expect_str(&actual));

        // Is transparent.
        assert_eq!(actual, encode(message));
    }

    #[test]
    fn test_any() {
        #[derive(Protobuf, PartialEq, Eq, Copy, Clone, Debug)]
        #[mesh(package = "test")]
        struct Message {
            #[mesh(1)]
            x: u32,
        }

        #[derive(Protobuf, Debug)]
        #[mesh(package = "test")]
        struct Other {
            #[mesh(1)]
            x: u32,
        }

        let msg = Message { x: 5 };
        let any = ProtobufAny::new(msg);

        assert_eq!(any.type_url, "type.googleapis.com/test.Message");
        assert!(any.is_message::<Message>());
        assert!(!any.is_message::<Other>());
        assert_eq!(any.parse::<Message>().unwrap(), msg);
        println!("{:?}", any.parse::<Other>().unwrap_err());
    }
}
