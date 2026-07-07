// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 SignedData parsing using the Windows CryptoAPI (`crypt32.dll`).

use super::*;
use std::ffi::c_void;
use std::ptr::NonNull;
use windows::Win32::Security::Cryptography::CMSG_CERT_COUNT_PARAM;
use windows::Win32::Security::Cryptography::CMSG_CERT_PARAM;
use windows::Win32::Security::Cryptography::CMSG_SIGNED;
use windows::Win32::Security::Cryptography::CMSG_SIGNER_COUNT_PARAM;
use windows::Win32::Security::Cryptography::CMSG_SIGNER_INFO;
use windows::Win32::Security::Cryptography::CMSG_SIGNER_INFO_PARAM;
use windows::Win32::Security::Cryptography::CMSG_TYPE_PARAM;
use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows::Win32::Security::Cryptography::PKCS_7_ASN_ENCODING;
use windows::Win32::Security::Cryptography::X509_ASN_ENCODING;

fn err(e: windows_result::Error, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError(e, op))
}

fn last_err(op: &'static str) -> Pkcs7Error {
    err(windows_result::Error::from_thread(), op)
}

fn bogus_err(op: &'static str) -> Pkcs7Error {
    err(
        windows_result::Error::from_hresult(windows::core::HRESULT(-1)),
        op,
    )
}

/// RAII wrapper around `HCRYPTMSG`.
struct Msg(NonNull<c_void>);
// SAFETY: handle can be sent across threads.
unsafe impl Send for Msg {}
// SAFETY: handle is read-only after the final CryptMsgUpdate.
unsafe impl Sync for Msg {}

impl Drop for Msg {
    fn drop(&mut self) {
        // SAFETY: handle is valid; CryptMsgClose tolerates a single close.
        let _ =
            unsafe { windows::Win32::Security::Cryptography::CryptMsgClose(Some(self.0.as_ptr())) };
    }
}

pub struct Pkcs7SignedDataInner {
    msg: Msg,
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        let encoding = PKCS_7_ASN_ENCODING.0 | X509_ASN_ENCODING.0;

        // SAFETY: passing no recipient info / stream info; standard usage
        // documented for CryptMsgOpenToDecode.
        let h = unsafe {
            windows::Win32::Security::Cryptography::CryptMsgOpenToDecode(
                encoding, 0, 0, None, None, None,
            )
        };
        let h = NonNull::new(h).ok_or_else(|| last_err("CryptMsgOpenToDecode"))?;
        let msg = Msg(h);

        // SAFETY: `data` is a valid byte slice; msg is a fresh decode handle.
        unsafe {
            windows::Win32::Security::Cryptography::CryptMsgUpdate(h.as_ptr(), Some(data), true)
        }
        .map_err(|e| err(e, "CryptMsgUpdate"))?;

        // Confirm the message is SignedData (type 2)
        let mtype: u32 = get_param_value::<u32>(&msg, CMSG_TYPE_PARAM, 0)?;
        if mtype != CMSG_SIGNED.0 {
            return Err(bogus_err("PKCS#7 message is not SignedData"));
        }

