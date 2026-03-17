// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for encoding various types into ACPI Machine Language (AML).

pub fn encode_name(name: &[u8]) -> Vec<u8> {
    let mut encoded_name: Vec<u8> = Vec::new();
    let mut segments: Vec<[u8; 4]> = Vec::new();
    let mut i = 0;
    if name[0] == b'\\' {
        encoded_name.push(b'\\');
        i = 1;
    }
    loop {
        if i == name.len() {
            break;
        }

        if name[i] == b'^' {
            assert!(
                (encoded_name.is_empty() || encoded_name[encoded_name.len() - 1] == b'^')
                    && segments.is_empty()
            );
            encoded_name.push(b'^');
            i += 1;
            continue;
        }

        assert!((name[i] == b'_') || (name[i] >= b'A' && name[i] <= b'Z'));
        let mut seg: [u8; 4] = [b'_'; 4];
        let mut seg_i = 0;
        loop {
            let off = i + seg_i;
            if off == name.len() || name[off] == b'.' {
                break;
            }

            assert!(seg_i < 4);
            assert!(
                (name[off] == b'_')
                    || (name[off] >= b'A' && name[off] <= b'Z')
                    || (name[off] >= b'0' && name[off] <= b'9')
            );
            seg[seg_i] = name[off];
            seg_i += 1
        }
        assert!(seg_i > 0);
        segments.push(seg);
        // advance past the last segment
        i += seg_i;
        if i < name.len() {
            // advance past the segment divider '.'
            i += 1;
        }
    }
    if segments.len() > 2 {
        encoded_name.push(0x2f);
        encoded_name.push(u8::try_from(segments.len()).unwrap());
    } else if segments.len() > 1 {
        encoded_name.push(0x2e);
    }
    for seg in segments {
        encoded_name.extend_from_slice(&seg);
    }
    encoded_name
}

pub fn encode_package_len(len: usize) -> Vec<u8> {
    assert!(len < (1 << 28) - 1);
    let mut result: Vec<u8> = Vec::new();
    if len < 63 {
        result.push(u8::try_from(len).unwrap() + 1);
    } else {
        // To store larger values, the length is stored in little-endian format, with the first byte encoding the
        // number of additional bytes as well as the least-significant nibble. With a maximum of three additional
        // bytes plus the extra nibble, the length can be up to 28 bits.
        let len_bytes = if len < 1 << 12 {
            2
        } else if len < 1 << 20 {
            3
        } else {
            4
        };

        let mut encoded_len: [u8; 4] = [0; 4];
        let mut rem = len + len_bytes;
        // byte count is in bits 6 and 7 and low nibble is in bits 0-3.
        encoded_len[0] =
            u8::try_from((len_bytes - 1) << 6).unwrap() | u8::try_from(rem & 0xf).unwrap();
        rem >>= 4;
        for e in encoded_len.iter_mut().take(len_bytes).skip(1) {
            *e = u8::try_from(rem & 0xff).unwrap();
            rem >>= 8;
        }

        result.extend_from_slice(&encoded_len[..len_bytes]);
    }
    result
}

pub fn encode_integer(value: u64) -> Vec<u8> {
    let mut byte_stream: Vec<u8> = Vec::new();
    let end;
    if value == 0 {
        // 0 has its own op
        return vec![0];
    } else if value == 1 {
        // 1 has its own op
        return vec![1];
    } else if value <= 0xff {
        byte_stream.push(0xa);
        end = 1;
    } else if value <= 0xffff {
        byte_stream.push(0xb);
        end = 2;
    } else if value <= 0xffffffff {
        byte_stream.push(0xc);
        end = 4;
    } else {
        byte_stream.push(0xe);
        end = 8;
    }

    let bytes = value.to_le_bytes();
    byte_stream.extend_from_slice(&bytes[..end]);
    byte_stream
}

