// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification using the Windows CryptoAPI
//! (crypt32.dll). This backend is fully native — it does not depend on the
//! `der` or `x509-cert` RustCrypto crates.

use super::X509Error;
use crate::win::KeyHandle;
use std::ffi::c_void;
use std::ptr::NonNull;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::CERT_AUTHORITY_KEY_ID2_INFO;
use windows::Win32::Security::Cryptography::CERT_CONTEXT;
use windows::Win32::Security::Cryptography::CERT_EXTENSION;
use windows::Win32::Security::Cryptography::CERT_INFO;
use windows::Win32::Security::Cryptography::CERT_KEY_CERT_SIGN_KEY_USAGE;
use windows::Win32::Security::Cryptography::CERT_NAME_ATTR_TYPE;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CERT_PUBLIC_KEY_INFO;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CRYPT_ALGORITHM_IDENTIFIER;
use windows::Win32::Security::Cryptography::CRYPT_BIT_BLOB;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CRYPT_ENCODE_OBJECT_FLAGS;
use windows::Win32::Security::Cryptography::CRYPT_IMPORT_PUBLIC_KEY_FLAGS;
use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows::Win32::Security::Cryptography::X509_ASN_ENCODING;
use windows::Win32::Security::Cryptography::X509_AUTHORITY_KEY_ID2;
use windows::Win32::Security::Cryptography::X509_CERT;
use windows::Win32::Security::Cryptography::X509_KEY_USAGE;
use windows::Win32::Security::Cryptography::szOID_AUTHORITY_KEY_IDENTIFIER2;
use windows::Win32::Security::Cryptography::szOID_COMMON_NAME;
use windows::Win32::Security::Cryptography::szOID_KEY_USAGE;
use windows::Win32::Security::Cryptography::szOID_RSA_RSA;
use windows::Win32::Security::Cryptography::szOID_RSA_SHA256RSA;
use windows::Win32::Security::Cryptography::szOID_SUBJECT_KEY_IDENTIFIER;
use windows::core::PCSTR;

fn err(err: windows_result::Error, op: &'static str) -> X509Error {
    X509Error(crate::BackendError(err, op))
}

fn rsa_err(err: windows_result::Error, op: &'static str) -> crate::rsa::RsaError {
    crate::rsa::RsaError(crate::BackendError(err, op))
}

fn last_err(op: &'static str) -> X509Error {
    err(windows_result::Error::from_thread(), op)
}

/// RAII wrapper around `PCCERT_CONTEXT`.
pub(crate) struct CertContext(pub(crate) NonNull<CERT_CONTEXT>);

// SAFETY: cert context can be sent across threads.
unsafe impl Send for CertContext {}
// SAFETY: cert context is read-only after creation and safe to share.
unsafe impl Sync for CertContext {}

impl Drop for CertContext {
    fn drop(&mut self) {
        // SAFETY: handle is valid; CertFreeCertificateContext tolerates
        // the call exactly once per CertCreateCertificateContext.
        let _ = unsafe {
            windows::Win32::Security::Cryptography::CertFreeCertificateContext(Some(
                self.0.as_ptr().cast_const(),
            ))
        };
    }
}

impl CertContext {
    pub(crate) fn cert_context(&self) -> &CERT_CONTEXT {
        // SAFETY: self.0 is a valid PCCERT_CONTEXT owned by `self`, and the
        // pointed-to CERT_CONTEXT is immutable for the lifetime of `self`.
        unsafe { self.0.as_ref() }
    }

    pub(crate) fn cert_info(&self) -> &CERT_INFO {
        // SAFETY: pCertInfo is a valid pointer owned by the cert context
        // and the pointed-to CERT_INFO is immutable for the lifetime of
        // `self`.
        unsafe { &*self.cert_context().pCertInfo }
    }
}

