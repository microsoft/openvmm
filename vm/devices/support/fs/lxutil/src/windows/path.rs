// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::borrow::Cow;
use std::path::Path;
use std::str;

// The utf-8 sequence for code point 0xf000
const ESCAPE_CHAR_BASE: [u8; 3] = [0xef, 0x80, 0x80];

const PATH_ESCAPE_MIN: u16 = 0xf000;
const PATH_ESCAPE_MAX: u16 = 0xf0ff;

// Convert a Unix-style path to a Windows one.
pub fn path_from_lx(path: &[u8]) -> lx::Result<Cow<'_, Path>> {
    let (escaped_len, has_sep) = escape_path_len(path);

    // If nothing needs to be escaped, return the original path.
    if escaped_len == path.len() && !has_sep {
        return Ok(Cow::Borrowed(
            str::from_utf8(path)
                .map_err(|_| lx::Error::EINVAL)?
                .as_ref(),
        ));
    }

    // Create the escaped path.
    let mut escaped_path = Vec::with_capacity(escaped_len);
    for &c in path {
        if c == b'/' {
            // Flip separators.
            escaped_path.push(b'\\');
        } else if char_needs_escape(c) {
            // Construct the escaped utf-8 character.
            escaped_path.push(ESCAPE_CHAR_BASE[0]);
            escaped_path.push(ESCAPE_CHAR_BASE[1] | (c >> 6));
            escaped_path.push(ESCAPE_CHAR_BASE[2] | (c & 0x3f));
        } else {
            escaped_path.push(c);
        }
    }

    Ok(Cow::Owned(
        String::from_utf8(escaped_path)
            .map_err(|_| lx::Error::EINVAL)?
            .into(),
    ))
}

// Determine the buffer size needed for an escaped path.
// Also checks if the path has separators.
fn escape_path_len(path: &[u8]) -> (usize, bool) {
    let mut has_sep = false;
    let len = path
        .iter()
        .map(|&c| {
            if char_needs_escape(c) {
                ESCAPE_CHAR_BASE.len()
            } else {
                if c == b'/' {
                    has_sep = true;
                }

                1
            }
        })
        .sum();

    (len, has_sep)
}

// Check if a character needs to be escaped.
// Since all characters that need escaped are single-byte in utf-8, there's no need to worry about
// the encoding here.
fn char_needs_escape(c: u8) -> bool {
    let index = c as usize;
    index < NTFS_LEGAL_ANSI_CHARACTERS.len() && !NTFS_LEGAL_ANSI_CHARACTERS[index]
}

// Check if a character needs to be unescaped.
fn char_needs_unescape(c: u16) -> bool {
    // Not all characters within the range correspond to escaped characters,
    // so  make sure that the original character needed to be escaped.
    (PATH_ESCAPE_MIN..=PATH_ESCAPE_MAX).contains(&c)
        && char_needs_escape((c - PATH_ESCAPE_MIN) as u8)
}

// Unescape a path.
pub fn unescape_path(path: &[u16]) -> lx::Result<lx::LxString> {
    char::decode_utf16(path.iter().map(|c| {
        if char_needs_unescape(*c) {
            *c - PATH_ESCAPE_MIN
        } else if (*c) == '\\' as u16 {
            '/' as u16
        } else {
            *c
        }
    }))
    .map(|c| c.map_err(|_| lx::Error::EIO))
    .collect()
}

// Unescape a path in place.
pub fn unescape_path_in_place(path: &mut [u16]) {
    for c in path {
        if char_needs_unescape(*c) {
            *c -= PATH_ESCAPE_MIN;
        } else if (*c) == '\\' as u16 {
            *c = '/' as u16;
        }
    }
}

