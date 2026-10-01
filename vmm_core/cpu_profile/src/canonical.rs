// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Canonical JSON and its SHA-256 digests.
//!
//! The canonical form sorts object keys by their UTF-8 bytes and keeps array
//! order, so the producer is responsible for ordering arrays. The compact form
//! has no insignificant whitespace and is the input of every digest. The
//! pretty form indents by two spaces and is what fingerprints are written as.
//! Both are independent of how `serde_json` orders map keys.

use serde::Serialize;
use serde_json::Value;
use sha2::Digest;

/// The prefix of every digest string.
pub(crate) const DIGEST_PREFIX: &str = "sha256:";

/// Returns the compact canonical encoding of `value`.
pub(crate) fn to_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None, 0);
    out
}

/// Returns the compact canonical encoding of a value whose serialization is
/// already canonical: every object's fields are serialized in the byte order
/// of their keys, and no map is unordered.
///
/// This serializes directly, without the [`Value`] tree that [`to_compact`]
/// sorts, which costs a few hundred microseconds per profile on every
/// OpenVMM start. Tests check that both give the same bytes for every type
/// that uses it.
pub(crate) fn to_compact_ordered<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("serialization with string keys is infallible")
}

/// Returns the pretty canonical encoding of a value whose serialization is
/// already canonical, as for [`to_compact_ordered`], with a final newline.
pub(crate) fn to_pretty_ordered<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out =
        serde_json::to_string_pretty(value).expect("serialization with string keys is infallible");
    out.push('\n');
    out
}

/// Returns the pretty canonical encoding of `value`, with a final newline.
pub(crate) fn to_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some(2), 0);
    out.push('\n');
    out
}

/// Returns `sha256:` and the hex SHA-256 of the compact canonical encoding of
/// `value`.
pub(crate) fn digest(value: &Value) -> String {
    format_digest(&sha256(to_compact(value).as_bytes()))
}

/// Returns the SHA-256 of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

/// Returns `sha256:` and the lowercase hex of `hash`.
pub(crate) fn format_digest(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(DIGEST_PREFIX.len() + 64);
    out.push_str(DIGEST_PREFIX);
    for byte in hash {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap());
        out.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap());
    }
    out
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => out.push_str(&value.to_string()),
        Value::String(value) => write_string(out, value),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                write_newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            write_newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                write_newline(out, indent, depth + 1);
                write_string(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, depth + 1);
            }
            write_newline(out, indent, depth);
            out.push('}');
        }
    }
}

fn write_newline(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(indent) = indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', indent * depth));
    }
}

fn write_string(out: &mut String, value: &str) {
    // serde_json escapes deterministically, and serializing a string cannot
    // fail.
    out.push_str(&serde_json::to_string(value).expect("string serialization is infallible"));
}

#[cfg(test)]
mod tests {
    use super::digest;
    use super::to_compact;
    use super::to_pretty;
    use serde_json::json;
    use test_with_tracing::test;

    #[test]
    fn sorts_object_keys_and_keeps_array_order() {
        let value = json!({"b": [3, 1, {"z": null, "a": true}], "a": "x", "B": {}});
        assert_eq!(
            to_compact(&value),
            r#"{"B":{},"a":"x","b":[3,1,{"a":true,"z":null}]}"#
        );
    }

    #[test]
    fn pretty_form_is_indented_and_terminated() {
        let value = json!({"b": [1, 2], "a": {"c": "d"}, "e": [], "f": {}});
        assert_eq!(
            to_pretty(&value),
            "{\n  \"a\": {\n    \"c\": \"d\"\n  },\n  \"b\": [\n    1,\n    2\n  ],\n  \"e\": [],\n  \"f\": {}\n}\n"
        );
    }

    #[test]
    fn escapes_strings() {
        let value = json!({"k\"ey": "line\nbreak\u{1}"});
        assert_eq!(to_compact(&value), r#"{"k\"ey":"line\nbreak\u0001"}"#);
    }

    #[test]
    fn digest_is_sha256_of_compact_form() {
        // SHA-256 of `{}`.
        assert_eq!(
            digest(&json!({})),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        // Key order does not change the digest.
        assert_eq!(
            digest(&json!({"a": 1, "b": 2})),
            digest(&serde_json::from_str(r#"{"b": 2, "a": 1}"#).unwrap())
        );
    }

    #[test]
    fn digest_strings_are_prefixed_lowercase_hex() {
        let text = super::format_digest(&super::sha256(b"{}"));
        assert_eq!(text, digest(&json!({})));
    }
}