pub(crate) struct X509CertificateInner(pub(crate) CertContext);

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        // SAFETY: `data` is a valid byte slice for its declared length.
        let p = unsafe {
            windows::Win32::Security::Cryptography::CertCreateCertificateContext(
                X509_ASN_ENCODING,
                data,
            )
        };
        let p = NonNull::new(p).ok_or_else(|| last_err("CertCreateCertificateContext"))?;
        Ok(Self(CertContext(p)))
    }

    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, crate::rsa::RsaError> {
        let info = &self.0.cert_info().SubjectPublicKeyInfo;

        // Reject non-RSA keys. The OID is a PSTR (null-terminated ASCII).

        let oid = info.Algorithm.pszObjId;
        // SAFETY: info points to a valid CERT_PUBLIC_KEY_INFO owned by
        // the cert context; the OID strings produced by crypt32 and the
        // szOID_* constants are null-terminated ASCII.
        let is_rsa = !oid.is_null() && unsafe { oid.as_bytes() == szOID_RSA_RSA.as_bytes() };
        if !is_rsa {
            return Err(rsa_err(
                windows_result::Error::from_hresult(windows::core::HRESULT(
                    windows::Win32::Foundation::E_NOTIMPL.0,
                )),
                "non-RSA public key in certificate",
            ));
        }

        // Import the SubjectPublicKeyInfo into a BCrypt key handle
        let mut h = BCRYPT_KEY_HANDLE::default();
        // SAFETY: info points to a valid CERT_PUBLIC_KEY_INFO owned by
        // the cert context.
        unsafe {
            windows::Win32::Security::Cryptography::CryptImportPublicKeyInfoEx2(
                X509_ASN_ENCODING,
                info,
                CRYPT_IMPORT_PUBLIC_KEY_FLAGS(0),
                None,
                &mut h,
            )
        }
        .map_err(|e| rsa_err(e, "CryptImportPublicKeyInfoEx2"))?;
        let key = KeyHandle(h);
        Ok(crate::rsa::RsaPublicKey(
            crate::rsa::win::RsaPublicKeyInner(key),
        ))
    }

    pub fn verify(
        &self,
        issuer_public_key: &crate::rsa::RsaPublicKey,
    ) -> Result<bool, crate::rsa::RsaError> {
        // Decode the cert into CERT_SIGNED_CONTENT_INFO to extract the
        // TBS bytes, signature algorithm OID, and signature bits, then
        // delegate to the RsaPublicKey::pkcs1_verify path.
        // This avoids constructing a CERT_PUBLIC_KEY_INFO from scratch,
        // which CryptVerifyCertificateSignatureEx is finicky about.
        let ctx = self.0.cert_context();
        let signed =
            decode_object::<windows::Win32::Security::Cryptography::CERT_SIGNED_CONTENT_INFO>(
                X509_CERT,
                &CRYPT_INTEGER_BLOB {
                    cbData: ctx.cbCertEncoded,
                    pbData: ctx.pbCertEncoded,
                },
            )
            .map_err(|X509Error(e)| crate::rsa::RsaError(e))?;

        // Hash algorithm from the signature algorithm OID.
        let oid = signed.value.SignatureAlgorithm.pszObjId;
        if oid.is_null() {
            return Err(rsa_err(
                windows_result::Error::from_hresult(windows::core::HRESULT(
                    windows::Win32::Foundation::E_NOTIMPL.0,
                )),
                "missing signature algorithm OID",
            ));
        }
        // SAFETY: ASN.1-decoded OID strings from crypt32 are null-terminated,
        // as are the szOID_* constants.
        let hash = unsafe {
            let oid_bytes = oid.as_bytes();
            if oid_bytes == szOID_RSA_SHA256RSA.as_bytes() {
                crate::HashAlgorithm::Sha256
            } else if oid_bytes
                == windows::Win32::Security::Cryptography::szOID_RSA_SHA384RSA.as_bytes()
            {
                crate::HashAlgorithm::Sha384
            } else if oid_bytes
                == windows::Win32::Security::Cryptography::szOID_RSA_SHA1RSA.as_bytes()
            {
                #[expect(deprecated)]
                {
                    crate::HashAlgorithm::Sha1
                }
            } else {
                return Err(rsa_err(
                    windows_result::Error::from_hresult(windows::core::HRESULT(
                        windows::Win32::Foundation::E_NOTIMPL.0,
                    )),
                    "unsupported signature algorithm OID",
                ));
            }
        };

        let tbs = blob_as_slice(&signed.value.ToBeSigned).ok_or_else(|| {
            rsa_err(
                windows_result::Error::from_hresult(windows::core::HRESULT(-1)),
                "malformed ToBeSigned blob",
            )
        })?;
        // Signature is a CRYPT_BIT_BLOB; reject empty/null defensively.
        let sig_bits = &signed.value.Signature;
        // For X.509 RSA signatures the BIT STRING must be byte-aligned;
        // a non-zero cUnusedBits means the encoding is malformed for this
        // algorithm. Treat as an invalid signature rather than verifying
        // bytes that don't reflect the encoded value.
        if sig_bits.cUnusedBits != 0 {
            return Ok(false);
        }
        let sig = if sig_bits.cbData == 0 || sig_bits.pbData.is_null() {
            &[][..]
        } else {
            // SAFETY: pbData is non-null and describes cbData bytes owned
            // by the decoded CERT_SIGNED_CONTENT_INFO allocation.
            unsafe { std::slice::from_raw_parts(sig_bits.pbData, sig_bits.cbData as usize) }
        };

        issuer_public_key.pkcs1_verify(tbs, sig, hash)
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> Result<bool, X509Error> {
        let issuer_info = self.0.cert_info();
        let subject_info = subject.0.cert_info();

        // Compare issuer DN of subject with subject DN of issuer.
        // SAFETY: blobs come from valid CERT_INFOs.
        let names_match = unsafe {
            windows::Win32::Security::Cryptography::CertCompareCertificateName(
                X509_ASN_ENCODING,
                &issuer_info.Subject,
                &subject_info.Issuer,
            )
        };
        if !names_match.as_bool() {
            return Ok(false);
        }

        // KeyUsage on issuer must permit cert signing if present.
        if let Some(ku_ext) = find_extension(issuer_info, szOID_KEY_USAGE) {
            let bits = decode_object::<CRYPT_BIT_BLOB>(X509_KEY_USAGE, &ku_ext.Value)?;
            let mut u32_usage: u32 = 0;
            // SAFETY: bits.pbData points to at least bits.cbData bytes if
            // non-null. Copy up to 4 bytes; default to 0 otherwise.
            unsafe {
                let n = bits.value.cbData.min(4) as usize;
                if !bits.value.pbData.is_null() {
                    std::ptr::copy_nonoverlapping(
                        bits.value.pbData,
                        std::ptr::from_mut(&mut u32_usage).cast::<u8>(),
                        n,
                    );
                }
            }
            if u32_usage & CERT_KEY_CERT_SIGN_KEY_USAGE == 0 {
                return Ok(false);
            }
        }

        // AKID validation on subject (if present), against issuer.
        if let Some(akid_ext) = find_extension(subject_info, szOID_AUTHORITY_KEY_IDENTIFIER2) {
            let akid = decode_object::<CERT_AUTHORITY_KEY_ID2_INFO>(
                X509_AUTHORITY_KEY_ID2,
                &akid_ext.Value,
            )?;

            if akid.value.KeyId.cbData != 0 {
                let skid_ext = find_extension(issuer_info, szOID_SUBJECT_KEY_IDENTIFIER);
                match skid_ext {
                    Some(ext) => {
                        // X509_OCTET_STRING decodes to CRYPT_INTEGER_BLOB
                        let skid = decode_object::<CRYPT_INTEGER_BLOB>(
                            windows::Win32::Security::Cryptography::X509_OCTET_STRING,
                            &ext.Value,
                        )?;
                        let a = blob_as_slice(&akid.value.KeyId);
                        let b = blob_as_slice(&skid.value);
                        if a.is_none() || b.is_none() || a != b {
                            return Ok(false);
                        }
                    }
                    None => return Ok(false),
                }
            }

            if akid.value.AuthorityCertSerialNumber.cbData != 0 {
                let a = blob_as_slice(&akid.value.AuthorityCertSerialNumber);
                let b = blob_as_slice(&issuer_info.SerialNumber);
                if a.is_none() || b.is_none() || a != b {
                    return Ok(false);
                }
            }

            // AuthorityCertIssuer (a CERT_ALT_NAME_INFO): if any
            // DirectoryName entry is present, at least one must equal the
            // issuer's subject DN.
            if akid.value.AuthorityCertIssuer.cAltEntry != 0 {
                // SAFETY: rgAltEntry/cAltEntry describe a valid array
                // owned by the decoded structure.
                let entries = unsafe {
                    std::slice::from_raw_parts(
                        akid.value.AuthorityCertIssuer.rgAltEntry,
                        akid.value.AuthorityCertIssuer.cAltEntry as usize,
                    )
                };
                let mut has_dn = false;
                let mut has_match = false;
                for e in entries {
                    // CERT_ALT_NAME_DIRECTORYNAME = 5
                    if e.dwAltNameChoice == 5 {
                        has_dn = true;
                        // SAFETY: union: DirectoryName is the active field.
                        let dn = unsafe { e.Anonymous.DirectoryName };
                        // SAFETY: comparing two cert name blobs.
                        let m = unsafe {
                            windows::Win32::Security::Cryptography::CertCompareCertificateName(
                                X509_ASN_ENCODING,
                                &dn,
                                &issuer_info.Subject,
                            )
                        };
                        if m.as_bool() {
                            has_match = true;
                            break;
                        }
                    }
                }
                if has_dn && !has_match {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        let ctx = self.0.cert_context();
        // SAFETY: pbCertEncoded/cbCertEncoded describe a valid byte slice.
        let s =
            unsafe { std::slice::from_raw_parts(ctx.pbCertEncoded, ctx.cbCertEncoded as usize) };
        Ok(s.to_vec())
    }

    pub fn issuer_dn(&self) -> Result<String, X509Error> {
        use windows::Win32::Security::Cryptography::CERT_X500_NAME_STR;
        let info = self.0.cert_info();
        let blob = &info.Issuer;
        // SAFETY: blob is a valid CRYPT_INTEGER_BLOB owned by the cert info;
        // passing None for psz queries the required length in WCHARs.
        let needed = unsafe {
            windows::Win32::Security::Cryptography::CertNameToStrW(
                X509_ASN_ENCODING,
                blob,
                CERT_X500_NAME_STR,
                None,
            )
        };
        if needed <= 1 {
            return Ok(String::new());
        }
        let mut buf = vec![0u16; needed as usize];
        // SAFETY: buf is sized per the previous query.
        let written = unsafe {
            windows::Win32::Security::Cryptography::CertNameToStrW(
                X509_ASN_ENCODING,
                blob,
                CERT_X500_NAME_STR,
                Some(&mut buf),
            )
        };
        if written == 0 {
            return Err(last_err("CertNameToStrW"));
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len]))
    }

    pub fn serial_number(&self) -> Result<Vec<u8>, X509Error> {
        let info = self.0.cert_info();
        blob_as_slice(&info.SerialNumber)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                err(
                    windows_result::Error::from_hresult(windows::core::HRESULT(-1)),
                    "malformed serial number blob",
                )
            })
    }

    pub fn subject_common_name(&self) -> Result<Option<String>, X509Error> {
        let oid = szOID_COMMON_NAME.0.cast::<c_void>();
        // SAFETY: ctx is valid; passing None for psznamestring queries the
        // required size.
        let needed = unsafe {
            windows::Win32::Security::Cryptography::CertGetNameStringW(
                self.0.0.as_ptr().cast_const(),
                CERT_NAME_ATTR_TYPE,
                0,
                Some(oid),
                None,
            )
        };
        // 0 from the size query indicates a CryptoAPI failure; 1 (just the
        // NUL terminator) indicates the attribute is absent.
        if needed == 0 {
            return Err(last_err("CertGetNameStringW (size query)"));
        }
        if needed == 1 {
            return Ok(None);
        }
        let mut buf = vec![0u16; needed as usize];
        // SAFETY: buf is sized per the previous query.
        let written = unsafe {
            windows::Win32::Security::Cryptography::CertGetNameStringW(
                self.0.0.as_ptr().cast_const(),
                CERT_NAME_ATTR_TYPE,
                0,
                Some(oid),
                Some(&mut buf),
            )
        };
        if written == 0 {
            return Err(last_err("CertGetNameStringW"));
        }
        // Strip trailing NUL.
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..len])))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn build_self_signed(
        key: &crate::rsa::RsaKeyPair,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        use windows::Win32::Security::Cryptography::CERT_V3;
        use windows::Win32::Security::Cryptography::X509_CERT_TO_BE_SIGNED;

        // 1. Encode the subject (== issuer for self-signed) DN.
        let name_der = encode_x500_name(country, state, locality, organization, common_name)
            .context("encoding X.500 name")?;

        // 2. Build the SubjectPublicKeyInfo DER.
        let components = key.to_components();
        let pkcs1_pub = encode_pkcs1_rsa_pubkey(&components.modulus, &components.public_exponent)
            .context("encoding RSA public key")?;

        // 3. Build CERT_INFO and encode TBS.
        let mut serial_bytes: [u8; 1] = [1];
        let mut name_buf = name_der.clone();
        let mut pk_buf = pkcs1_pub.clone();
        let mut null_params = [0x05u8, 0x00];

        let cert_info = CERT_INFO {
            dwVersion: CERT_V3,
            SerialNumber: CRYPT_INTEGER_BLOB {
                cbData: serial_bytes.len() as u32,
                pbData: serial_bytes.as_mut_ptr(),
            },
            SignatureAlgorithm: CRYPT_ALGORITHM_IDENTIFIER {
                pszObjId: windows::core::PSTR(szOID_RSA_SHA256RSA.0.cast_mut()),
                Parameters: CRYPT_INTEGER_BLOB {
                    cbData: null_params.len() as u32,
                    pbData: null_params.as_mut_ptr(),
                },
            },
            Issuer: CRYPT_INTEGER_BLOB {
                cbData: name_buf.len() as u32,
                pbData: name_buf.as_mut_ptr(),
            },
            NotBefore: unix_to_filetime(0),
            NotAfter: unix_to_filetime(i32::MAX as i64),
            Subject: CRYPT_INTEGER_BLOB {
                cbData: name_buf.len() as u32,
                pbData: name_buf.as_mut_ptr(),
            },
            SubjectPublicKeyInfo: CERT_PUBLIC_KEY_INFO {
                Algorithm: CRYPT_ALGORITHM_IDENTIFIER {
                    pszObjId: windows::core::PSTR(szOID_RSA_RSA.0.cast_mut()),
                    Parameters: CRYPT_INTEGER_BLOB {
                        cbData: null_params.len() as u32,
                        pbData: null_params.as_mut_ptr(),
                    },
                },
                PublicKey: CRYPT_BIT_BLOB {
                    cbData: pk_buf.len() as u32,
                    pbData: pk_buf.as_mut_ptr(),
                    cUnusedBits: 0,
                },
            },
            IssuerUniqueId: CRYPT_BIT_BLOB::default(),
            SubjectUniqueId: CRYPT_BIT_BLOB::default(),
            cExtension: 0,
            rgExtension: std::ptr::null_mut(),
        };

        let tbs = encode_object(X509_CERT_TO_BE_SIGNED, &cert_info)
            .context("encoding TBS certificate")?;

        // 4. Sign the TBS.
        let signature = key.pkcs1_sign(&tbs, crate::HashAlgorithm::Sha256)?;

        // 5. Build CERT_SIGNED_CONTENT_INFO and encode the whole cert.
        let mut tbs_buf = tbs.clone();
        let mut sig_buf = signature.clone();
        let mut null_params2 = [0x05u8, 0x00];
        let signed = windows::Win32::Security::Cryptography::CERT_SIGNED_CONTENT_INFO {
            ToBeSigned: CRYPT_INTEGER_BLOB {
                cbData: tbs_buf.len() as u32,
                pbData: tbs_buf.as_mut_ptr(),
            },
            SignatureAlgorithm: CRYPT_ALGORITHM_IDENTIFIER {
                pszObjId: windows::core::PSTR(szOID_RSA_SHA256RSA.0.cast_mut()),
                Parameters: CRYPT_INTEGER_BLOB {
                    cbData: null_params2.len() as u32,
                    pbData: null_params2.as_mut_ptr(),
                },
            },
            Signature: CRYPT_BIT_BLOB {
                cbData: sig_buf.len() as u32,
                pbData: sig_buf.as_mut_ptr(),
                cUnusedBits: 0,
            },
        };
        let cert_der = encode_object(X509_CERT, &signed).context("encoding X.509 certificate")?;

        Ok(Self::from_der(&cert_der)?)
    }
}