        Ok(Self { msg })
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        use windows::Win32::Security::Cryptography::CMSG_ENCODED_MESSAGE;
        get_param_bytes(&self.msg, CMSG_ENCODED_MESSAGE, 0)
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn sign(
        cert: &X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
        use windows::Win32::Security::Cryptography::BCRYPT_RSAFULLPRIVATE_BLOB;
        use windows::Win32::Security::Cryptography::CERT_KEY_CONTEXT;
        use windows::Win32::Security::Cryptography::CERT_KEY_CONTEXT_0;
        use windows::Win32::Security::Cryptography::CERT_KEY_CONTEXT_PROP_ID;
        use windows::Win32::Security::Cryptography::CERT_NCRYPT_KEY_SPEC;
        use windows::Win32::Security::Cryptography::CRYPT_ALGORITHM_IDENTIFIER;
        use windows::Win32::Security::Cryptography::CRYPT_SIGN_MESSAGE_PARA;
        use windows::Win32::Security::Cryptography::CertCreateCertificateContext;
        use windows::Win32::Security::Cryptography::CertSetCertificateContextProperty;
        use windows::Win32::Security::Cryptography::CryptSignMessage;
        use windows::Win32::Security::Cryptography::MS_KEY_STORAGE_PROVIDER;
        use windows::Win32::Security::Cryptography::NCRYPT_FLAGS;
        use windows::Win32::Security::Cryptography::NCRYPT_KEY_HANDLE;
        use windows::Win32::Security::Cryptography::NCRYPT_PROV_HANDLE;
        use windows::Win32::Security::Cryptography::NCryptImportKey;
        use windows::Win32::Security::Cryptography::NCryptOpenStorageProvider;
        use windows::Win32::Security::Cryptography::szOID_NIST_sha256;
        use windows::core::PSTR;

        fn rsa_err(e: windows_result::Error, op: &'static str) -> crate::rsa::RsaError {
            crate::rsa::RsaError(crate::BackendError(e, op))
        }

        // Hand the BCrypt private key to NCrypt as an ephemeral (no-name,
        // not persisted) Software KSP key so CryptSignMessage can drive it.
        let key_blob = crate::rsa::win::export_key(&key_pair.0.0, BCRYPT_RSAFULLPRIVATE_BLOB)?;

        let mut prov_raw = NCRYPT_PROV_HANDLE::default();
        // SAFETY: standard NCrypt provider open; MS_KEY_STORAGE_PROVIDER is
        // a static, NUL-terminated wide string.
        unsafe { NCryptOpenStorageProvider(&mut prov_raw, MS_KEY_STORAGE_PROVIDER, 0) }
            .map_err(|e| rsa_err(e, "NCryptOpenStorageProvider"))?;
        let prov = NCryptProv(prov_raw);

        let mut nkey_raw = NCRYPT_KEY_HANDLE::default();
        // SAFETY: prov is valid; key_blob holds a BCRYPT_RSAFULLPRIVATE_BLOB
        // freshly produced by BCryptExportKey, which NCrypt accepts as an
        // import blob type. No key name is supplied, so the key is
        // ephemeral.
        unsafe {
            NCryptImportKey(
                prov.0,
                None,
                BCRYPT_RSAFULLPRIVATE_BLOB,
                None,
                &mut nkey_raw,
                &key_blob,
                NCRYPT_FLAGS(0),
            )
        }
        .map_err(|e| rsa_err(e, "NCryptImportKey"))?;
        let nkey = NCryptKey(nkey_raw);

        // Build a fresh CERT_CONTEXT from the DER so we can attach the
        // ephemeral key without mutating the caller's certificate.
        let cert_der = cert.to_der().map_err(|e| crate::rsa::RsaError(e.0))?;
        // SAFETY: cert_der is a valid DER X.509 byte slice.
        let ctx_ptr = unsafe { CertCreateCertificateContext(X509_ASN_ENCODING, &cert_der) };
        let ctx_ptr = NonNull::new(ctx_ptr).ok_or_else(|| {
            rsa_err(
                windows_result::Error::from_thread(),
                "CertCreateCertificateContext",
            )
        })?;
        let signing_ctx = OwnedCertContext(ctx_ptr);

        let key_ctx = CERT_KEY_CONTEXT {
            cbSize: size_of::<CERT_KEY_CONTEXT>() as u32,
            Anonymous: CERT_KEY_CONTEXT_0 { hNCryptKey: nkey.0 },
            dwKeySpec: CERT_NCRYPT_KEY_SPEC.0,
        };
        // SAFETY: signing_ctx owns a valid CERT_CONTEXT; key_ctx is sized
        // to match CERT_KEY_CONTEXT.
        unsafe {
            CertSetCertificateContextProperty(
                signing_ctx.0.as_ptr(),
                CERT_KEY_CONTEXT_PROP_ID,
                0,
                Some(std::ptr::from_ref(&key_ctx).cast::<c_void>()),
            )
        }
        .map_err(|e| rsa_err(e, "CertSetCertificateContextProperty"))?;

        let hash_alg = CRYPT_ALGORITHM_IDENTIFIER {
            pszObjId: PSTR(szOID_NIST_sha256.0.cast_mut()),
            Parameters: CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            },
        };
        // CryptSignMessage omits the signing cert from the message by
        // default; embed it explicitly so signer_cert_sig() can find it.
        let mut msg_cert: *mut windows::Win32::Security::Cryptography::CERT_CONTEXT =
            signing_ctx.0.as_ptr();
        let params = CRYPT_SIGN_MESSAGE_PARA {
            cbSize: size_of::<CRYPT_SIGN_MESSAGE_PARA>() as u32,
            dwMsgEncodingType: PKCS_7_ASN_ENCODING.0 | X509_ASN_ENCODING.0,
            pSigningCert: signing_ctx.0.as_ptr(),
            HashAlgorithm: hash_alg,
            cMsgCert: 1,
            rgpMsgCert: &mut msg_cert,
            ..Default::default()
        };

        let data_ptr: *const u8 = data.as_ptr();
        let data_len: u32 = data.len().try_into().expect("signed content fits in u32");