pub fn encode_dword(value: u32) -> Vec<u8> {
    let mut byte_stream = vec![0xcu8];
    byte_stream.extend_from_slice(&value.to_le_bytes());
    while byte_stream.len() < 5 {
        byte_stream.push(0);
    }
    byte_stream
}

pub fn encode_string(value: &[u8]) -> Vec<u8> {
    let mut byte_stream: Vec<u8> = Vec::new();
    byte_stream.push(0xd);
    byte_stream.extend_from_slice(value);
    byte_stream.push(0);
    byte_stream
}

pub fn char_to_hex(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => 10 + value - b'a',
        b'A'..=b'F' => 10 + value - b'A',
        _ => panic!("Unsupported hex char {}", value),
    }
}

/// Encode an AML Arg reference (Arg0-Arg6: opcodes 0x68-0x6e).
pub fn encode_arg(n: u8) -> Vec<u8> {
    assert!(n < 7, "Arg index must be 0-6");
    vec![0x68 + n]
}

/// Encode an AML Local variable reference (Local0-Local7: opcodes 0x60-0x67).
pub fn encode_local(n: u8) -> Vec<u8> {
    assert!(n < 8, "Local index must be 0-7");
    vec![0x60 + n]
}

/// Encode a UUID in ACPI's byte-swapped format as an AML Buffer.
///
/// ACPI stores UUIDs with the first three groups byte-swapped (little-endian)
/// and the last two groups in network order. Input format: "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
pub fn encode_uuid_buffer(uuid: &str) -> Vec<u8> {
    let hex: Vec<u8> = uuid
        .bytes()
        .filter(|b| *b != b'-')
        .collect();
    assert_eq!(hex.len(), 32, "UUID must be 32 hex characters");

    let parse_byte = |i: usize| -> u8 {
        char_to_hex(hex[i]) << 4 | char_to_hex(hex[i + 1])
    };

    // ACPI UUID byte order: first 3 groups are LE, last 2 are BE
    let bytes: [u8; 16] = [
        // Group 1 (4 bytes, LE)
        parse_byte(6), parse_byte(4), parse_byte(2), parse_byte(0),
        // Group 2 (2 bytes, LE)
        parse_byte(10), parse_byte(8),
        // Group 3 (2 bytes, LE)
        parse_byte(14), parse_byte(12),
        // Group 4 (2 bytes, BE)
        parse_byte(16), parse_byte(18),
        // Group 5 (6 bytes, BE)
        parse_byte(20), parse_byte(22), parse_byte(24),
        parse_byte(26), parse_byte(28), parse_byte(30),
    ];

    use super::objects::Buffer;
    use super::objects::AmlObject;
    Buffer(bytes).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::test_helpers::verify_expected_bytes;

    #[test]
    fn verify_simple_name() {
        let bytes = encode_name(b"FOO");
        verify_expected_bytes(&bytes, b"FOO_");
    }

    #[test]
    fn verify_simple_name_with_root() {
        let bytes = encode_name(b"\\FOO");
        verify_expected_bytes(&bytes, b"\\FOO_");
    }

    #[test]
    fn verify_simple_name_with_prefix() {
        let bytes = encode_name(b"^FOO");
        verify_expected_bytes(&bytes, b"^FOO_");
    }

    #[test]
    fn verify_dual_name() {
        let bytes = encode_name(b"FOO.BAR");
        verify_expected_bytes(&bytes, b"\x2eFOO_BAR_");
    }

    #[test]
    fn verify_dual_name_with_root() {
        let bytes = encode_name(b"\\_SB.FOO");
        verify_expected_bytes(&bytes, b"\\\x2e_SB_FOO_");
    }

    #[test]
    fn verify_multi_name() {
        let bytes = encode_name(b"FOO.BAR.BAZ.BLAM");
        verify_expected_bytes(&bytes, b"\x2f\x04FOO_BAR_BAZ_BLAM");
    }
}
