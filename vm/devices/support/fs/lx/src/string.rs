// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Transmuting to implement from_bytes.
#![expect(unsafe_code)]

use std::borrow;
use std::fmt::Write;
use std::fmt::{self};
use std::ops;
use std::str;

/// An owned string that may or may not be valid utf-8.
///
/// This is analogous to `OsString` on Linux, but behaves the same on all platforms.
#[derive(Clone, Hash, PartialEq, Eq, Default)]
pub struct LxString {
    bytes: Vec<u8>,
}

impl LxString {
    /// Creates an empty `LxString`.
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Creates a `LxString` from a byte vector.
    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self { bytes: vec }
    }

    /// Creates a `LxString` with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// Yields the underlying byte vector of this `LxString`.
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }

    /// Converts the `LxString` into a `String` if it contains valid Unicode data.
    pub fn into_string(self) -> Result<String, Self> {
        String::from_utf8(self.bytes).map_err(|e| Self {
            bytes: e.into_bytes(),
        })
    }

    /// Converts to an `LxStr` slice.
    pub fn as_lx_str(&self) -> &LxStr {
        self
    }

    /// Returns the capacity of the `LxString`.
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Clears the contents of the `LxString`.
    pub fn clear(&mut self) {
        self.bytes.clear()
    }

    /// Extends the string with the given `AsRef<LxStr>` slice.
    pub fn push(&mut self, s: impl AsRef<LxStr>) {
        self.bytes.extend_from_slice(&s.as_ref().bytes)
    }

    /// Reserves capacity for at least `additional` more capacity to be inserted into this string.
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    /// Reserves the minimum capacity for exactly additional more capacity to be inserted into this string. Does nothing if
    /// the capacity is already sufficient.
    pub fn reserve_exact(&mut self, additional: usize) {
        self.bytes.reserve_exact(additional);
    }

    /// Shrinks the capacity of this string to match its length.
    pub fn shrink_to_fit(&mut self) {
        self.bytes.shrink_to_fit()
    }
}

impl ops::Deref for LxString {
    type Target = LxStr;

    fn deref(&self) -> &Self::Target {
        LxStr::from_bytes(&self.bytes)
    }
}

impl borrow::Borrow<LxStr> for LxString {
    fn borrow(&self) -> &LxStr {
        self
    }
}

impl<T> From<&T> for LxString
where
    T: ?Sized + AsRef<LxStr>,
{
    fn from(s: &T) -> Self {
        s.as_ref().to_lx_string()
    }
}

impl From<String> for LxString {
    fn from(s: String) -> Self {
        LxString::from_vec(s.into())
    }
}

impl PartialEq<LxString> for &str {
    fn eq(&self, other: &LxString) -> bool {
        **self == **other
    }
}

impl PartialEq<LxString> for str {
    fn eq(&self, other: &LxString) -> bool {
        &**other == self
    }
}

impl PartialEq<str> for LxString {
    fn eq(&self, other: &str) -> bool {
        &**self == other
    }
}

impl PartialEq<&str> for LxString {
    fn eq(&self, other: &&str) -> bool {
        **self == **other
    }
}

impl fmt::Debug for LxString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl FromIterator<char> for LxString {
    fn from_iter<T: IntoIterator<Item = char>>(iter: T) -> Self {
        String::from_iter(iter).into()
    }
}

/// A borrowed reference to a string that may or may not be valid utf-8.
///
/// This is analogous to `OsStr` on Linux, but behaves the same on all platforms.
#[derive(PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct LxStr {
    bytes: [u8],
}

impl LxStr {
    /// Coerces into an `LxStr` slice.
    pub fn new<T: AsRef<LxStr> + ?Sized>(s: &T) -> &LxStr {
        s.as_ref()
    }

    /// Creates an `LxStr` from a byte slice.
    pub fn from_bytes(slice: &[u8]) -> &LxStr {
        // SAFETY: &LxStr has the same repr as &[u8], and doesn't add any
        // additional invariants
        unsafe { std::mem::transmute(slice) }
    }

    /// Gets the underlying byte view of the `LxStr` slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Copies the slice into an owned `LxString`.
    pub fn to_lx_string(&self) -> LxString {
        LxString::from_vec(self.bytes.into())
    }

    /// Returns the length of this `LxStr`.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Checks whether the `LxStr` is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Yields a `&str` slice if the `LxStr` is valid Unicode.
    pub fn to_str(&self) -> Option<&str> {
        str::from_utf8(&self.bytes).ok()
    }

    /// Convert an `LxStr` to a `Cow<str>`.
    ///
    /// Any non-Unicode sequences are replaced with `U+FFFD REPLACEMENT CHARACTER`.
    pub fn to_string_lossy(&self) -> borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

impl ToOwned for LxStr {
    type Owned = LxString;

