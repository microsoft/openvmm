// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Underhill configuration schema
//!
//! Generic schema structs and functions

use self::v1::NAMESPACE_BASE;
use self::v1::NAMESPACE_NETWORK_ACCELERATION;
use self::v1::NAMESPACE_NETWORK_DEVICE;
use crate::Vtl2SettingsErrorInfoVec;
use crate::errors::ParseErrors;
use crate::errors::ParseErrorsBase;
use crate::errors::ParseResultExt;
use crate::errors::ParsingStopped;
use thiserror::Error;
use vtl2_settings_proto::*;

mod v1;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("json parsing failed")]
    Json(#[source] serde_json::Error),
    #[error("protobuf parsing failed")]
    Protobuf(#[source] prost::DecodeError),
    #[error("validation failed")]
    Validation(#[source] Vtl2SettingsErrorInfoVec),
}

enum ParseErrorInner {
    Parse(ParseError),
    Validation(ParsingStopped),
}

impl From<ParseError> for ParseErrorInner {
    fn from(value: ParseError) -> Self {
        ParseErrorInner::Parse(value)
    }
}

impl From<ParsingStopped> for ParseErrorInner {
    fn from(value: ParsingStopped) -> Self {
        ParseErrorInner::Validation(value)
    }
}

impl crate::Vtl2Settings {
    /// Reads the settings from either a JSON- or protobuf-encoded schema.
    pub fn read_from(
        data: &[u8],
        old_settings: crate::Vtl2Settings,
    ) -> Result<crate::Vtl2Settings, ParseError> {
        let mut base = ParseErrorsBase::new();
        let mut errors = base.root();
        match Self::read_from_inner(data, old_settings, &mut errors) {
            Ok(v) => {
                base.result().map_err(ParseError::Validation)?;
                Ok(v)
            }
            Err(ParseErrorInner::Parse(err)) => Err(err),
            Err(ParseErrorInner::Validation(err)) => {
                Err::<(), _>(err).collect_error(&mut errors);
                Err(ParseError::Validation(base.result().unwrap_err()))
            }
        }
    }

    fn read_from_inner(
        data: &[u8],
        old_settings: crate::Vtl2Settings,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::Vtl2Settings, ParseErrorInner> {
        let mut old_settings = old_settings;

        let decoded: Vtl2Settings = Self::read(data)?;

        let mut has_base: bool = decoded.fixed.is_some() || decoded.dynamic.is_some();

        // backwards compatibility
        if has_base {
            v1::validate_version(decoded.version, errors)?;
        }

        let mut fixed = Self::read_fixed(decoded.fixed.unwrap_or_default(), errors)?;
        let mut dynamic = decoded.dynamic.unwrap_or_default().parse(errors)?;

        let mut nic_devices: Option<Vec<crate::NicDevice>> = None;
        let mut nic_acceleration: Option<Vec<crate::NicDevice>> = None;
        for chunk in &decoded.namespace_settings {
            if chunk.settings.is_empty() {
                errors.push(v1::Error::EmptyNamespaceChunk(chunk.namespace.as_ref()));
            }
            match chunk.namespace.as_str() {
                NAMESPACE_BASE => {
                    has_base = true;
                    let base: Vtl2SettingsBase = Self::read(&chunk.settings)?;
                    v1::validate_version(base.version, errors)?;
                    fixed = Self::read_fixed(base.fixed.unwrap_or_default(), errors)?;
                    dynamic = base.dynamic.unwrap_or_default().parse(errors)?;
                }
                NAMESPACE_NETWORK_DEVICE => {
                    let settings: Vtl2SettingsNetworkDevice = Self::read(&chunk.settings)?;
                    nic_devices = Some(
                        settings
                            .nic_devices
                            .iter()
                            .flat_map(|v| v.parse(errors).collect_error(errors))
                            .collect(),
                    );
                }
                NAMESPACE_NETWORK_ACCELERATION => {
                    let settings: Vtl2SettingsNetworkAcceleration = Self::read(&chunk.settings)?;
                    nic_acceleration = Some(
                        settings
                            .nic_acceleration
                            .iter()
                            .flat_map(|v| v.parse(errors).collect_error(errors))
                            .collect(),
                    );
                }
                _ => {
                    errors.push(v1::Error::UnsupportedSchemaNamespace(
                        chunk.namespace.as_ref(),
                    ));
                }
            }
        }

        // NAMESPACE_BASE
        if has_base {
            old_settings.fixed = fixed;
            let old_nic_devices = std::mem::take(&mut old_settings.dynamic.nic_devices);
            old_settings.dynamic = dynamic;
            // If new network information is not present, do nothing. This handles the
            // case where the base namespace is modified for non-networking reasons
            // (e.g. storage), without adding current network information.
            if old_settings.dynamic.nic_devices.is_empty() && nic_devices.is_none() {
                old_settings.dynamic.nic_devices = old_nic_devices;
            }
        }

        // NAMESPACE_NETWORK_DEVICE
        if let Some(nic_devices) = nic_devices {
            old_settings.dynamic.nic_devices = nic_devices;
        }

        // NAMESPACE_NETWORK_ACCELERATION
        if let Some(nic_acceleration) = nic_acceleration {
            // From the nic acceleration namespace, only process those instances which were
            // originally specified.
            for acc in nic_acceleration.iter() {
                for nic in old_settings.dynamic.nic_devices.iter_mut() {
                    if nic.instance_id == acc.instance_id {
                        nic.subordinate_instance_id = acc.subordinate_instance_id;
                        break;
                    }
                }
            }
        }

        Ok(old_settings)
    }

    fn read_fixed(
        fixed: Vtl2SettingsFixed,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::Vtl2SettingsFixed, ParseErrorInner> {
        let mut settings: crate::Vtl2SettingsFixed = fixed.parse(errors)?;
        if let Some(raw_attested_settings) = fixed
            .attested_settings
            .as_deref()
            .filter(|bytes| !bytes.is_empty())
        {
            let attested_settings: Vtl2AttestedSettings = Self::read(raw_attested_settings)?;
            settings.attested_settings = Some(crate::Vtl2AttestedSettings {
                version: attested_settings.version,
                init_data_hash: Some(openssl::sha::sha256(raw_attested_settings)),
            });
        }
        Ok(settings)
    }

    fn read<'a, T>(data: &'a [u8]) -> Result<T, ParseError>
    where
        T: Default,
        T: prost::Message,
        T: serde::Deserialize<'a>,
    {
        // Detect JSON vs. protobuf by looking for an opening
        // brace by skipping whitespaces. This is mostly safe* (see below) because
        // we reserve the protobuf field numbers that would conflict with this detection.
        let idx = data.iter().position(|&b| !b.is_ascii_whitespace());
        let is_json = match idx {
            Some(idx) => data[idx] == b'{',
            None => false,
        };
        let decoded: T = if is_json {
            // *: in very rare cases, the message might be protobuf but
            // LEN-encoded with a length of exactly 0x7b aka '{' which
            // will take this branch and cause a failure. To address this,
            // if the JSON parse fails attempt a protobuf parse before failing.
            // Preserve the original JSON parse error to return in case the
            // protobuf parse fails.
            match serde_json::from_slice(data).map_err(ParseError::Json) {
                Ok(json) => json,
                Err(json_parse_err) => {
                    match prost::Message::decode(data).map_err(ParseError::Protobuf) {
                        Ok(protobuf) => protobuf,
                        Err(_) => return Err(json_parse_err),
                    }
                }
            }
        } else {
            prost::Message::decode(data).map_err(ParseError::Protobuf)?
        };

        Ok(decoded)
    }
}

impl Default for crate::Vtl2SettingsFixed {
    fn default() -> Self {
        Vtl2SettingsFixed::default()
            .parse(&mut ParseErrorsBase::new().root())
            .unwrap()
    }
}

impl Default for crate::Vtl2SettingsDynamic {
    fn default() -> Self {
        Vtl2SettingsDynamic::default()
            .parse(&mut ParseErrorsBase::new().root())
            .unwrap()
    }
}

/// Convert scheme structs to config structs.
pub(crate) trait ParseSchema<T>: Sized {
    /// Parse the schema into a config struct.
    ///
    /// If possible, the parser should try to continue parsing after
    /// encountering an error, pushing errors into `errors`. If the parser
    /// cannot continue parsing, it should return a [`ParsingStopped`] error.
    fn parse_schema(&self, errors: &mut ParseErrors<'_>) -> Result<T, ParsingStopped>;
}

/// Extension trait on schema types to parse them into config types.
///
/// This is useful over `ParseSchema<T>` so that you can use turbo-fish syntax
/// to specify the type to parse into.
pub(crate) trait ParseSchemaExt {
    /// Parse the schema into a config struct.
    fn parse<T>(&self, errors: &mut ParseErrors<'_>) -> Result<T, ParsingStopped>
    where
        Self: ParseSchema<T>;
}

impl<T> ParseSchemaExt for T {
    fn parse<U>(&self, errors: &mut ParseErrors<'_>) -> Result<U, ParsingStopped>
    where
        T: ParseSchema<U>,
    {
        self.parse_schema(errors)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Vtl2SettingsErrorCode;
    use crate::Vtl2SettingsErrorInfo;
    use guid::Guid;
    use prost::Message;

    #[test]
    fn smoke_test_sample() {
        // { "version": "V1" }
        let json = b"{ \"version\": \"V1\" }";
        crate::Vtl2Settings::read_from(json, Default::default()).unwrap();
    }

    #[test]
    fn smoke_test_namespace() {
        let json = include_bytes!("vtl2s_test_namespace.json");
        crate::Vtl2Settings::read_from(json, Default::default()).unwrap();
    }

    #[test]
    fn smoke_test_namespace_mix_protobuf_json() {
        let json = include_bytes!("vtl2s_test_namespace.json");
        let settings: Vtl2Settings = crate::Vtl2Settings::read(json).unwrap();
        // read only decode the top level payload, namespace settings chunk keeps its encoding
        let base_json: &[u8] = &settings.namespace_settings[0].settings;
        assert_eq!(base_json, include_bytes!("vtl2s_test_json.json"));
        let mut buf = Vec::new();
        settings.encode(&mut buf).unwrap();
        crate::Vtl2Settings::read_from(&buf, Default::default()).unwrap();
    }

    #[test]
    fn smoke_test_attested_settings_in_base_namespace() {
        let settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_attested_settings_base.json"),
            Default::default(),
        )
        .unwrap();
        let attested_settings = settings.fixed.attested_settings.unwrap();
        assert_eq!(1, attested_settings.version);
        assert_eq!(
            Some([
                0xa9, 0xc6, 0x34, 0xb8, 0x84, 0x64, 0xae, 0xd6, 0x9d, 0x67, 0xaf, 0xeb, 0xd6, 0x84,
                0x4f, 0x56, 0xfd, 0xa3, 0x85, 0xe8, 0x99, 0xaf, 0xf9, 0xb1, 0x40, 0xb3, 0x74, 0x98,
                0xc6, 0x56, 0xc8, 0xa3,
            ]),
            attested_settings.init_data_hash
        );
    }

    #[test]
    fn smoke_test_attested_settings_in_base_namespace_pb() {
        let mut raw_attested_settings = Vec::new();
        Vtl2AttestedSettings { version: 1 }
            .encode(&mut raw_attested_settings)
            .unwrap();
        let expected_hash = openssl::sha::sha256(&raw_attested_settings);

        let base = Vtl2SettingsBase {
            version: 1,
            fixed: Some(Vtl2SettingsFixed {
                attested_settings: Some(raw_attested_settings),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut raw_base = Vec::new();
        base.encode(&mut raw_base).unwrap();

        let settings = Vtl2Settings {
            namespace_settings: vec![Vtl2SettingsChunk {
                namespace: NAMESPACE_BASE.into(),
                settings: raw_base,
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        settings.encode(&mut buf).unwrap();

        let settings = crate::Vtl2Settings::read_from(&buf, Default::default()).unwrap();
        let attested_settings = settings.fixed.attested_settings.unwrap();
        assert_eq!(1, attested_settings.version);
        assert_eq!(Some(expected_hash), attested_settings.init_data_hash);
    }

    fn attested_settings_inputs(payload: Option<&[u8]>) -> Vec<Vec<u8>> {
        let fixed = Vtl2SettingsFixed {
            attested_settings: payload.map(|bytes| bytes.to_vec()),
            ..Default::default()
        };
        let legacy = Vtl2Settings {
            version: 1,
            fixed: Some(fixed.clone()),
            ..Default::default()
        };
        let mut inputs = vec![legacy.encode_to_vec(), serde_json::to_vec(&legacy).unwrap()];
        let base = Vtl2SettingsBase {
            version: 1,
            fixed: Some(fixed),
            ..Default::default()
        };
        for raw_base in [base.encode_to_vec(), serde_json::to_vec(&base).unwrap()] {
            let namespaced = Vtl2Settings {
                namespace_settings: vec![Vtl2SettingsChunk {
                    namespace: NAMESPACE_BASE.into(),
                    settings: raw_base,
                }],
                ..Default::default()
            };
            inputs.push(namespaced.encode_to_vec());
            inputs.push(serde_json::to_vec(&namespaced).unwrap());
        }
        inputs
    }

    #[test]
    fn attested_settings_hashes_original_json_bytes() {
        let payloads: &[&[u8]] = &[br#"{"version":"V1"}"#, b"{ \"version\": \"V1\" }\n"];
        for &payload in payloads {
            for input in attested_settings_inputs(Some(payload)) {
                let settings = crate::Vtl2Settings::read_from(&input, Default::default()).unwrap();
                let attested_settings = settings.fixed.attested_settings.unwrap();
                assert_eq!(1, attested_settings.version);
                assert_eq!(
                    Some(openssl::sha::sha256(payload)),
                    attested_settings.init_data_hash
                );
            }
        }
    }

    #[test]
    fn attested_settings_hashes_original_protobuf_bytes() {
        // Unknown fields must remain part of the hash even though decoding drops them.
        let payloads: &[&[u8]] = &[&[0x08, 0x01], &[0x10, 0x2a, 0x08, 0x01]];
        for &payload in payloads {
            for input in attested_settings_inputs(Some(payload)) {
                let settings = crate::Vtl2Settings::read_from(&input, Default::default()).unwrap();
                let attested_settings = settings.fixed.attested_settings.unwrap();
                assert_eq!(1, attested_settings.version);
                assert_eq!(
                    Some(openssl::sha::sha256(payload)),
                    attested_settings.init_data_hash
                );
            }
        }
    }

    #[test]
    fn attested_settings_rejects_malformed_json() {
        for input in attested_settings_inputs(Some(br#"{"version":"#)) {
            assert!(matches!(
                crate::Vtl2Settings::read_from(&input, Default::default()),
                Err(ParseError::Json(_))
            ));
        }
    }

    #[test]
    fn attested_settings_rejects_malformed_protobuf() {
        for input in attested_settings_inputs(Some(&[0x08])) {
            assert!(matches!(
                crate::Vtl2Settings::read_from(&input, Default::default()),
                Err(ParseError::Protobuf(_))
            ));
        }
    }

    #[test]
    fn attested_settings_absent_or_empty_remains_optional() {
        for payload in [None, Some(&[][..])] {
            for input in attested_settings_inputs(payload) {
                let settings = crate::Vtl2Settings::read_from(&input, Default::default()).unwrap();
                assert!(settings.fixed.attested_settings.is_none());
            }
        }
    }

    #[test]
    fn smoke_test_compat() {
        crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_compat.json"),
            Default::default(),
        )
        .unwrap();
    }

    #[test]
    fn validation_test_max_storage_controllers() {
        crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_max_storage_controllers.json"),
            Default::default(),
        )
        .unwrap();
    }

    fn stable_error_json(err: &Vtl2SettingsErrorInfo) -> String {
        let mut value = serde_json::to_value(err).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("file_name").unwrap();
        obj.remove("line").unwrap();
        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn validation_test_storage_controllers_exceeds_limits() {
        let err = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_storage_controllers_exceeds_limits.json"),
            Default::default(),
        )
        .unwrap_err();

        let ParseError::Validation(err) = err else {
            panic!("wrong error {err:?}")
        };
        let [err] = err.errors.try_into().unwrap();
        assert_eq!(
            err.code(),
            Vtl2SettingsErrorCode::StorageScsiControllerExceedsMaxLimits
        );
        let expected = r#"{"error_id":"Configuration.StorageScsiControllerExceedsMaxLimits","message":"exceeded 4 max SCSI controllers, instance ID: 0bf355d5-0cae-411e-9662-86c3035556ae"}"#;
        assert_eq!(stable_error_json(&err).as_str(), expected);
    }

    #[test]
    fn namespace_test_nic_namespaces() {
        /*
        NetworkDevice
        {
            "nic_devices": [
                {
                    "instance_id": "9e14fd10-19cb-4da5-b667-e8e38a436cb8"
                },
                {
                    "instance_id": "9e14fd11-19cb-4da5-b667-e8e38a436cb8"
                },
                {
                    "instance_id": "9e14fd12-19cb-4da5-b667-e8e38a436cb8"
                },
            ]
        }
        NetworkAcceleration
        {
            "nic_acceleration": [
                {
                    "instance_id": "9e14fd11-19cb-4da5-b667-e8e38a436cb8",
                    "subordinate_instance_id": "12345678-19cb-4da5-b667-e8e38a436cb8"
                },
                {
                    "instance_id": "9e14fd12-19cb-4da5-b667-e8e38a436cb8",
                    "subordinate_instance_id": "00000000-0000-0000-0000-000000000000"
                }
            ]
        }
        */

        let settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_nic_namespaces.json"),
            Default::default(),
        )
        .unwrap();

        assert_eq!(3, settings.dynamic.nic_devices.len());

        let nic0 = settings
            .dynamic
            .nic_devices
            .iter()
            .find(|nic| {
                nic.instance_id
                    == "9e14fd10-19cb-4da5-b667-e8e38a436cb8"
                        .parse::<Guid>()
                        .unwrap()
            })
            .unwrap();
        assert_eq!(true, nic0.subordinate_instance_id.is_none());

        let nic1 = settings
            .dynamic
            .nic_devices
            .iter()
            .find(|nic| {
                nic.instance_id
                    == "9e14fd11-19cb-4da5-b667-e8e38a436cb8"
                        .parse::<Guid>()
                        .unwrap()
            })
            .unwrap();
        assert_eq!(
            "12345678-19cb-4da5-b667-e8e38a436cb8"
                .parse::<Guid>()
                .unwrap(),
            nic1.subordinate_instance_id.unwrap()
        );

        let nic2 = settings
            .dynamic
            .nic_devices
            .iter()
            .find(|nic| {
                nic.instance_id
                    == "9e14fd12-19cb-4da5-b667-e8e38a436cb8"
                        .parse::<Guid>()
                        .unwrap()
            })
            .unwrap();
        assert_eq!(true, nic2.subordinate_instance_id.is_none());
    }

    #[test]
    fn namespace_test_empty_nic_devices_ignored_json() {
        let old_settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_nic_namespaces.json"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(3, old_settings.dynamic.nic_devices.len());

        let no_nic_settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_json_no_nic.json"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(0, no_nic_settings.dynamic.nic_devices.len());

        let settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_json_no_nic.json"),
            old_settings.clone(),
        )
        .unwrap();
        assert_eq!(3, settings.dynamic.nic_devices.len());
    }

    #[test]
    fn namespace_test_adding_nic_devices_json() {
        let old_settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_json_no_nic.json"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(0, old_settings.dynamic.nic_devices.len());

        let nic_settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_nic_namespaces.json"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(3, nic_settings.dynamic.nic_devices.len());

        let settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_nic_namespaces.json"),
            old_settings.clone(),
        )
        .unwrap();
        assert_eq!(3, settings.dynamic.nic_devices.len());
    }

    #[test]
    fn namespace_test_adding_nic_devices_protobuf() {
        let json = include_bytes!("vtl2s_test_nic_namespaces.json");
        let settings: Vtl2Settings = crate::Vtl2Settings::read(json).unwrap();
        assert_eq!("NetworkDevice", settings.namespace_settings[0].namespace);
        let mut buf = Vec::new();
        settings.encode(&mut buf).unwrap();
        let settings = crate::Vtl2Settings::read_from(&buf, Default::default()).unwrap();
        assert_eq!(3, settings.dynamic.nic_devices.len());
    }

    #[test]
    fn namespace_test_empty_nic_devices_protobuf() {
        let old_settings = crate::Vtl2Settings::read_from(
            include_bytes!("vtl2s_test_nic_namespaces.json"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(3, old_settings.dynamic.nic_devices.len());

        // Create protobuff of empty nic_devices[]
        let json = include_bytes!("vtl2s_test_json_no_nic.json");
        let empty_json_settings: Vtl2Settings = crate::Vtl2Settings::read(json).unwrap();
        let mut empty_protobuf = Vec::new();
        empty_json_settings.encode(&mut empty_protobuf).unwrap();

        // Create Vtl2Settings with NetworkDevices and no nic_devices
        let mut empty_vtl2_settings: Vtl2Settings =
            crate::Vtl2Settings::read(include_bytes!("vtl2s_test_nic_namespaces.json")).unwrap();
        empty_vtl2_settings.namespace_settings.pop(); // Get rid of NetworkAcceleration
        empty_vtl2_settings.namespace_settings[0].settings = empty_protobuf; // Empty nic_devices[]

        let mut buf = Vec::new();
        empty_vtl2_settings.encode(&mut buf).unwrap();
        let settings = crate::Vtl2Settings::read_from(&buf, old_settings).unwrap();
        assert_eq!(0, settings.dynamic.nic_devices.len());
    }
}
