// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for Windows operations, used by multiple algorithms.

#![cfg(all(native, windows))]

use std::ffi::c_void;
use std::ptr::NonNull;
use windows::Win32::Foundation::HLOCAL;
use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::CERT_PUBLIC_KEY_INFO;
use windows::Win32::Security::Cryptography::CRYPT_ALGORITHM_IDENTIFIER;
use windows::Win32::Security::Cryptography::CRYPT_BIT_BLOB;
use windows::Win32::Security::Cryptography::CRYPT_DECODE_ALLOC_FLAG;
use windows::Win32::Security::Cryptography::CRYPT_DECODE_NO_SIGNATURE_BYTE_REVERSAL_FLAG;
use windows::Win32::Security::Cryptography::CRYPT_ENCODE_OBJECT_FLAGS;
use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows::Win32::Security::Cryptography::X509_ASN_ENCODING;
use windows::Win32::Security::Cryptography::X509_CERT;
use windows::core::PCSTR;
use windows::core::PSTR;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

/// Owns the buffers referenced by a native SubjectPublicKeyInfo.
pub(crate) struct PublicKeyInfo {
    algorithm: PCSTR,
    parameters: Vec<u8>,
    public_key: Vec<u8>,
}

impl PublicKeyInfo {
    pub(crate) fn new(algorithm: PCSTR, parameters: Vec<u8>, public_key: Vec<u8>) -> Self {
        Self {
            algorithm,
            parameters,
            public_key,
        }
    }

    /// The returned pointers remain valid while `self` is alive.
    pub(crate) fn as_info(&self) -> CERT_PUBLIC_KEY_INFO {
        CERT_PUBLIC_KEY_INFO {
            Algorithm: CRYPT_ALGORITHM_IDENTIFIER {
                pszObjId: PSTR(self.algorithm.0.cast_mut()),
                Parameters: CRYPT_INTEGER_BLOB {
                    cbData: self.parameters.len() as u32,
                    pbData: self.parameters.as_ptr().cast_mut(),
                },
            },
            PublicKey: CRYPT_BIT_BLOB {
                cbData: self.public_key.len() as u32,
                pbData: self.public_key.as_ptr().cast_mut(),
                cUnusedBits: 0,
            },
        }
    }
}

/// Owns a CryptoAPI-decoded structure and the allocation its pointers reference.
pub(crate) struct Decoded<T> {
    ptr: NonNull<c_void>,
    len: u32,
    pub(crate) value: T,
    _phantom: std::marker::PhantomData<*const T>,
}

impl<T> Drop for Decoded<T> {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated by Crypt32 with LocalAlloc semantics.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.ptr.as_ptr()))) };
    }
}

impl<T> Decoded<T> {
    /// Borrow the decoded value while its backing allocation is alive.
    pub(crate) fn get(&self) -> &T {
        &self.value
    }

    /// Returns the allocation as a byte slice.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: ptr is non-null and points to `len` bytes owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().cast::<u8>(), self.len as usize) }
    }
}

/// Decode a CryptoAPI-defined object, retaining its allocated backing buffer.
pub(crate) fn decode_object<T: Copy>(
    struct_type: PCSTR,
    encoded: &[u8],
) -> Result<Decoded<T>, crate::BackendError> {
    let _: u32 = encoded
        .len()
        .try_into()
        .map_err(|_| crate::BackendError::Invalid("ASN.1 object encoding is too large"))?;
    let mut raw: *mut c_void = std::ptr::null_mut();
    let mut size: u32 = 0;
    // SAFETY: With CRYPT_DECODE_ALLOC_FLAG the API allocates the output
    // buffer and writes its pointer into `raw`.
    unsafe {
        windows::Win32::Security::Cryptography::CryptDecodeObjectEx(
            X509_ASN_ENCODING,
            struct_type,
            encoded,
            CRYPT_DECODE_ALLOC_FLAG
                | if struct_type == X509_CERT {
                    // Preserve the DER signature byte order expected by BCrypt.
                    CRYPT_DECODE_NO_SIGNATURE_BYTE_REVERSAL_FLAG
                } else {
                    0
                },
            None,
            Some(std::ptr::from_mut(&mut raw).cast::<c_void>()),
            &mut size,
        )
    }
    .map_err(|e| crate::BackendError::Windows(e, "decoding an ASN.1 object"))?;
    let ptr = NonNull::new(raw).ok_or(crate::BackendError::Invalid(
        "CryptDecodeObjectEx succeeded but returned no buffer",
    ))?;
    if (size as usize) < size_of::<T>() {
        // SAFETY: ptr was allocated by CryptoAPI via CRYPT_DECODE_ALLOC_FLAG.
        let _ = unsafe { LocalFree(Some(HLOCAL(ptr.as_ptr()))) };
        return Err(crate::BackendError::Invalid(
            "CryptDecodeObjectEx returned a buffer smaller than the decoded structure",
        ));
    }
    // SAFETY: CryptoAPI wrote a T-sized header; copying it leaves any
    // referenced data in the allocation retained by Decoded.
    let value = unsafe { std::ptr::read_unaligned(ptr.as_ptr().cast::<T>()) };
    Ok(Decoded {
        ptr,
        len: size,
        value,
        _phantom: std::marker::PhantomData,
    })
}

pub(crate) fn encode_object<T: ?Sized>(
    struct_type: PCSTR,
    value: &T,
) -> Result<Vec<u8>, windows_result::Error> {
    let mut size: u32 = 0;
    let value = std::ptr::from_ref(value).cast();
    let flags = CRYPT_ENCODE_OBJECT_FLAGS(if struct_type == X509_CERT {
        windows::Win32::Security::Cryptography::CRYPT_ENCODE_NO_SIGNATURE_BYTE_REVERSAL_FLAG
    } else {
        0
    });
    // SAFETY: first call queries size.
    unsafe {
        windows::Win32::Security::Cryptography::CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            struct_type,
            value,
            flags,
            None,
            None,
            &mut size,
        )
    }?;
    let mut out = vec![0u8; size as usize];
    // SAFETY: out sized per query.
    unsafe {
        windows::Win32::Security::Cryptography::CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            struct_type,
            value,
            flags,
            None,
            Some(out.as_mut_ptr().cast::<c_void>()),
            &mut size,
        )
    }?;
    out.truncate(size as usize);
    Ok(out)
}

pub struct AlgHandle(pub BCRYPT_ALG_HANDLE);
// SAFETY: the handle can be sent across threads.
unsafe impl Send for AlgHandle {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for AlgHandle {}

impl Drop for AlgHandle {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased
        let _ = unsafe {
            windows::Win32::Security::Cryptography::BCryptCloseAlgorithmProvider(self.0, 0)
        };
    }
}

pub struct KeyHandle(pub BCRYPT_KEY_HANDLE);
// SAFETY: the handle can be sent across threads.
unsafe impl Send for KeyHandle {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for KeyHandle {}

impl Drop for KeyHandle {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased
        let _ = unsafe { windows::Win32::Security::Cryptography::BCryptDestroyKey(self.0) };
    }
}

// TODO: Consider making KeyBlob generic over the key size once zerocopy has better
// const generic support.
#[repr(C)]
#[derive(IntoBytes, Immutable)]
pub struct KeyBlob32 {
    header_magic: u32,
    header_version: u32,
    key_len: u32,
    key: [u8; 32],
}

impl KeyBlob32 {
    pub fn new(key: &[u8; 32]) -> KeyBlob32 {
        KeyBlob32 {
            header_magic: windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB_MAGIC,
            header_version: windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB_VERSION1,
            key_len: 32,
            key: *key,
        }
    }
}
