// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the shutdown IC.

use mesh::MeshPayload;
use mesh::rpc::Rpc;
use vm_resource::ResourceId;
use vm_resource::kind::VmbusDeviceHandleKind;

/// A handle to a shutdown IC.
#[derive(MeshPayload)]
pub struct ShutdownIcHandle {
    /// The channel by which to receive shutdown requests.
    pub recv: mesh::Receiver<ShutdownRpc>,
}

impl ResourceId<VmbusDeviceHandleKind> for ShutdownIcHandle {
    const ID: &'static str = "shutdown_ic";
}

/// An RPC request to the shutdown IC.
#[derive(MeshPayload)]
pub enum ShutdownRpc {
    /// Wait for the shutdown IC to be ready.
    ///
    /// Returns a receiver that closes when the IC is no longer ready.
    ///
    /// FUTURE: this should instead be a `Sender` that is used to send the
    /// shutdown request. Do this once `Sender` has a mechanism to wait on the
    /// receiver to be closed.
    WaitReady(Rpc<(), mesh::OneshotReceiver<()>>),
    /// Send a shutdown request to the guest.
    ///
    /// Waits for the guest acknowledgment, not completion of the power-state change.
    /// Stopping the caller's wait does not withdraw a command already sent to the
    /// guest. The device continues waiting for acknowledgment and rejects further
    /// shutdown requests with [`ShutdownResult::AlreadyInProgress`].
    Shutdown(Rpc<ShutdownParams, ShutdownResult>),
}

/// Guest shutdown parameters.
#[derive(Debug, MeshPayload)]
pub struct ShutdownParams {
    /// The type of power state change.
    pub shutdown_type: ShutdownType,
    /// Whether to force a shutdown.
    pub force: bool,
    /// Guest-visible text, limited to 1023 UTF-16 code units without embedded NULs.
    pub message: ShutdownText,
}

/// Well-formed guest-visible UTF-16 text that fits the shutdown protocol buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShutdownText(widestring::Utf16String);

impl ShutdownText {
    /// Maximum number of UTF-16 code units, excluding the wire NUL terminator.
    pub const MAX_CODE_UNITS: usize = 1023;

    /// Constructs text without NULs that fits the shutdown protocol buffer.
    pub fn new(text: &str) -> Result<Self, ShutdownTextError> {
        if text.encode_utf16().take(Self::MAX_CODE_UNITS + 1).count() > Self::MAX_CODE_UNITS {
            return Err(ShutdownTextError::TooLong);
        }
        if text.contains('\0') {
            return Err(ShutdownTextError::ContainsNul);
        }
        Ok(Self(widestring::Utf16String::from_str(text)))
    }

    /// Constructs text from UTF-16 code units, without a NUL terminator.
    pub fn from_utf16(text: &[u16]) -> Result<Self, ShutdownTextError> {
        if text.len() > Self::MAX_CODE_UNITS {
            return Err(ShutdownTextError::TooLong);
        }
        if text.contains(&0) {
            return Err(ShutdownTextError::ContainsNul);
        }
        Ok(Self(widestring::Utf16Str::from_slice(text)?.to_owned()))
    }

    /// Constructs text from a NUL-terminated protocol buffer, ignoring padding.
    pub fn from_terminated_utf16(text: &[u16]) -> Result<Self, ShutdownTextError> {
        let text = widestring::U16CStr::from_slice_truncate(text)
            .map_err(|_| ShutdownTextError::MissingTerminator)?;
        Self::from_utf16(text.as_slice())
    }

    /// Returns UTF-16 code units without a NUL terminator.
    pub fn as_utf16(&self) -> &[u16] {
        self.0.as_slice()
    }
}

