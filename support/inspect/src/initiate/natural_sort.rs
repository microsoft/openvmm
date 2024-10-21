// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::cmp::Ordering;

/// Compares `a` and `b` with a natural ordering, where numbers within the
/// strings compare as numbers instead of strings.
///
/// For example, foo3 < foo100.
///
/// This implementation supports decimal unsigned integers and hexadecimal
/// unsigned integers prefixed by "0x".
///
/// Internal numbers with a alphabetic suffix are treated as strings.
///
/// So 3foo > 100foo. But 3_foo < 100_foo.
///
/// This is done to avoid parsing non-prefixed hexadecimal data as decimal data,
/// which can confuse the ordering of GUIDs and similar data.
pub fn compare(a: impl AsRef<str>, b: impl AsRef<str>) -> Ordering {
    SegmentIter(a.as_ref().as_bytes()).cmp(SegmentIter(b.as_ref().as_bytes()))
}

struct SegmentIter<'a>(&'a [u8]);

#[derive(PartialOrd, Ord, PartialEq, Eq)]
enum Segment<'a> {
    Dec(u64),
    Hex(u64),
    Str(&'a [u8]),
}

impl<'a> Iterator for SegmentIter<'a> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let len = match self.0 {
            [] => return None,
            [b'0', b'x', b, ..] if b.is_ascii_hexdigit() => {
                let (n, len) = parse_prefix(&self.0[2..], 16);
                if let Some(n) = n {
                    self.0 = &self.0[2 + len..];
                    return Some(Segment::Hex(n));
                } else {
                    2 + len
                }
            }
            [b'0'..=b'9', ..] => {
                let (n, len) = parse_prefix(self.0, 16);
                if let Some(n) = n {
                    self.0 = &self.0[len..];
                    return Some(Segment::Dec(n));
                } else {
                    len
                }
            }
            _ => {
                // Skip to the next digit.
                self.0
                    .iter()
                    .position(|b| b.is_ascii_digit())
                    .unwrap_or(self.0.len())
            }
        };

        assert_ne!(len, 0);

        let (seg, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(Segment::Str(seg))
    }
}

fn parse_prefix(v: &[u8], base: u32) -> (Option<u64>, usize) {
    let mut n = 0u64;
    let mut i = 0;
    while let Some(&d) = v.get(i) {
        let d = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' if base == 16 => d - b'a' + 10,
            b'A'..=b'F' if base == 16 => d - b'A' + 10,
            x if x.is_ascii_alphabetic() => return (None, i),
            _ => break,
        };
        if let Some(m) = n.checked_mul(base.into()) {
            n = m + d as u64;
        } else {
            break;
        }
        i += 1;
    }
    (Some(n), i)
}

#[cfg(test)]
mod tests {
    use super::compare;
    use alloc::vec;

    #[test]
    fn test_natural_sort() {
        let mut x = vec![
            "foo", "foo299", "foo3", "bar_0x5", "bar_0x0f", "bar_0xg", "100foo", "100_foo",
            "3_foo", "3foo",
        ];
        x.sort_by(|x, y| compare(x, y));
        assert_eq!(
            x.as_slice(),
            &[
                "3_foo", "100_foo", "100foo", "3foo", "bar_0x5", "bar_0x0f", "bar_0xg", "foo",
                "foo3", "foo299"
            ]
        );
    }
}
