// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! macOS Security.framework backend for PKCS#7 verification.
//!
//! Uses the CMSDecoder API for parsing and signature verification, and
//! SecTrust for certificate chain evaluation against a caller-provided
//! trust store.

// UNSAFETY: calling Security.framework and CoreFoundation C APIs via FFI.
#![expect(unsafe_code)]

use super::*;
use crate::mac::*;
use std::ptr;

// === Security FFI types ===

type SecCertificateRef = CFTypeRef;
type SecPolicyRef = CFTypeRef;
type SecTrustRef = CFTypeRef;
type CMSDecoderRef = CFTypeRef;

/// `kCMSSignerValid` — the cryptographic signature is valid.
const K_CMS_SIGNER_VALID: u32 = 1;

/// SecTrustOptionFlags — allow expired leaf certificates.
const K_SEC_TRUST_OPTION_ALLOW_EXPIRED: u32 = 0x01;
/// SecTrustOptionFlags — allow expired root certificates.
const K_SEC_TRUST_OPTION_ALLOW_EXPIRED_ROOT: u32 = 0x08;

#[repr(C)]
struct CFArrayCallBacks {
    version: CFIndex,
    retain: Option<unsafe extern "C" fn(CFAllocatorRef, CFTypeRef) -> CFTypeRef>,
    release: Option<unsafe extern "C" fn(CFAllocatorRef, CFTypeRef)>,
    copy_description: Option<unsafe extern "C" fn(CFTypeRef) -> CFTypeRef>,
    equal: Option<unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> u8>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFArrayRef;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCertificateCreateWithData(
        allocator: CFAllocatorRef,
        data: CFDataRef,
    ) -> SecCertificateRef;
    fn SecPolicyCreateBasicX509() -> SecPolicyRef;
    fn SecTrustSetAnchorCertificates(trust: SecTrustRef, anchor_certs: CFArrayRef) -> OsStatusCode;
    fn SecTrustSetAnchorCertificatesOnly(trust: SecTrustRef, only: u8) -> OsStatusCode;
    fn SecTrustEvaluateWithError(trust: SecTrustRef, error: *mut CFErrorRef) -> u8;
    fn SecTrustSetOptions(trust: SecTrustRef, options: u32) -> OsStatusCode;
    fn SecTrustSetNetworkFetchAllowed(trust: SecTrustRef, allowed: u8) -> OsStatusCode;

    fn CMSDecoderCreate(decoder_out: *mut CMSDecoderRef) -> OsStatusCode;
    fn CMSDecoderUpdateMessage(
        decoder: CMSDecoderRef,
        msg: *const u8,
        msg_len: usize,
    ) -> OsStatusCode;
    fn CMSDecoderSetDetachedContent(decoder: CMSDecoderRef, content: CFDataRef) -> OsStatusCode;
    fn CMSDecoderFinalizeMessage(decoder: CMSDecoderRef) -> OsStatusCode;
    fn CMSDecoderGetNumSigners(decoder: CMSDecoderRef, num_signers: *mut usize) -> OsStatusCode;
    fn CMSDecoderCopySignerStatus(
        decoder: CMSDecoderRef,
        signer_index: usize,
        policy_or_array: CFTypeRef,
        evaluate_sec_trust: u8,
        signer_status: *mut u32,
        sec_trust: *mut SecTrustRef,
        cert_verify_result_code: *mut OsStatusCode,
    ) -> OsStatusCode;
}

/// Create a Pkcs7Error from an OsStatusCode and operation description.
fn os_err(status: OsStatusCode, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError::OsStatus(status, op))
}

/// Create a Pkcs7Error for a null return with no OS error code.
fn null_err(op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError::Null(op))
}

pub struct Pkcs7CertStoreInner {
    certs: Vec<CfHandle>,
}

pub struct Pkcs7SignedDataInner {
    decoder: CfHandle,
}

impl Pkcs7CertStoreInner {
    pub fn new() -> Result<Self, Pkcs7Error> {
        Ok(Self { certs: Vec::new() })
    }

    pub fn add_cert_der(&mut self, data: &[u8]) -> Result<(), Pkcs7Error> {
        let cf_data = cf_data(data, "create CFData for certificate").map_err(Pkcs7Error)?;
        // SAFETY: cf_data wraps a valid CFDataRef; SecCertificateCreateWithData
        // is safe with valid CFData input.
        let cert = unsafe { SecCertificateCreateWithData(ptr::null(), cf_data.0) };
        if cert.is_null() {
            return Err(null_err("create SecCertificate from DER"));
        }
        self.certs.push(CfHandle(cert));
        Ok(())
    }