impl std::fmt::Display for ShutdownText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Invalid guest-visible shutdown text.
#[derive(Debug, thiserror::Error)]
pub enum ShutdownTextError {
    /// The text exceeds the shutdown protocol buffer.
    #[error("shutdown text exceeds 1023 UTF-16 code units")]
    TooLong,
    /// The text contains a NUL code unit.
    #[error("shutdown text contains a NUL")]
    ContainsNul,
    /// The protocol buffer does not contain a NUL terminator.
    #[error("shutdown text is missing its NUL terminator")]
    MissingTerminator,
    /// The text contains malformed UTF-16.
    #[error(transparent)]
    InvalidUtf16(#[from] widestring::error::Utf16Error),
}

mod text_encoding {
    use super::ShutdownText;
    use mesh::payload::encoding::StringField;
    use mesh::payload::inplace::InplaceOption;
    use mesh::payload::protobuf::{FieldReader, FieldSizer, FieldWriter};
    use mesh::payload::protofile::{DescribeField, FieldType};
    use mesh::payload::{DefaultEncoding, Error, FieldDecode, FieldEncode};

    pub struct ShutdownTextEncoding;

    impl DefaultEncoding for ShutdownText {
        type Encoding = ShutdownTextEncoding;
    }

    impl DescribeField<ShutdownText> for ShutdownTextEncoding {
        const FIELD_TYPE: FieldType<'static> = FieldType::builtin("string");
    }

    impl<Resource> FieldEncode<ShutdownText, Resource> for ShutdownTextEncoding {
        fn write_field(text: ShutdownText, writer: FieldWriter<'_, '_, Resource>) {
            StringField::write_field(text.to_string(), writer);
        }

        fn compute_field_size(text: &mut ShutdownText, sizer: FieldSizer<'_>) {
            sizer.bytes(text.0.chars().map(char::len_utf8).sum());
        }
    }

    impl<'a, Resource> FieldDecode<'a, ShutdownText, Resource> for ShutdownTextEncoding {
        fn read_field(
            text: &mut InplaceOption<'_, ShutdownText>,
            reader: FieldReader<'a, '_, Resource>,
        ) -> mesh::payload::Result<()> {
            let value = std::str::from_utf8(reader.bytes()?).map_err(Error::new)?;
            text.set(ShutdownText::new(value).map_err(Error::new)?);
            Ok(())
        }

        fn default_field(text: &mut InplaceOption<'_, ShutdownText>) -> mesh::payload::Result<()> {
            text.set(ShutdownText::default());
            Ok(())
        }
    }
}

/// The shutdown type.
#[derive(Debug, MeshPayload)]
pub enum ShutdownType {
    /// Power off the VM.
    PowerOff,
    /// Reboot the VM.
    Reboot,
    /// Hibernate the VM.
    Hibernate,
}

/// The result of a shutdown request.
#[derive(MeshPayload, Debug, PartialEq)]
pub enum ShutdownResult {
    /// The shutdown has been initiated.
    Ok,
    /// The IC is not ready to send shutdown requests.
    NotReady,
    /// A shutdown is already in progress.
    AlreadyInProgress,
    /// The shutdown failed with the given error code.
    Failed(u32),
}

#[cfg(test)]
mod tests {
    use super::{ShutdownParams, ShutdownText, ShutdownTextError, ShutdownType};
    use mesh::payload::Protobuf;
    use test_with_tracing::test;

