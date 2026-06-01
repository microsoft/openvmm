// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signature verification using the Windows CryptoAPI.

use super::*;
use std::ptr;
use windows::Win32::Foundation::CRYPT_E_NOT_FOUND;
use windows::Win32::Foundation::NTE_BAD_SIGNATURE;
use windows::Win32::Security::Cryptography::*;

fn err(e: windows_result::Error, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError(e, op))
}

/// RAII wrapper for HCERTSTORE.
struct CertStoreHandle(HCERTSTORE);

impl Drop for CertStoreHandle {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased.
        let _ = unsafe { CertCloseStore(Some(self.0), 0) };
    }
}

/// RAII wrapper for *mut CERT_CONTEXT.
struct CertContextHandle(*mut CERT_CONTEXT);

impl Drop for CertContextHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: pointer is valid.
            let _ = unsafe { CertFreeCertificateContext(Some(self.0)) };
        }
    }
}

/// RAII wrapper for *mut CERT_CHAIN_CONTEXT.
struct ChainContextHandle(*mut CERT_CHAIN_CONTEXT);

impl Drop for ChainContextHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: pointer is valid.
            unsafe { CertFreeCertificateChain(self.0) };
        }
    }
}

/// RAII wrapper for HCERTCHAINENGINE.
struct ChainEngineHandle(HCERTCHAINENGINE);

impl Drop for ChainEngineHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: handle is valid and not aliased.
            unsafe { CertFreeCertificateChainEngine(Some(self.0)) };
        }
    }
}

/// RAII wrapper for HCRYPTMSG (*mut c_void).
struct MsgHandle(*mut std::ffi::c_void);

impl Drop for MsgHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle is valid and not aliased.
            let _ = unsafe { CryptMsgClose(Some(self.0)) };
        }
    }
}

pub struct Pkcs7CertStoreInner {
    store: CertStoreHandle,
}

pub struct Pkcs7SignedDataInner {
    msg: MsgHandle,
}

const ENCODING: CERT_QUERY_ENCODING_TYPE =
    CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0);

const ENCODING_RAW: u32 = X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0;

