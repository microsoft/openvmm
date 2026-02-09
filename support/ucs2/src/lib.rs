// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Defining and implementing from_slice_unchecked.
#![expect(unsafe_code)]

//! Wrappers around possibly misaligned `[u8]` buffers containing UCS-2 LE data.

use std::fmt;
use thiserror::Error;

/// Errors which may occur while parsing UCS-2
#[derive(Debug, Error)]
pub enum Ucs2ParseError {
    /// buffer's length was not a multiple of 2
    #[error("buffer's length was not a multiple of 2")]
    NotMultiple2,
    /// buffer did not contain a null terminator
    #[error("buffer did not contain a null terminator")]
    MissingNullTerm,
}

/// Wrapper around `Vec<u8>` containing a valid null-terminated UCS-2 LE string.
///
/// **This type is not FFI compatible with `*const u16`!**
///
/// Because `Ucs2LeVec` uses a `[u8]` as the backing data type (as opposed to a
/// `[u16]`), the data is **not** guaranteed to be `u16` aligned!
///
/// DEVNOTE: While we want `Ucs2LeSlice` to be backed by a `[u8]`, `Ucs2LeVec`
/// should likely get switched over to a `Vec<u16>`, so we can get proper `u16`
/// alignment. Note that in this case, we could use a bit of (trivially save)
/// `unsafe` code to impl `Deref<Target = Ucs2LeSlice>` by reinterpretting the
/// `Vec<u16>` as a `&[u8]`, so there wouldn't be any major ergonomic hit.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ucs2LeVec(Vec<u8>);

impl Ucs2LeVec {
    /// Validate that the provided `Vec<u8>` is a valid null-terminated UCS-2 LE
    /// string, truncating the slice to the position of the first null u16.
    pub fn from_vec_with_nul(mut buf: Vec<u8>) -> Result<Ucs2LeVec, Ucs2ParseError> {
        let slice = Ucs2LeSlice::from_slice_with_nul(&buf)?;
        // SAFETY: `from_slice_with_nul` performs the truncation on a slice-view
        // of the buf, so using that slice to truncate the buffer is ok.
        buf.truncate(slice.0.len());
        Ok(Ucs2LeVec(buf))
    }

    /// Consume self, returning the underlying raw `Vec<u8>`
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl Default for Ucs2LeVec {
    fn default() -> Ucs2LeVec {
        let s: &Ucs2LeSlice = Default::default();
        s.to_ucs2_le_vec()
    }
}

impl fmt::Debug for Ucs2LeVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.as_ref(), f)
    }
}

impl fmt::Display for Ucs2LeVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.as_ref(), f)
    }
}

impl AsRef<Ucs2LeSlice> for Ucs2LeVec {
    fn as_ref(&self) -> &Ucs2LeSlice {
        // SAFETY: Ucs2LeVec can only contain valid UCS-2 data
        unsafe { Ucs2LeSlice::from_slice_unchecked(&self.0) }
    }
}

impl std::ops::Deref for Ucs2LeVec {
    type Target = Ucs2LeSlice;

    fn deref(&self) -> &Ucs2LeSlice {
        self.as_ref()
    }
}

impl std::borrow::Borrow<Ucs2LeSlice> for Ucs2LeVec {
    fn borrow(&self) -> &Ucs2LeSlice {
        self.as_ref()
    }
}

impl<'a> From<&'a Ucs2LeSlice> for std::borrow::Cow<'a, Ucs2LeSlice> {
    fn from(val: &'a Ucs2LeSlice) -> Self {
        std::borrow::Cow::Borrowed(val)
    }
}

impl From<Ucs2LeVec> for std::borrow::Cow<'_, Ucs2LeSlice> {
    fn from(val: Ucs2LeVec) -> Self {
        std::borrow::Cow::Owned(val)
    }
}

impl<'a> From<&'a str> for Ucs2LeVec {
    fn from(s: &'a str) -> Ucs2LeVec {
        let mut s = s
            .encode_utf16()
            .flat_map(|w| [w as u8, (w >> 8) as u8])
            .collect::<Vec<u8>>();
        s.push(0);
        s.push(0);
        // SAFETY: UTF-8 str has been converted into a valid null-terminated UCS-2 Le string
        Ucs2LeVec(s)
    }
}

impl From<String> for Ucs2LeVec {
    fn from(s: String) -> Ucs2LeVec {
        Ucs2LeVec::from(s.as_str())
    }
}

/// Wrapper around `[u8]` containing a valid null-terminated UCS-2 LE string.
///
/// **This type is not FFI compatible with `*const u16`!**
///
/// Because `Ucs2LeSlice` uses a `[u8]` as the backing data type (as opposed to
/// a `[u16]`), the data is **not** guaranteed to be `u16` aligned!
///
/// # Example
///
/// ```
/// # use ucs2::Ucs2LeSlice;
/// let raw = [b'O', 0, b'K', 0, 0, 0];
/// let s = Ucs2LeSlice::from_slice_with_nul(&raw).unwrap();
/// assert_eq!(s.as_bytes().len(), raw.len());
/// assert_eq!(s.to_string(), "OK");
/// ```
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ucs2LeSlice([u8]);

impl<'a> Default for &'a Ucs2LeSlice {
    fn default() -> &'a Ucs2LeSlice {
        // SAFETY: &[0, 0] is a valid null-terminated UCS-2 LE string.
        unsafe { Ucs2LeSlice::from_slice_unchecked(&[0, 0]) }
    }
}

