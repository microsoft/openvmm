// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serde de/serialization helpers. To use one of these helpers, use the `with`
//! serde attribute: `#[serde(with = "serde_helpers::foo")]`

mod prelude {
    pub use serde::Deserialize;
    pub use serde::Deserializer;
    pub use serde::Serialize;
    pub use serde::Serializer;
}

/// de/serialize a `Vec<u8>` to/from a base64 encoded string.
pub mod base64_vec {
    use crate::prelude::*;
    use base64::Engine;

    #[allow(clippy::ptr_arg)] // required by serde
    pub fn serialize<S: Serializer>(v: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s: &str = Deserialize::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

/// de/serialize a `T: Display + FromStr`
pub mod as_string {
    use crate::prelude::*;
    use std::fmt::Display;
    use std::str::FromStr;

    pub fn serialize<T: Display, S: Serializer>(value: &T, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(value)
    }

    pub fn deserialize<'de, T, D: Deserializer<'de>>(d: D) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: std::error::Error,
    {
        let s: &str = Deserialize::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// serialize a `T: Debug`
pub mod as_debug {
    use crate::prelude::*;
    use std::fmt::Debug;

    pub fn serialize<T: Debug, S: Serializer>(value: &T, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(&format_args!("{:?}", value))
    }
}

/// de/serialize an [`Option<Guid>`](guid::Guid) to/from a possibly-missing GUID
/// string. Make sure to also specify `#[serde(default)]`.
pub mod opt_guid_str {
    use crate::prelude::*;
    use guid::Guid;

    pub fn serialize<S: Serializer>(guid: &Option<Guid>, ser: S) -> Result<S::Ok, S::Error> {
        match guid {
            Some(guid) => ser.serialize_some(&guid.to_string()),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Guid>, D::Error> {
        let s: Option<&str> = Deserialize::deserialize(d)?;
        Ok(match s {
            None => None,
            Some(s) => Some(s.parse().map_err(serde::de::Error::custom)?),
        })
    }
}

/// de/serialize an `Option<T>` (where `T` implements is de/serializable)
/// to/from a `base64` encoded string containing a JSON payload.
/// Make sure to also specify `#[serde(default)]`.
pub mod opt_base64_json {
    use crate::prelude::*;
    use base64::Engine;

    pub fn serialize<S: Serializer, T: Serialize>(
        t: &Option<T>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => ser.serialize_str(
                &base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_string(t).map_err(serde::ser::Error::custom)?),
            ),
            None => ser.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        let s: Option<&str> = Deserialize::deserialize(d)?;
        Ok(match s {
            None => None,
            Some(s) => {
                let s = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(serde::de::Error::custom)?;
                Some(serde_json::from_slice(&s).map_err(serde::de::Error::custom)?)
            }
        })
    }
}

pub mod opt_base64_vec {
    use base64::Engine;
    use serde::*;

    #[allow(clippy::ptr_arg)] // required by serde
    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, ser: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => ser.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v)),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let s: Option<&str> = Deserialize::deserialize(d)?;
        Ok(match s {
            Some(s) => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(de::Error::custom)?,
            ),
            None => None,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use guid::Guid;
    use serde::Deserialize;

    #[test]
    fn opt_guid() {
        #[derive(Deserialize)]
        struct OptGuidSample {
            field1: u32,
            #[serde(default)]
            #[serde(with = "opt_guid_str")]
            field2: Option<Guid>,
        }

        let json = r#"
        {
            "field1": 123,
            "field2": "12345678-9abc-def0-1234-56789abcdef0"
        }"#;
        let guid_sample: OptGuidSample = serde_json::from_str(json).expect("deserialization");
        assert_eq!(guid_sample.field1, 123);
        assert_eq!(
            guid_sample.field2.expect("field2 set"),
            Guid::from_static_str("12345678-9abc-def0-1234-56789abcdef0")
        );

        let json = r#"
        {
            "field1": 123
        }"#;
        let guid_sample: OptGuidSample = serde_json::from_str(json).expect("deserialization");
        assert_eq!(guid_sample.field1, 123);
        assert!(guid_sample.field2.is_none());
    }
}