/// View a `CRYPT_INTEGER_BLOB` as a byte slice. Returns `None` if the blob
/// is malformed (non-zero `cbData` with a null `pbData`), since
/// `from_raw_parts` would be immediate UB in that case. An empty blob
/// returns `Some(&[])`.
fn blob_as_slice(blob: &CRYPT_INTEGER_BLOB) -> Option<&[u8]> {
    if blob.cbData == 0 {
        return Some(&[]);
    }
    if blob.pbData.is_null() {
        return None;
    }
    // SAFETY: pbData is non-null and describes cbData bytes owned by the
    // containing CryptoAPI allocation.
    Some(unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) })
}

/// Find an extension by OID in a CERT_INFO. Returns `None` if not present.
fn find_extension(info: &CERT_INFO, oid: PCSTR) -> Option<&CERT_EXTENSION> {
    if info.cExtension == 0 || info.rgExtension.is_null() {
        return None;
    }
    // SAFETY: rgExtension/cExtension describe a valid array.
    let exts = unsafe { std::slice::from_raw_parts(info.rgExtension, info.cExtension as usize) };
    // SAFETY: CertFindExtension is a deterministic comparison over the slice.
    let p = unsafe { windows::Win32::Security::Cryptography::CertFindExtension(oid, exts) };
    if p.is_null() {
        None
    } else {
        // SAFETY: p points into the same array, owned by the CERT_INFO.
        Some(unsafe { &*p })
    }
}