        let mut needed: u32 = 0;
        // SAFETY: size query; output buffer pointer is None.
        unsafe {
            CryptSignMessage(
                &params,
                true,
                1,
                Some(&data_ptr),
                &data_len,
                None,
                &mut needed,
            )
        }
        .map_err(|e| rsa_err(e, "CryptSignMessage (size query)"))?;

        let mut out = vec![0u8; needed as usize];
        // SAFETY: out is sized per the previous query; data buffer pointer
        // is non-null and describes data_len bytes.
        unsafe {
            CryptSignMessage(
                &params,
                true,
                1,
                Some(&data_ptr),
                &data_len,
                Some(out.as_mut_ptr()),
                &mut needed,
            )
        }
        .map_err(|e| rsa_err(e, "CryptSignMessage"))?;
        out.truncate(needed as usize);

        Self::from_der(&out).map_err(|e| crate::rsa::RsaError(e.0))
    }

    pub fn embedded_certificates(&self) -> Result<Vec<X509Certificate>, Pkcs7Error> {
        let count: u32 = get_param_value::<u32>(&self.msg, CMSG_CERT_COUNT_PARAM, 0)?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let der = get_param_bytes(&self.msg, CMSG_CERT_PARAM, i)?;
            out.push(X509Certificate::from_der(&der).map_err(|e| Pkcs7Error(e.0))?);
        }
        Ok(out)
    }

    pub fn signer_cert_sig(&self) -> Result<(X509Certificate, Vec<u8>), Pkcs7Error> {
        // Require exactly one signer.
        let signer_count: u32 = get_param_value::<u32>(&self.msg, CMSG_SIGNER_COUNT_PARAM, 0)?;
        if signer_count != 1 {
            return Err(bogus_err(
                "expected exactly one signer in PKCS#7 SignedData",
            ));
        }

        // Pull the signer info as raw bytes and reinterpret as
        // CMSG_SIGNER_INFO.
        let signer_buf = get_param_bytes(&self.msg, CMSG_SIGNER_INFO_PARAM, 0)?;
        if signer_buf.len() < size_of::<CMSG_SIGNER_INFO>() {
            return Err(bogus_err("CMSG_SIGNER_INFO buffer smaller than struct"));
        }
        // SAFETY: CryptMsgGetParam(CMSG_SIGNER_INFO_PARAM) populates a
        // CMSG_SIGNER_INFO at the start of the buffer. Read unaligned to
        // sidestep the Vec alignment.
        let si =
            unsafe { std::ptr::read_unaligned(signer_buf.as_ptr().cast::<CMSG_SIGNER_INFO>()) };

        // CMSG_SIGNER_INFO only represents IssuerAndSerialNumber signer
        // identifiers (PKCS#7 v1). CMS SubjectKeyIdentifier signers leave
        // both blobs empty here; reject them explicitly rather than
        // silently matching an empty issuer.
        if si.Issuer.cbData == 0 || si.SerialNumber.cbData == 0 {
            return Err(bogus_err(
                "signer identifier is not IssuerAndSerialNumber (SubjectKeyIdentifier not supported)",
            ));
        }
        let want_issuer =
            blob_to_vec(&si.Issuer).ok_or_else(|| bogus_err("malformed signer issuer blob"))?;
        let want_serial = blob_to_vec(&si.SerialNumber)
            .ok_or_else(|| bogus_err("malformed signer serial blob"))?;
        let signature = blob_to_vec(&si.EncryptedHash)
            .ok_or_else(|| bogus_err("malformed signer signature blob"))?;

        let count: u32 = get_param_value::<u32>(&self.msg, CMSG_CERT_COUNT_PARAM, 0)?;
        for i in 0..count {
            let der = get_param_bytes(&self.msg, CMSG_CERT_PARAM, i)?;
            let cert = X509Certificate::from_der(&der).map_err(|e| Pkcs7Error(e.0))?;
            let (issuer, serial) =
                extract_issuer_and_serial(&cert.0).map_err(|e| Pkcs7Error(e.0))?;
            if issuer == want_issuer && serial == want_serial {
                return Ok((cert, signature));
            }
        }
        Err(bogus_err("no embedded certificate matches the signer"))
    }
}

fn blob_to_vec(b: &CRYPT_INTEGER_BLOB) -> Option<Vec<u8>> {
    if b.cbData == 0 {
        return Some(Vec::new());
    }
    if b.pbData.is_null() {
        return None;
    }
    // SAFETY: pbData is non-null and describes cbData bytes owned by the
    // CryptMsgGetParam-returned buffer; we copy the contents out.
    Some(unsafe { std::slice::from_raw_parts(b.pbData, b.cbData as usize) }.to_vec())
}