impl Pkcs7CertStoreInner {
    pub fn new() -> Result<Self, Pkcs7Error> {
        // SAFETY: CERT_STORE_PROV_MEMORY with no extra params is safe.
        let store = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_MEMORY,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(0),
                None,
            )
        }
        .map_err(|e| err(e, "open memory cert store"))?;

        Ok(Self {
            store: CertStoreHandle(store),
        })
    }

    pub fn add_cert_der(&mut self, data: &[u8]) -> Result<(), Pkcs7Error> {
        // SAFETY: store handle is valid, data is a valid slice.
        unsafe {
            CertAddEncodedCertificateToStore(
                Some(self.store.0),
                ENCODING,
                data,
                CERT_STORE_ADD_ALWAYS,
                None,
            )
        }
        .map_err(|e| err(e, "add certificate to store"))
    }
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        // Step 1: Decode the PKCS7 message with CMSG_DETACHED_FLAG
        // since content is provided separately.
        // SAFETY: standard CryptMsg decode sequence.
        let msg =
            unsafe { CryptMsgOpenToDecode(ENCODING_RAW, CMSG_DETACHED_FLAG, 0, None, None, None) };
        if msg.is_null() {
            return Err(err(
                windows_result::Error::from_thread(),
                "open message for decode",
            ));
        }
        let msg = MsgHandle(msg);

        // SAFETY: msg handle is valid, data is a valid slice.
        unsafe { CryptMsgUpdate(msg.0, Some(data), true) }
            .map_err(|e| err(e, "decode pkcs7 message"))?;

        Ok(Self { msg })
    }

    pub fn verify(
        self,
        store: Pkcs7CertStoreInner,
        signed_content: &[u8],
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        // Feed the detached content.
        // SAFETY: msg handle is valid.
        unsafe { CryptMsgUpdate(self.msg.0, Some(signed_content), true) }
            .map_err(|e| err(e, "feed signed content"))?;

        // Step 2: Open the message's embedded certificate store.
        // SAFETY: msg handle is valid.
        let msg_store = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_MSG,
                ENCODING,
                None,
                CERT_OPEN_STORE_FLAGS(0),
                Some(self.msg.0.cast_const()),
            )
        }
        .map_err(|e| err(e, "open message cert store"))?;
        let msg_store = CertStoreHandle(msg_store);

        // Step 3: Get the signer count.
        let mut signer_count: u32 = 0;
        let mut count_size = size_of::<u32>() as u32;
        // SAFETY: msg handle is valid.
        unsafe {
            CryptMsgGetParam(
                self.msg.0,
                CMSG_SIGNER_COUNT_PARAM,
                0,
                Some((&raw mut signer_count).cast()),
                &mut count_size,
            )
        }
        .map_err(|e| err(e, "get signer count"))?;

        if signer_count == 0 {
            return Ok(false);
        }

        // Create a custom chain engine with `hExclusiveRoot` set to the
        // caller's trust store so that only the caller-provided certificates
        // are treated as trust anchors (not the Windows system root store).
        //
        // The engine depends only on the trust store, so create it once here
        // and share it across all signers.
        let engine_config = CERT_CHAIN_ENGINE_CONFIG {
            cbSize: size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32,
            hExclusiveRoot: store.store.0,
            ..Default::default()
        };

        let mut engine = HCERTCHAINENGINE::default();
        // SAFETY: engine_config is valid with a valid hExclusiveRoot store handle.
        unsafe { CertCreateCertificateChainEngine(&engine_config, &mut engine) }
            .map_err(|e| err(e, "create chain engine"))?;
        let engine = ChainEngineHandle(engine);

        // Verify each signer in the message.
        for signer_index in 0..signer_count {
            if !self.verify_signer(&msg_store, &engine, signer_index, uefi_mode)? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Verify a single signer at the given index.
    fn verify_signer(
        &self,
        msg_store: &CertStoreHandle,
        engine: &ChainEngineHandle,
        signer_index: u32,
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        // Step 4: Get signer certificate info.
        let mut signer_info_size: u32 = 0;
        // SAFETY: msg handle is valid. First call to get required size.
        unsafe {
            CryptMsgGetParam(
                self.msg.0,
                CMSG_SIGNER_CERT_INFO_PARAM,
                signer_index,
                None,
                &mut signer_info_size,
            )
        }
        .map_err(|e| err(e, "get signer info size"))?;

        if signer_info_size == 0 {
            return Ok(false);
        }

        // Use u64 vec to guarantee 8-byte alignment for the CERT_INFO struct
        // that Windows will write into this buffer.
        let aligned_len = (signer_info_size as usize).div_ceil(8);
        let mut signer_info_buf = vec![0u64; aligned_len];
        // SAFETY: msg handle is valid, buffer is large enough and properly aligned.
        unsafe {
            CryptMsgGetParam(
                self.msg.0,
                CMSG_SIGNER_CERT_INFO_PARAM,
                signer_index,
                Some((signer_info_buf.as_mut_ptr()).cast()),
                &mut signer_info_size,
            )
        }
        .map_err(|e| err(e, "get signer cert info"))?;

        let cert_info = signer_info_buf.as_ptr().cast::<CERT_INFO>();

        // Step 5: Find the signer certificate in the message's embedded store.
        // SAFETY: msg_store is valid, cert_info points to valid CERT_INFO.
        let signer_cert =
            unsafe { CertGetSubjectCertificateFromStore(msg_store.0, ENCODING, cert_info) };
        if signer_cert.is_null() {
            let e = windows_result::Error::from_thread();
            // The signer's certificate not being present in the message is a
            // signature verification failure, not an internal error.
            if e.code() == CRYPT_E_NOT_FOUND {
                return Ok(false);
            }
            return Err(err(e, "find signer certificate"));
        }
        let signer_cert = CertContextHandle(signer_cert);

        // Step 6: Verify the cryptographic signature.
        // SAFETY: msg handle is valid, signer_cert points to valid CERT_INFO.
        let verify_result = unsafe {
            CryptMsgControl(
                self.msg.0,
                0,
                CMSG_CTRL_VERIFY_SIGNATURE,
                Some((*signer_cert.0).pCertInfo as *const _),
            )
        };

        if let Err(e) = verify_result {
            // A bad signature is a verification failure; anything else is an
            // internal error that should be propagated. Newer Windows builds
            // surface the NTSTATUS form instead of the `NTE_*` HRESULT, so
            // accept both.
            if e.code() == NTE_BAD_SIGNATURE
                || e.code().0 == windows::Win32::Foundation::STATUS_INVALID_SIGNATURE.0
            {
                return Ok(false);
            }
            return Err(err(e, "verify message signature"));
        }

        // Step 7: Build a certificate chain from the signer cert to our
        // trusted store using the shared chain engine.
        //
        // SAFETY: CERT_CHAIN_PARA is a plain data struct that is valid when zeroed.
        let mut chain_para: CERT_CHAIN_PARA = unsafe { std::mem::zeroed() };
        chain_para.cbSize = size_of::<CERT_CHAIN_PARA>() as u32;

        // No revocation checking, matching the OpenSSL backend which does not
        // perform any revocation checks.
        let chain_flags: u32 = 0;

        let mut chain_context: *mut CERT_CHAIN_CONTEXT = ptr::null_mut();
        // SAFETY: engine, signer_cert, and msg_store handles are valid.
        //
        // `CertGetCertificateChain` only fails for internal errors such as
        // invalid parameters or out-of-memory; trust problems with the chain
        // are reported via the `TrustStatus` field of the returned context and
        // are surfaced later by `CertVerifyCertificateChainPolicy`.
        unsafe {
            CertGetCertificateChain(
                Some(engine.0),
                signer_cert.0,
                None,
                Some(msg_store.0),
                &chain_para,
                chain_flags,
                None,
                &mut chain_context,
            )
        }
        .map_err(|e| err(e, "build certificate chain"))?;

        let chain_context = ChainContextHandle(chain_context);

        // Step 8: Verify the chain policy.
        // SAFETY: CERT_CHAIN_POLICY_PARA is a plain data struct that is valid when zeroed.
        let mut policy_para: CERT_CHAIN_POLICY_PARA = unsafe { std::mem::zeroed() };
        policy_para.cbSize = size_of::<CERT_CHAIN_POLICY_PARA>() as u32;

        if uefi_mode {
            // See `Pkcs7SignedData::verify` for the semantics of `uefi_mode`.
            //
            // 1. Partial chain: already provided by `hExclusiveRoot` on the
            //    chain engine above -- any cert in the caller's trust store
            //    is treated as a trust anchor, including intermediates.
            //
            //    Do NOT also set `CERT_CHAIN_POLICY_ALLOW_UNKNOWN_CA_FLAG`
            //    here: that would mask the legitimate "no matching trust
            //    anchor" error when the trust store lacks an appropriate
            //    cert, causing untrusted signatures to be accepted.
            //
            // 2. Ignore time validity: `IGNORE_ALL_NOT_TIME_VALID_FLAGS`
            //    covers the signer, intermediates, and root
            //
            // 3. Any key-usage / EKU: `IGNORE_WRONG_USAGE_FLAG` accepts
            //    certs regardless of their EKU / key-usage extensions.
            //
            // `IGNORE_ALL_REV_UNKNOWN_FLAGS` is also set so as to not
            // perform revocation checking. In practice CryptoAPI is not
            // asked to fetch revocation info here (`chain_flags = 0`),
            // but the flag makes the intent explicit and is harmless if
            // a provider does supply one.
            policy_para.dwFlags = CERT_CHAIN_POLICY_IGNORE_ALL_NOT_TIME_VALID_FLAGS
                | CERT_CHAIN_POLICY_IGNORE_WRONG_USAGE_FLAG
                | CERT_CHAIN_POLICY_IGNORE_ALL_REV_UNKNOWN_FLAGS;
        }

        // SAFETY: CERT_CHAIN_POLICY_STATUS is a plain data struct that is valid when zeroed.
        let mut policy_status: CERT_CHAIN_POLICY_STATUS = unsafe { std::mem::zeroed() };
        policy_status.cbSize = size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;

        // SAFETY: chain_context is valid.
        let policy_result = unsafe {
            CertVerifyCertificateChainPolicy(
                CERT_CHAIN_POLICY_BASE,
                chain_context.0,
                &policy_para,
                &mut policy_status,
            )
        };

        // FALSE from CertVerifyCertificateChainPolicy indicates a
        // function-level error (e.g., invalid parameters), not a policy
        // violation. Surface it as an internal error rather than panicking,
        // since this code processes potentially untrusted signature material.
        if !policy_result.as_bool() {
            return Err(err(
                windows_result::Error::from_thread(),
                "verify certificate chain policy",
            ));
        }

        if policy_status.dwError != 0 {
            return Ok(false);
        }

        Ok(true)
    }
}