/// RAII wrapper for a CryptoAPI-allocated decoded structure. Frees with
/// `LocalFree` when dropped.
struct Decoded<T> {
    raw: NonNull<c_void>,
    /// Layout-compatible reference to the decoded structure inside `raw`.
    value: T,
    _phantom: std::marker::PhantomData<*const T>,
}

impl<T> Drop for Decoded<T> {
    fn drop(&mut self) {
        // SAFETY: raw was allocated by CryptoAPI via CRYPT_DECODE_ALLOC_FLAG.
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(self.raw.as_ptr()),
            ));
        }
    }
}

/// Decode a CryptoAPI-defined object using `CryptDecodeObjectEx` with the
/// alloc flag. The returned `Decoded<T>` owns the buffer.
fn decode_object<T: Copy>(
    struct_type: PCSTR,
    blob: &CRYPT_INTEGER_BLOB,
) -> Result<Decoded<T>, X509Error> {
    use windows::Win32::Security::Cryptography::CRYPT_DECODE_ALLOC_FLAG;

    let encoded = blob_as_slice(blob).ok_or_else(|| {
        err(
            windows_result::Error::from_hresult(windows::core::HRESULT(-1)),
            "CryptDecodeObjectEx: input blob has null pbData with non-zero cbData",
        )
    })?;
    let mut raw: *mut c_void = std::ptr::null_mut();
    let mut size: u32 = 0;
    // SAFETY: We pass CRYPT_DECODE_ALLOC_FLAG so the API allocates the
    // output buffer. `&mut raw` is the documented form when alloc flag is set.
    unsafe {
        windows::Win32::Security::Cryptography::CryptDecodeObjectEx(
            X509_ASN_ENCODING,
            struct_type,
            encoded,
            CRYPT_DECODE_ALLOC_FLAG,
            None,
            Some(std::ptr::from_mut(&mut raw).cast::<c_void>()),
            &mut size,
        )
    }
    .map_err(|e| err(e, "CryptDecodeObjectEx"))?;
    let raw = NonNull::new(raw).ok_or_else(|| {
        err(
            windows_result::Error::from_hresult(windows::core::HRESULT(-1)),
            "CryptDecodeObjectEx returned null",
        )
    })?;
    // Validate the API actually wrote a `T`-sized header before reading
    // it. A short buffer here would mean `read_unaligned` reads past the
    // allocation.
    if (size as usize) < size_of::<T>() {
        // SAFETY: raw was allocated by CryptoAPI via CRYPT_DECODE_ALLOC_FLAG.
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(raw.as_ptr()),
            ));
        }
        return Err(err(
            windows_result::Error::from_hresult(windows::core::HRESULT(-1)),
            "CryptDecodeObjectEx returned buffer smaller than expected struct",
        ));
    }
    // SAFETY: raw points to a `T` written by CryptoAPI (validated above to
    // be at least size_of::<T>() bytes). We copy it out so that `value`
    // lives at a stable address.
    let value = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast::<T>()) };
    Ok(Decoded {
        raw,
        value,
        _phantom: std::marker::PhantomData,
    })
}