    fn to_owned(&self) -> Self::Owned {
        self.to_lx_string()
    }
}

impl AsRef<LxStr> for LxStr {
    fn as_ref(&self) -> &LxStr {
        self
    }
}

impl AsRef<LxStr> for LxString {
    fn as_ref(&self) -> &LxStr {
        self
    }
}

impl AsRef<LxStr> for str {
    fn as_ref(&self) -> &LxStr {
        LxStr::from_bytes(self.as_bytes())
    }
}

impl AsRef<LxStr> for String {
    fn as_ref(&self) -> &LxStr {
        (**self).as_ref()
    }
}

impl PartialEq<LxStr> for str {
    fn eq(&self, other: &LxStr) -> bool {
        LxStr::new(self).eq(other)
    }
}

impl PartialEq<str> for LxStr {
    fn eq(&self, other: &str) -> bool {
        self.eq(LxStr::new(other))
    }
}

impl fmt::Debug for LxStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // This isn't quite identical to how Debug works on OsStr, but that requires the use of
        // Utf8Lossy which is not stable.
        let value = self.to_string_lossy();
        f.write_str("\"")?;
        for c in value.chars().flat_map(|c| c.escape_debug()) {
            f.write_char(c)?
        }

        f.write_str("\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_string_new() {
        let s = LxString::new();
        assert_eq!(s.capacity(), 0);
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s, "");
        assert_ne!(s, "something");
    }

    #[test]
    fn lx_string_capacity() {
        let s = LxString::with_capacity(100);
        assert_eq!(s.capacity(), 100);
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s, "");
        assert_ne!(s, "something");
    }

    #[test]
    fn lx_string_from() {
        let s = LxString::from("foo");
        assert_eq!(s.capacity(), 3);
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s, "foo");
        assert_ne!(s, "something");
        assert_ne!(s, "");

        let s = LxString::from(String::from("foo"));
        assert_eq!(s.capacity(), 3);
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s, "foo");
        assert_ne!(s, "something");
        assert_ne!(s, "");
    }

    #[test]
    fn lx_string_from_vec() {
        let s = LxString::from_vec(b"foo".to_vec());
        assert_eq!(s.capacity(), 3);
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s, "foo");
        assert_ne!(s, "something");
        assert_ne!(s, "");
        let vec = s.into_vec();
        assert_eq!(vec, b"foo");
    }

    #[test]
    fn lx_string_into_string() {
        let s = LxString::from("foo");
        let s = s.into_string().unwrap();
        assert_eq!(s, "foo");

        let s = LxString::from_vec(vec![b'a', 0xfe, 0xfe]);
        let e = s.into_string().unwrap_err();
        assert_eq!(e, LxString::from_vec(vec![b'a', 0xfe, 0xfe]));
    }

    #[test]
    fn lx_string_debug() {
        let s = LxString::from("foo\\bar");
        let debug = format!("{:?}", s);
        assert_eq!(debug, r#""foo\\bar""#)
    }

    #[test]
    fn lx_str_new() {
        let s = LxStr::new("");
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s, "");
        assert_ne!(s, "something");

        let s = LxStr::new("foo");
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s, "foo");
        assert_eq!(s, LxStr::new("foo"));
        assert_ne!(s, "something");
        assert_ne!(s, "");
    }

    #[test]
    fn lx_str_from_bytes() {
        let s = LxStr::from_bytes(b"foo");
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s, "foo");
        assert_ne!(s, "something");
        assert_ne!(s, "");
    }

    #[test]
    fn lx_str_from_lx_string() {
        let s = LxString::from("foo");
        let s = s.as_lx_str();
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s, "foo");
        assert_ne!(s, "something");
        assert_ne!(s, "");
    }

    #[test]
    fn lx_str_to_str() {
        let s = LxStr::new("foo");
        let s = s.to_str().unwrap();
        assert_eq!(s, "foo");

        let s = LxStr::from_bytes(&[b'a', 0xfe, 0xfe]);
        assert!(s.to_str().is_none());
    }

    #[test]
    fn lx_str_to_string_lossy() {
        let s = LxStr::new("foo");
        let s = s.to_string_lossy();
        assert!(matches!(s, borrow::Cow::Borrowed(_)));
        assert_eq!(s, "foo");
        let s = LxStr::from_bytes(&[b'a', 0xfe, 0xfe, b'b']);

        let s = s.to_string_lossy();
        assert!(matches!(s, borrow::Cow::Owned(_)));
        assert_eq!(s, "a\u{fffd}\u{fffd}b");
    }

    #[test]
    fn lx_str_debug() {
        let s = LxStr::new("foo\\bar");
        let debug = format!("{:?}", s);
        assert_eq!(debug, r#""foo\\bar""#)
    }
}