// List indicating which characters are legal in NTFS. This was adapted from
// FsRtl, with two modifications from the original:
// 1. Slashes are allowed (because escaping is done on full Linux paths).
// 2. Colons are disallowed (because they indicate alternate data streams).
const NTFS_LEGAL_ANSI_CHARACTERS: [bool; 128] = [
    false, // 0x00 ^@
    false, // 0x01 ^A
    false, // 0x02 ^B
    false, // 0x03 ^C
    false, // 0x04 ^D
    false, // 0x05 ^E
    false, // 0x06 ^F
    false, // 0x07 ^G
    false, // 0x08 ^H
    false, // 0x09 ^I
    false, // 0x0A ^J
    false, // 0x0B ^K
    false, // 0x0C ^L
    false, // 0x0D ^M
    false, // 0x0E ^N
    false, // 0x0F ^O
    false, // 0x10 ^P
    false, // 0x11 ^Q
    false, // 0x12 ^R
    false, // 0x13 ^S
    false, // 0x14 ^T
    false, // 0x15 ^U
    false, // 0x16 ^V
    false, // 0x17 ^W
    false, // 0x18 ^X
    false, // 0x19 ^Y
    false, // 0x1A ^Z
    false, // 0x1B ESC
    false, // 0x1C FS
    false, // 0x1D GS
    false, // 0x1E RS
    false, // 0x1F US
    true,  // 0x20 space
    true,  // 0x21 !
    false, // 0x22 "
    true,  // 0x23 #
    true,  // 0x24 $
    true,  // 0x25 %
    true,  // 0x26 &
    true,  // 0x27 '
    true,  // 0x28 (
    true,  // 0x29 )
    false, // 0x2A *
    true,  // 0x2B +
    true,  // 0x2C,
    true,  // 0x2D -
    true,  // 0x2E .
    true,  // 0x2F /   *** Normally "false"
    true,  // 0x30 0
    true,  // 0x31 1
    true,  // 0x32 2
    true,  // 0x33 3
    true,  // 0x34 4
    true,  // 0x35 5
    true,  // 0x36 6
    true,  // 0x37 7
    true,  // 0x38 8
    true,  // 0x39 9
    false, // 0x3A :   *** Normally "true"
    true,  // 0x3B ;
    false, // 0x3C <
    true,  // 0x3D =
    false, // 0x3E >
    false, // 0x3F ?
    true,  // 0x40 @
    true,  // 0x41 A
    true,  // 0x42 B
    true,  // 0x43 C
    true,  // 0x44 D
    true,  // 0x45 E
    true,  // 0x46 F
    true,  // 0x47 G
    true,  // 0x48 H
    true,  // 0x49 I
    true,  // 0x4A J
    true,  // 0x4B K
    true,  // 0x4C L
    true,  // 0x4D M
    true,  // 0x4E N
    true,  // 0x4F O
    true,  // 0x50 P
    true,  // 0x51 Q
    true,  // 0x52 R
    true,  // 0x53 S
    true,  // 0x54 T
    true,  // 0x55 U
    true,  // 0x56 V
    true,  // 0x57 W
    true,  // 0x58 X
    true,  // 0x59 Y
    true,  // 0x5A Z
    true,  // 0x5B [
    false, // 0x5C backslash
    true,  // 0x5D ]
    true,  // 0x5E ^
    true,  // 0x5F _
    true,  // 0x60 `
    true,  // 0x61 a
    true,  // 0x62 b
    true,  // 0x63 c
    true,  // 0x64 d
    true,  // 0x65 e
    true,  // 0x66 f
    true,  // 0x67 g
    true,  // 0x68 h
    true,  // 0x69 i
    true,  // 0x6A j
    true,  // 0x6B k
    true,  // 0x6C l
    true,  // 0x6D m
    true,  // 0x6E n
    true,  // 0x6F o
    true,  // 0x70 p
    true,  // 0x71 q
    true,  // 0x72 r
    true,  // 0x73 s
    true,  // 0x74 t
    true,  // 0x75 u
    true,  // 0x76 v
    true,  // 0x77 w
    true,  // 0x78 x
    true,  // 0x79 y
    true,  // 0x7A z
    true,  // 0x7B {
    false, // 0x7C |
    true,  // 0x7D }
    true,  // 0x7E ~
    true,  // 0x7F 
];

#[cfg(test)]
mod tests {
    use super::*;
    use pal::windows::UnicodeString;

    #[test]
    fn unescape() {
        let mut path1: UnicodeString = "test".try_into().unwrap();
        let mut path2: UnicodeString = "foo\u{f03a}bar".try_into().unwrap();
        let mut path3: UnicodeString = "foo\\bar".try_into().unwrap();

        let new_path1 = unescape_path(path1.as_mut_slice()).unwrap();
        let new_path2 = unescape_path(path2.as_mut_slice()).unwrap();
        let new_path3 = unescape_path(path3.as_mut_slice()).unwrap();
        assert_eq!(new_path1, "test");
        assert_eq!(new_path2, "foo:bar");
        assert_eq!(new_path3, "foo/bar");
    }
}