#[cfg(any(test, feature = "test_helpers"))]
fn encode_object<T: ?Sized>(
    struct_type: PCSTR,
    value: &T,
) -> Result<Vec<u8>, windows_result::Error> {
    let mut size: u32 = 0;
    let value = std::ptr::from_ref(value).cast();
    // SAFETY: first call queries size.
    unsafe {
        windows::Win32::Security::Cryptography::CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            struct_type,
            value,
            CRYPT_ENCODE_OBJECT_FLAGS(0),
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
            CRYPT_ENCODE_OBJECT_FLAGS(0),
            None,
            Some(out.as_mut_ptr().cast::<c_void>()),
            &mut size,
        )
    }?;
    out.truncate(size as usize);
    Ok(out)
}

/// Convert a Unix timestamp (seconds since 1970-01-01) to a Windows
/// `FILETIME` (100-ns intervals since 1601-01-01).
#[cfg(any(test, feature = "test_helpers"))]
fn unix_to_filetime(unix_secs: i64) -> windows::Win32::Foundation::FILETIME {
    // FILETIME counts 100-ns intervals since 1601-01-01; Unix epoch is
    // 1970-01-01 which is 11_644_473_600 seconds later.
    let ticks = ((unix_secs + 11_644_473_600) as u64) * 10_000_000;
    windows::Win32::Foundation::FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    }
}