    /// Build a CFArray of the stored SecCertificateRef values.
    ///
    /// The array retains its own references, so the caller can release it
    /// independently of the store.
    fn as_cf_array(&self) -> Result<CfHandle, Pkcs7Error> {
        let refs: Vec<CFTypeRef> = self.certs.iter().map(|c| c.0).collect();
        // SAFETY: refs contains valid SecCertificateRef pointers.
        // kCFTypeArrayCallBacks retains each element.
        unsafe {
            let array = CFArrayCreate(
                ptr::null(),
                refs.as_ptr(),
                refs.len() as CFIndex,
                &kCFTypeArrayCallBacks,
            );
            if array.is_null() {
                return Err(null_err("create CFArray of certificates"));
            }
            Ok(CfHandle(array))
        }
    }
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        // SAFETY: CMSDecoderCreate and CMSDecoderUpdateMessage are safe with
        // valid pointers and lengths.
        unsafe {
            let mut decoder: CMSDecoderRef = ptr::null();
            let status = CMSDecoderCreate(&mut decoder);
            if !status.success() {
                return Err(os_err(status, "create CMS decoder"));
            }
            let decoder = CfHandle(decoder);

            let status = CMSDecoderUpdateMessage(decoder.0, data.as_ptr(), data.len());
            if !status.success() {
                return Err(os_err(status, "update CMS decoder with message"));
            }

            Ok(Self { decoder })
        }
    }

    pub fn verify(
        self,
        store: Pkcs7CertStoreInner,
        signed_content: &[u8],
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        // Provide the detached content that was signed.
        let content_data =
            cf_data(signed_content, "create CFData for signed content").map_err(Pkcs7Error)?;

        // SAFETY: all CF/Security API calls use valid handles produced by
        // earlier successful calls. RAII wrappers ensure proper cleanup.
        unsafe {
            let status = CMSDecoderSetDetachedContent(self.decoder.0, content_data.0);
            if !status.success() {
                return Err(os_err(status, "set detached content"));
            }

            // Finalize the message (triggers internal parsing/validation).
            let status = CMSDecoderFinalizeMessage(self.decoder.0);
            if !status.success() {
                return Err(os_err(status, "finalize CMS message"));
            }

            // Get the number of signers.
            let mut num_signers: usize = 0;
            let status = CMSDecoderGetNumSigners(self.decoder.0, &mut num_signers);
            if !status.success() {
                return Err(os_err(status, "get number of signers"));
            }

            if num_signers == 0 {
                return Ok(false);
            }

            // Create a BasicX509 policy for trust evaluation.
            let policy = SecPolicyCreateBasicX509();
            if policy.is_null() {
                return Err(null_err("create BasicX509 policy"));
            }
            let _policy = CfHandle(policy);

            // Build the anchor cert array from the caller's trust store.
            let anchor_array = store.as_cf_array()?;

            // Verify each signer.
            for i in 0..num_signers {
                if !self.verify_signer(i, policy, &anchor_array, uefi_mode)? {
                    return Ok(false);
                }
            }

            Ok(true)
        }
    }

    /// Verify a single signer at the given index.
    ///
    /// # Safety
    ///
    /// `policy` must be a valid SecPolicyRef. `anchor_array` must contain a
    /// valid CFArrayRef of SecCertificateRef values.
    unsafe fn verify_signer(
        &self,
        signer_index: usize,
        policy: SecPolicyRef,
        anchor_array: &CfHandle,
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        let mut signer_status: u32 = 0;
        let mut trust: SecTrustRef = ptr::null();
        let mut cert_verify_status = OsStatusCode::SUCCESS;

        // Get the signer status without evaluating trust — we need to
        // configure our own anchor certificates first.
        // SAFETY: decoder, policy are valid CF handles from earlier
        // successful API calls.
        let status = unsafe {
            CMSDecoderCopySignerStatus(
                self.decoder.0,
                signer_index,
                policy,
                0, // evaluateSecTrust = false
                &mut signer_status,
                &mut trust,
                &mut cert_verify_status,
            )
        };
        if !status.success() {
            return Err(os_err(status, "get signer status"));
        }

        // Check if the cryptographic signature is valid.
        if signer_status != K_CMS_SIGNER_VALID {
            return Ok(false);
        }

        if trust.is_null() {
            return Err(null_err("get SecTrust from signer"));
        }
        let _trust = CfHandle(trust);

        // Set our anchor certificates as the only trust roots, excluding
        // the system root store. This is equivalent to OpenSSL's
        // PARTIAL_CHAIN behavior: any cert in the caller's store
        // terminates the chain.
        // SAFETY: trust and anchor_array are valid CF handles.
        let status = unsafe { SecTrustSetAnchorCertificates(trust, anchor_array.0) };
        if !status.success() {
            return Err(os_err(status, "set anchor certificates"));
        }

        // SAFETY: trust is a valid SecTrustRef.
        let status = unsafe { SecTrustSetAnchorCertificatesOnly(trust, 1) };
        if !status.success() {
            return Err(os_err(status, "set anchor certificates only"));
        }

        // Disable network fetches so that trust evaluation never attempts
        // OCSP or CRL lookups, matching the other backends which perform
        // no revocation checking.
        // SAFETY: trust is a valid SecTrustRef.
        let status = unsafe { SecTrustSetNetworkFetchAllowed(trust, 0) };
        if !status.success() {
            return Err(os_err(status, "disable network fetch"));
        }

        if uefi_mode {
            // Ignore certificate time validity, matching the OpenSSL
            // backend's NO_CHECK_TIME flag. UEFI signing keys are
            // commonly expired.
            let options = K_SEC_TRUST_OPTION_ALLOW_EXPIRED | K_SEC_TRUST_OPTION_ALLOW_EXPIRED_ROOT;
            // SAFETY: trust is a valid SecTrustRef.
            let status = unsafe { SecTrustSetOptions(trust, options) };
            if !status.success() {
                return Err(os_err(status, "set trust options"));
            }
        }

        // Evaluate the trust chain.
        let mut error: CFErrorRef = ptr::null();
        // SAFETY: trust is a valid SecTrustRef.
        let trusted = unsafe { SecTrustEvaluateWithError(trust, &mut error) };
        if !error.is_null() {
            // SAFETY: error is a non-null CFErrorRef that we must release.
            unsafe { CFRelease(error) };
        }

        Ok(trusted != 0)
    }
}