/// Fetch the bytes of a CryptMsg parameter, allocating the buffer ourselves
/// after a size query.
fn get_param_bytes(msg: &Msg, param: u32, index: u32) -> Result<Vec<u8>, Pkcs7Error> {
    let mut size: u32 = 0;
    // SAFETY: size query; passing None for the output buffer.
    unsafe {
        windows::Win32::Security::Cryptography::CryptMsgGetParam(
            msg.0.as_ptr(),
            param,
            index,
            None,
            &mut size,
        )
    }
    .map_err(|e| err(e, "CryptMsgGetParam (size query)"))?;
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf is sized per the size query.
    unsafe {
        windows::Win32::Security::Cryptography::CryptMsgGetParam(
            msg.0.as_ptr(),
            param,
            index,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut size,
        )
    }
    .map_err(|e| err(e, "CryptMsgGetParam"))?;
    buf.truncate(size as usize);
    Ok(buf)
}

/// Fetch a fixed-size CryptMsg parameter and reinterpret it as `T`.
fn get_param_value<T: Copy>(msg: &Msg, param: u32, index: u32) -> Result<T, Pkcs7Error> {
    let buf = get_param_bytes(msg, param, index)?;
    if buf.len() < size_of::<T>() {
        return Err(bogus_err("CryptMsgGetParam returned short buffer"));
    }
    // SAFETY: buffer is at least size_of::<T>() bytes; T is Copy.
    Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<T>()) })
}

/// Extract the Issuer Name DER and SerialNumber blob from an X509Certificate
/// by re-decoding it through CryptDecodeObjectEx. The Issuer blob holds the
/// DER-encoded Name SEQUENCE; SerialNumber holds CryptoAPI's
/// little-endian-ordered bytes.
fn extract_issuer_and_serial(
    cert: &crate::x509::win::X509CertificateInner,
) -> Result<(Vec<u8>, Vec<u8>), crate::x509::X509Error> {
    let info = cert.0.cert_info();
    // SAFETY: Issuer.pbData/cbData are populated by CryptoAPI and remain
    // valid for the lifetime of the cert context.
    let issuer =
        unsafe { std::slice::from_raw_parts(info.Issuer.pbData, info.Issuer.cbData as usize) }
            .to_vec();
    // SAFETY: same lifetime/validity argument as Issuer above.
    let serial = unsafe {
        std::slice::from_raw_parts(info.SerialNumber.pbData, info.SerialNumber.cbData as usize)
    }
    .to_vec();
    Ok((issuer, serial))
}

#[cfg(any(test, feature = "test_helpers"))]
struct NCryptProv(windows::Win32::Security::Cryptography::NCRYPT_PROV_HANDLE);

#[cfg(any(test, feature = "test_helpers"))]
impl Drop for NCryptProv {
    fn drop(&mut self) {
        // SAFETY: opened by NCryptOpenStorageProvider; NCRYPT_HANDLE and
        // NCRYPT_PROV_HANDLE both wrap the same usize value.
        let _ = unsafe {
            windows::Win32::Security::Cryptography::NCryptFreeObject(
                windows::Win32::Security::Cryptography::NCRYPT_HANDLE(self.0.0),
            )
        };
    }
}

#[cfg(any(test, feature = "test_helpers"))]
struct NCryptKey(windows::Win32::Security::Cryptography::NCRYPT_KEY_HANDLE);

#[cfg(any(test, feature = "test_helpers"))]
impl Drop for NCryptKey {
    fn drop(&mut self) {
        // SAFETY: imported by NCryptImportKey; NCRYPT_HANDLE and
        // NCRYPT_KEY_HANDLE both wrap the same usize value.
        let _ = unsafe {
            windows::Win32::Security::Cryptography::NCryptFreeObject(
                windows::Win32::Security::Cryptography::NCRYPT_HANDLE(self.0.0),
            )
        };
    }
}

#[cfg(any(test, feature = "test_helpers"))]
struct OwnedCertContext(NonNull<windows::Win32::Security::Cryptography::CERT_CONTEXT>);

#[cfg(any(test, feature = "test_helpers"))]
impl Drop for OwnedCertContext {
    fn drop(&mut self) {
        // SAFETY: produced by CertCreateCertificateContext; safe to free
        // exactly once.
        let _ = unsafe {
            windows::Win32::Security::Cryptography::CertFreeCertificateContext(Some(
                self.0.as_ptr(),
            ))
        };
    }
}