/// Build a single-RDN-per-component DN as `C=…, ST=…, L=…, O=…, CN=…`.
#[cfg(any(test, feature = "test_helpers"))]
fn encode_x500_name(
    country: &str,
    state: &str,
    locality: &str,
    organization: &str,
    common_name: &str,
) -> Result<Vec<u8>, windows_result::Error> {
    use windows::Win32::Security::Cryptography::CERT_NAME_INFO;
    use windows::Win32::Security::Cryptography::CERT_RDN;
    use windows::Win32::Security::Cryptography::CERT_RDN_ATTR;
    use windows::Win32::Security::Cryptography::CERT_RDN_PRINTABLE_STRING;

    let mut rdn_entries: Vec<(PCSTR, Vec<u8>)> = vec![
        (
            windows::Win32::Security::Cryptography::szOID_COUNTRY_NAME,
            country.as_bytes().to_vec(),
        ),
        (
            windows::Win32::Security::Cryptography::szOID_STATE_OR_PROVINCE_NAME,
            state.as_bytes().to_vec(),
        ),
        (
            windows::Win32::Security::Cryptography::szOID_LOCALITY_NAME,
            locality.as_bytes().to_vec(),
        ),
        (
            windows::Win32::Security::Cryptography::szOID_ORGANIZATION_NAME,
            organization.as_bytes().to_vec(),
        ),
        (szOID_COMMON_NAME, common_name.as_bytes().to_vec()),
    ];

    // Build CERT_RDN_ATTR list and one CERT_RDN per attribute (each in its
    // own RDN so the resulting DN is fully ordered).
    let mut attrs: Vec<CERT_RDN_ATTR> = Vec::with_capacity(rdn_entries.len());
    for (oid, bytes) in &mut rdn_entries {
        attrs.push(CERT_RDN_ATTR {
            pszObjId: windows::core::PSTR(oid.0.cast_mut()),
            dwValueType: CERT_RDN_PRINTABLE_STRING.0 as u32,
            Value: CRYPT_INTEGER_BLOB {
                cbData: bytes.len() as u32,
                pbData: bytes.as_mut_ptr(),
            },
        });
    }
    let mut rdns: Vec<CERT_RDN> = attrs
        .iter_mut()
        .map(|a| CERT_RDN {
            cRDNAttr: 1,
            rgRDNAttr: std::ptr::from_mut(a),
        })
        .collect();
    let name_info = CERT_NAME_INFO {
        cRDN: rdns.len() as u32,
        rgRDN: rdns.as_mut_ptr(),
    };
    let out = encode_object(
        windows::Win32::Security::Cryptography::X509_NAME,
        &name_info,
    )?;
    drop(rdns);
    drop(attrs);
    drop(rdn_entries);
    Ok(out)
}