impl Ucs2LeSlice {
    /// Validate that the provided `&[u8]` is a valid null-terminated UCS-2 LE
    /// string, truncating the slice to the position of the first null u16.
    pub fn from_slice_with_nul(buf: &[u8]) -> Result<&Ucs2LeSlice, Ucs2ParseError> {
        if !buf.len().is_multiple_of(2) {
            return Err(Ucs2ParseError::NotMultiple2);
        }

        // Unlike UTF-8 or UTF-16, UCS-2 doesn't require any complex semantic
        // validation, as all values from 0 to 0xFFFF are valid codepoints.

        let mut buf_as_u16_iter = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().unwrap()));

        match buf_as_u16_iter.position(|c| c == 0) {
            None => Err(Ucs2ParseError::MissingNullTerm),
            // SAFETY: buf has been validated to contain valid data
            Some(idx) => Ok(unsafe { Ucs2LeSlice::from_slice_unchecked(&buf[..(idx + 1) * 2]) }),
        }
    }

    /// Create a `Ucs2LeSlice` from a raw `&[u8]` without performing any
    /// validation.
    ///
    /// # Safety
    ///
    /// Callers must ensure that the buf has a length that is a multiple of 2,
    /// contains valid UCS-2 codepoints, and terminates with a single null u16.
    unsafe fn from_slice_unchecked(buf: &[u8]) -> &Ucs2LeSlice {
        // SAFETY: caller has maintained invariants, and `Ucs2LeSlice` has the
        // same representation as [u8]
        unsafe { std::mem::transmute(buf) }
    }

    /// View the underlying data as raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// View the underlying data as raw bytes, without the trailing null `u16`.
    pub fn as_bytes_without_nul(&self) -> &[u8] {
        self.0.strip_suffix(&[0, 0]).unwrap()
    }

    /// Copies `self` into a new [`Ucs2LeVec`].
    pub fn to_ucs2_le_vec(&self) -> Ucs2LeVec {
        Ucs2LeVec(self.0.to_vec())
    }

    fn to_string_inner(&self) -> String {
        // TODO: this isn't strictly correct, since UCS-2 handles chars in the
        // surragate range (0xD800–0xDFFF) differently from UTF-16.
        //
        // Properly converting UCS-2 to UTF-8/16 is a bit more subtle, and
        // handling this properly will require a PR in its own right.
        String::from_utf16_lossy(
            &self
                .0
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
                .take_while(|b| *b != 0)
                .collect::<Vec<u16>>(),
        )
    }
}

impl ToOwned for Ucs2LeSlice {
    type Owned = Ucs2LeVec;

    fn to_owned(&self) -> Ucs2LeVec {
        self.to_ucs2_le_vec()
    }
}

impl fmt::Debug for Ucs2LeSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_string_inner(), f)
    }
}

impl fmt::Display for Ucs2LeSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_string_inner(), f)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn str_to_utf16_buf(s: &str) -> Vec<u8> {
        // TODO: This _technically_ incorrect, and will result in wonky behavior
        // if the string contains a code point outside of the Basic Multilingual
        // Plane (i.e: 0x0000-0xffff)
        //
        // Wonky != Invalid, since technically, UCS-2 doesn't have any "invalid"
        // values...
        //
        // In any case, this is test code, and we aren't using any funky chars
        // here, so it's not _super_ important.
        //
        // Too bad UEFI doesn't support proper UTF-16... imagine if we could use
        // Emoji as nvram variable names 👀
        s.encode_utf16()
            .flat_map(|b| b.to_le_bytes().into_iter())
            .collect::<Vec<u8>>()
    }

    #[test]
    fn smoke() {
        let s8 = "hello!\0";

        let s16 = str_to_utf16_buf(s8);
        let s16 = Ucs2LeSlice::from_slice_with_nul(&s16).unwrap();
        assert_eq!(s16.as_bytes().len(), s8.len() * 2);
        assert_eq!(
            s16.as_bytes().chunks_exact(2).last(),
            Some([0u8, 0].as_ref())
        )
    }

    #[test]
    fn interior_middle_null() {
        let s8 = "hello!\0extra";
        let s8_expected = "hello!\0";

        let s16 = str_to_utf16_buf(s8);
        let s16_expected = str_to_utf16_buf(s8_expected);

        let s16 = Ucs2LeSlice::from_slice_with_nul(&s16).unwrap();
        let s16_expected = Ucs2LeSlice::from_slice_with_nul(&s16_expected).unwrap();

        assert_eq!(s16, s16_expected)
    }

    #[test]
    fn zero_len() {
        let s8 = "\0";

        let s16 = str_to_utf16_buf(s8);
        let s16 = Ucs2LeSlice::from_slice_with_nul(&s16).unwrap();
        assert_eq!(s16.as_bytes().len(), 2);
        assert_eq!(s16.as_bytes(), [0u8, 0].as_ref())
    }

    #[test]
    fn not_multiple_2() {
        let s8 = "so close!\0";

        let mut s16 = str_to_utf16_buf(s8);
        s16.push(0);

        let res = Ucs2LeSlice::from_slice_with_nul(&s16);
        assert!(matches!(res, Err(Ucs2ParseError::NotMultiple2)))
    }

    #[test]
    fn missing_null_term() {
        let s8 = "so close!";

        let s16 = str_to_utf16_buf(s8);
        let res = Ucs2LeSlice::from_slice_with_nul(&s16);
        assert!(matches!(res, Err(Ucs2ParseError::MissingNullTerm)))
    }
}