    #[test]
    fn shutdown_text_bounds() {
        for text in [
            String::new(),
            "a".repeat(1023),
            format!("{}a", "\u{1f600}".repeat(511)),
        ] {
            let message = ShutdownText::new(&text).unwrap();
            assert_eq!(message.to_string(), text);
            assert_eq!(
                ShutdownText::from_utf16(message.as_utf16()).unwrap(),
                message
            );
        }
        for text in ["a".repeat(1024), "\u{1f600}".repeat(512)] {
            assert!(matches!(
                ShutdownText::new(&text),
                Err(ShutdownTextError::TooLong)
            ));
        }
        for text in ["\0", "a\0b", "a\0"] {
            assert!(matches!(
                ShutdownText::new(text),
                Err(ShutdownTextError::ContainsNul)
            ));
        }
        assert!(matches!(
            ShutdownText::from_utf16(&[0xd800]),
            Err(ShutdownTextError::InvalidUtf16(_))
        ));
        assert!(matches!(
            ShutdownText::from_utf16(&[0xdc00]),
            Err(ShutdownTextError::InvalidUtf16(_))
        ));
        assert!(matches!(
            ShutdownText::from_utf16(&[0]),
            Err(ShutdownTextError::ContainsNul)
        ));
        assert!(matches!(
            ShutdownText::from_utf16(&[65; 1024]),
            Err(ShutdownTextError::TooLong)
        ));
        assert_eq!(
            ShutdownText::from_terminated_utf16(&[0, 0xd800]).unwrap(),
            ShutdownText::default()
        );
        assert_eq!(
            ShutdownText::from_terminated_utf16(&[65, 0, 66])
                .unwrap()
                .to_string(),
            "A"
        );
        assert!(matches!(
            ShutdownText::from_terminated_utf16(&[65; 1024]),
            Err(ShutdownTextError::MissingTerminator)
        ));
        assert!(matches!(
            ShutdownText::from_terminated_utf16(&[0xd800, 0]),
            Err(ShutdownTextError::InvalidUtf16(_))
        ));
    }

    #[test]
    fn shutdown_text_serialization() {
        #[derive(Protobuf)]
        struct LegacyText {
            #[mesh(1)]
            message: String,
        }
        #[derive(Protobuf)]
        struct Text {
            #[mesh(1)]
            message: ShutdownText,
        }
        for value in [
            String::new(),
            "text".into(),
            format!("{}a", "\u{1f600}".repeat(511)),
        ] {
            let legacy = mesh::payload::encode(LegacyText {
                message: value.clone(),
            });
            let text: Text = mesh::payload::decode(&legacy).unwrap();
            assert_eq!(text.message.to_string(), value);
            let encoded = mesh::payload::encode(text);
            assert_eq!(encoded, legacy);
            let decoded: LegacyText = mesh::payload::decode(&encoded).unwrap();
            assert_eq!(decoded.message, value);
        }
        assert_eq!(
            mesh::payload::decode::<Text>(&[]).unwrap().message,
            ShutdownText::default()
        );
        for value in [
            "a".repeat(1024),
            "\u{1f600}".repeat(512),
            "a\0b".into(),
            "a\0".into(),
        ] {
            let encoded = mesh::payload::encode(LegacyText { message: value });
            assert!(mesh::payload::decode::<Text>(&encoded).is_err());
        }
        assert!(mesh::payload::decode::<Text>(&[0x0a, 1, 0xff]).is_err());
    }

    #[test]
    fn shutdown_params_serialization() {
        #[derive(mesh::MeshPayload)]
        struct PreviousParams {
            shutdown_type: ShutdownType,
            force: bool,
            message: String,
        }
        let message = "maintenance \u{1f680}";
        let (data, resources) = mesh::Message::new(PreviousParams {
            shutdown_type: ShutdownType::Reboot,
            force: true,
            message: message.into(),
        })
        .serialize();
        assert!(resources.is_empty());
        let params: ShutdownParams = mesh::Message::serialized(&data, resources).parse().unwrap();
        assert!(matches!(params.shutdown_type, ShutdownType::Reboot));
        assert!(params.force);
        assert_eq!(params.message.to_string(), message);
        assert_eq!(mesh::Message::new(params).serialize().0, data);
        let (data, resources) = mesh::Message::new(PreviousParams {
            shutdown_type: ShutdownType::PowerOff,
            force: false,
            message: "invalid\0text".into(),
        })
        .serialize();
        assert!(
            mesh::Message::serialized(&data, resources)
                .parse::<ShutdownParams>()
                .is_err()
        );
    }
}