/// Encode a PKCS#1 RSAPublicKey (`SEQUENCE { INTEGER n, INTEGER e }`) by
/// asking CryptoAPI to encode a `BCRYPT_RSAKEY_BLOB` via the
/// `CNG_RSA_PUBLIC_KEY_BLOB` struct type.
#[cfg(any(test, feature = "test_helpers"))]
fn encode_pkcs1_rsa_pubkey(n: &[u8], e: &[u8]) -> Result<Vec<u8>, windows_result::Error> {
    use windows::Win32::Security::Cryptography::BCRYPT_RSAKEY_BLOB;
    use windows::Win32::Security::Cryptography::BCRYPT_RSAPUBLIC_MAGIC;
    use windows::Win32::Security::Cryptography::CNG_RSA_PUBLIC_KEY_BLOB;

    // Compute bit length of the modulus (big-endian, ignoring leading zeros).
    let bit_length = {
        let mut bits = 0u32;
        for (i, &b) in n.iter().enumerate() {
            if b != 0 {
                bits = ((n.len() - i) as u32) * 8 - b.leading_zeros();
                break;
            }
        }
        bits
    };

    let header = BCRYPT_RSAKEY_BLOB {
        Magic: BCRYPT_RSAPUBLIC_MAGIC,
        BitLength: bit_length,
        cbPublicExp: e.len() as u32,
        cbModulus: n.len() as u32,
        cbPrime1: 0,
        cbPrime2: 0,
    };

    let mut blob = Vec::with_capacity(size_of::<BCRYPT_RSAKEY_BLOB>() + e.len() + n.len());
    // SAFETY: BCRYPT_RSAKEY_BLOB is #[repr(C)] with all-integer fields, so
    // any byte pattern is a valid representation.
    blob.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(&header).cast::<u8>(),
            size_of::<BCRYPT_RSAKEY_BLOB>(),
        )
    });
    blob.extend_from_slice(e);
    blob.extend_from_slice(n);

    encode_object(CNG_RSA_PUBLIC_KEY_BLOB, blob.as_slice())
}
