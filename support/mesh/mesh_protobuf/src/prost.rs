// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for encoding Prost types as Mesh types.

use super::inplace::InplaceOption;
use super::protobuf;
use super::MessageDecode;
use super::MessageEncode;
use super::Result;
use crate::Error;

/// Encoding for using Prost messages as Mesh messages.
pub struct ProstMessage;

impl<T: prost::Message + Default, R> MessageEncode<T, R> for ProstMessage {
    fn write_message(item: T, mut writer: protobuf::MessageWriter<'_, '_, R>) {
        let mut v = Vec::with_capacity(item.encoded_len());
        item.encode(&mut v).unwrap();
        writer.bytes(&v);
    }

    fn compute_message_size(item: &mut T, mut sizer: protobuf::MessageSizer<'_>) {
        sizer.bytes(item.encoded_len())
    }
}

impl<T: prost::Message + Default, R> MessageDecode<'_, T, R> for ProstMessage {
    fn read_message(
        item: &mut InplaceOption<'_, T>,
        reader: protobuf::MessageReader<'_, '_, R>,
    ) -> Result<()> {
        match item.as_mut() {
            Some(item) => item.merge(reader.bytes()),
            None => T::decode(reader.bytes()).map(|m| {
                item.set(m);
            }),
        }
        .map_err(Error::new)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::SerializedMessage;

    mod items {
        // Crates used by generated code. Reference them explicitly to ensure that
        // automated tools do not remove them.
        use prost as _;
        use prost_types as _;

        include!(concat!(env!("OUT_DIR"), "/foo.rs"));
    }

    #[test]
    fn prost() {
        let foo = items::Foo {
            bar: "foo".to_string(),
            baz: 0,
            stuff: Some(items::foo::Stuff::Abc(5)),
        };
        let foo2 = <SerializedMessage>::from_message(foo.clone())
            .into_message()
            .unwrap();
        assert_eq!(foo, foo2);
    }
}
