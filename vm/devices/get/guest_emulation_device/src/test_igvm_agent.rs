// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test IGVM Agent
//!
//! This module contains a test version of the IGVM agent for handling
//! attestation requests in VMM tests.

//! NOTE: This is a test implementation and should not be used in production.
//! The cryptographic crates (`rsa`, `sha1`, and `aes_kw`) are not vetted
//! for production use and are *exclusively* for this test module on the
//! Windows platform.

use aes_kw::KekAes256;
use base64::Engine;
use get_resources::ged::IgvmAttestTestConfig;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestKeyReleaseResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequest;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestWrappedKeyResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use rsa::Oaep;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use rsa::pkcs8::EncodePrivateKey;
use rsa::rand_core::OsRng;
use sha1::Sha1;
use sha2::Sha256;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("unsupported igvm attest request type: {0:?}")]
    UnsupportedIgvmAttestRequestType(u32),
    #[error("invalid igvm attest state: {state:?}, test config: {test_config:?}")]
    InvalidIgvmAttestState {
        state: IgvmAttestState,
        test_config: Option<IgvmAttestTestConfig>,
    },
    #[error("failed to initialize keys for attestation")]
    KeyInitializationFailed(#[source] rsa::Error),
    #[error("keys not initialized")]
    KeysNotInitialized,
    #[error("invalid igvm attest request")]
    InvalidIgvmAttestRequest,
    #[error("failed to generate mock wrapped key response")]
    WrappedKeyError(#[source] WrappedKeyError),
    #[error("failed to generate mock key release response")]
    KeyReleaseError(#[source] KeyReleaseError),
}

#[derive(Debug, Error)]
pub(crate) enum WrappedKeyError {
    #[error("RSA encryption error")]
    RsaEncryptionError(#[source] rsa::Error),
    #[error("JSON serialization error")]
    JsonSerializeError(#[source] serde_json::Error),
    #[error("DES key not initialized")]
    DesKeyNotInitialized,
    #[error("Secret key not initialized")]
    SecretKeyNotInitialized,
}

#[derive(Debug, Error)]
pub(crate) enum KeyReleaseError {
    #[error("invalid runtime claims")]
    InvalidRuntimeClaims,
    #[error("missing transfer key in runtime claims")]
    MissingTransferKeyInRuntimeClaims,
    #[error("failed to convert JWK RSA key")]
    ConvertJwkRsaFailed(#[source] rsa::Error),
    #[error("Secret key not initialized")]
    SecretKeyNotInitialized,
    #[error("failed to convert RSA key to PKCS8 format")]
    RsaToPkcs8Error(#[source] rsa::pkcs8::Error),
    #[error("AES key wrap error")]
    AesKeyWrapError(aes_kw::Error),
    #[error("RSA encryption error")]
    RsaEncryptionError(#[source] rsa::Error),
    #[error("JSON serialization error")]
    JsonSerializeError(#[source] serde_json::Error),
}

/// Simple state machine to support AK cert preserving test.
// TODO: add more states to cover other test scenarios.
#[derive(Debug, Clone, Copy)]
pub(crate) enum IgvmAttestState {
    Init,
    SendEmptyAkCert,
    SendInvalidAkCert,
    SendValidAkCert,
    Done,
}

// Test IGVM agent includes states that need to be persisted.
#[derive(Debug, Clone)]
pub(crate) struct TestIgvmAgent {
    /// State machine for `handle_request`
    pub state: IgvmAttestState,
    /// Optional RSA private key used for attestation.
    pub secret_key: Option<RsaPrivateKey>,
    /// Optional DES key
    pub des_key: Option<[u8; 32]>,
}

impl TestIgvmAgent {
    pub(crate) fn handle_request(
        &mut self,
        request_bytes: &[u8],
        test_config: Option<&IgvmAttestTestConfig>,
    ) -> Result<(Vec<u8>, u32), Error> {
        tracing::info!(state = ?self.state, test_config = ?test_config, "Test IGVM agent");

        let request = IgvmAttestRequest::read_from_prefix(request_bytes)
            .map_err(|_| Error::InvalidIgvmAttestRequest)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        // Validate and extract runtime claims
        // The runtime claims are appended after the fixed-size IgvmAttestRequest structure
        let runtime_claims_start = size_of::<IgvmAttestRequest>();
        let runtime_claims_end =
            runtime_claims_start + request.request_data.variable_data_size as usize;
        if request_bytes.len() < runtime_claims_end {
            tracing::error!(
                "Message buffer too short to contain runtime claims, len={}, expected_end={}",
                request_bytes.len(),
                runtime_claims_end
            );
            return Err(Error::InvalidIgvmAttestRequest);
        }
        let runtime_claims_bytes = &request_bytes[runtime_claims_start..runtime_claims_end];

        // Determine the first state before handling the request
        if matches!(self.state, IgvmAttestState::Init) {
            self.update_igvm_attest_state(test_config).map_err(|_| {
                Error::InvalidIgvmAttestState {
                    state: self.state,
                    test_config: test_config.cloned(),
                }
            })?;
            tracing::info!(state = ?self.state, test_config = ?test_config, "Update init state");
        }

        let (response, length) = match request.header.request_type {
            IgvmAttestRequestType::AK_CERT_REQUEST => match self.state {
                IgvmAttestState::SendEmptyAkCert => {
                    tracing::info!("Send an empty response for AK_CERT_REQEUST");
                    (vec![], 0)
                }
                IgvmAttestState::SendInvalidAkCert => {
                    tracing::info!("Return an invalid response for AK_CERT_REQUEST");
                    (
                        vec![],
                        get_protocol::IGVM_ATTEST_VMWP_GENERIC_ERROR_CODE as u32,
                    )
                }
                IgvmAttestState::SendValidAkCert => {
                    tracing::info!("Send a response for AK_CERT_REQEUST");
                    let data = vec![0xab; 2500];
                    let header = IgvmAttestAkCertResponseHeader {
                        data_size: (data.len() + size_of::<IgvmAttestAkCertResponseHeader>())
                            as u32,
                        version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                        error_info: IgvmErrorInfo::default(),
                    };
                    let payload = [header.as_bytes(), &data].concat();
                    let payload_len = payload.len() as u32;

                    (payload, payload_len)
                }
                IgvmAttestState::Done => {
                    tracing::info!("Bypass AK_CERT_REQEUST");

                    return Ok((vec![], 0));
                }
                _ => {
                    return Err(Error::InvalidIgvmAttestState {
                        state: self.state,
                        test_config: test_config.cloned(),
                    });
                }
            },
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST => {
                tracing::info!("Send a response for WRAPPED_KEY_REQUEST");

                self.initialize_keys()?;

                let mock_response = self
                    .generate_mock_wrapped_key_response()
                    .map_err(Error::WrappedKeyError)?;
                let data = mock_response;

                let header = IgvmAttestWrappedKeyResponseHeader {
                    data_size: (data.len() + size_of::<IgvmAttestWrappedKeyResponseHeader>())
                        as u32,
                    version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                    error_info: IgvmErrorInfo::default(),
                };
                let payload = [header.as_bytes(), &data].concat();
                let payload_len = payload.len() as u32;

                tracing::info!(
                    "Sent mock response for WRAPPED_KEY_REQUEST, length: {}",
                    payload.len()
                );

                (payload, payload_len)
            }
            IgvmAttestRequestType::KEY_RELEASE_REQUEST => {
                tracing::info!("Send a response for KEY_RELEASE_REQUEST");

                if self.secret_key.is_none() {
                    return Err(Error::KeysNotInitialized);
                }

                // Generate a mock JWT response for testing - convert request to proper type
                let jwt_response = self
                    .generate_mock_key_release_response(runtime_claims_bytes)
                    .map_err(Error::KeyReleaseError)?;
                let data = jwt_response.as_bytes().to_vec();

                let header = IgvmAttestKeyReleaseResponseHeader {
                    data_size: (data.len() + size_of::<IgvmAttestKeyReleaseResponseHeader>())
                        as u32,
                    version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                    error_info: IgvmErrorInfo::default(),
                };
                let payload = [header.as_bytes(), &data].concat();
                let payload_len = payload.len() as u32;

                tracing::info!(
                    "Sent mock response for KEY_RELEASE_REQUEST, length: {}",
                    payload.len()
                );

                (payload, payload_len)
            }
            ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
        };

        // Update state
        self.update_igvm_attest_state(test_config)
            .map_err(|_| Error::InvalidIgvmAttestState {
                state: self.state,
                test_config: test_config.cloned(),
            })?;

        tracing::info!(state = ?self.state, test_config = ?test_config, "Updated state after request");

        Ok((response, length))
    }

    /// Update IGVM Attest state machine based on IGVM Attest test config.
    pub(crate) fn update_igvm_attest_state(
        &mut self,
        test_config: Option<&IgvmAttestTestConfig>,
    ) -> Result<(), Error> {
        match test_config {
            // No test config set, default to sending valid AK cert for now.
            None => {
                self.state = IgvmAttestState::SendValidAkCert;
            }
            // State machine for testing retrying AK cert request after failing attempt.
            Some(IgvmAttestTestConfig::AkCertRequestFailureAndRetry) => match self.state {
                IgvmAttestState::Init => self.state = IgvmAttestState::SendEmptyAkCert,
                IgvmAttestState::SendEmptyAkCert => self.state = IgvmAttestState::SendInvalidAkCert,
                IgvmAttestState::SendInvalidAkCert => self.state = IgvmAttestState::SendValidAkCert,
                IgvmAttestState::SendValidAkCert => self.state = IgvmAttestState::Done,
                IgvmAttestState::Done => {}
            },
            // State machine for testing AK cert persistency across boots.
            Some(IgvmAttestTestConfig::AkCertPersistentAcrossBoot) => match self.state {
                IgvmAttestState::Init => self.state = IgvmAttestState::SendValidAkCert,
                IgvmAttestState::SendValidAkCert => self.state = IgvmAttestState::SendEmptyAkCert,
                IgvmAttestState::SendEmptyAkCert => self.state = IgvmAttestState::Done,
                IgvmAttestState::Done => {}
                _ => {
                    return Err(Error::InvalidIgvmAttestState {
                        state: self.state,
                        test_config: test_config.copied(),
                    });
                }
            },
        }

        Ok(())
    }

    pub(crate) fn initialize_keys(&mut self) -> Result<(), Error> {
        if self.secret_key.is_some() && self.des_key.is_some() {
            // Keys are already initialized, nothing to do.
            return Ok(());
        }

        if self.secret_key.is_some() || self.des_key.is_some() {
            // If one key is initialized, the other must be too.
            return Err(Error::KeysNotInitialized);
        }

        let mut rng = OsRng;
        let private_key =
            RsaPrivateKey::new(&mut rng, 2048).map_err(Error::KeyInitializationFailed)?;
        let mut des_key = [0u8; 32];

        self.secret_key = Some(private_key);

        rsa::rand_core::RngCore::fill_bytes(&mut rng, &mut des_key);
        self.des_key = Some(des_key);

        Ok(())
    }

    pub(crate) fn generate_mock_wrapped_key_response(&self) -> Result<Vec<u8>, WrappedKeyError> {
        use openhcl_attestation_protocol::igvm_attest::cps;

        // Ensure DES key is available
        let des_key = if let Some(key) = self.des_key {
            key
        } else {
            return Err(WrappedKeyError::DesKeyNotInitialized);
        };

        let secret_key = self
            .secret_key
            .as_ref()
            .ok_or(WrappedKeyError::SecretKeyNotInitialized)?;

        // Encrypt the DES key using RSA-OAEP
        let mut rng = OsRng;
        let padding = Oaep::new::<Sha256>();
        let rsa_public = RsaPublicKey::from(secret_key);
        let encrypted_des = rsa_public
            .encrypt(&mut rng, padding, &des_key)
            .map_err(WrappedKeyError::RsaEncryptionError)?;

        let aes_info = cps::AesInfo {
            ciphertext: encrypted_des.to_vec(),
        };

        let key_reference = serde_json::json!({
            "key_info": {
                "host": "name"
            },
            "attestation_info": {
                "host": "attestation_name"
            }
        });

        let encryption_info = cps::EncryptionInfo {
            aes_info,
            key_reference,
        };
        let disk_encryption_settings = cps::DiskEncryptionSettings { encryption_info };
        let payload = cps::VmmdBlob {
            disk_encryption_settings,
        };

        let payload =
            serde_json::to_string(&payload).map_err(WrappedKeyError::JsonSerializeError)?;

        tracing::info!(
            "Sending WRAPPED_KEY response (length: {}): {}",
            payload.len(),
            payload
        );

        Ok(payload.as_bytes().to_vec())
    }

    /// Generate a mock JWT response for testing KEY_RELEASE_REQUEST
    pub(crate) fn generate_mock_key_release_response(
        &self,
        runtime_claims_bytes: &[u8],
    ) -> Result<String, KeyReleaseError> {
        use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RuntimeClaims;

        // Parse the runtime claims JSON
        let runtime_claims = String::from_utf8_lossy(runtime_claims_bytes);

        tracing::info!(
            "Attempting to parse runtime claims JSON (length: {}): {}",
            runtime_claims.len(),
            runtime_claims
        );

        let runtime_claims: RuntimeClaims = serde_json::from_str(&runtime_claims).map_err(|e| {
            tracing::error!("Failed to parse runtime claims JSON: {}", e);
            KeyReleaseError::InvalidRuntimeClaims
        })?;

        // Extract the RSA key from the runtime claims
        let transfer_key = runtime_claims
            .keys
            .iter()
            .find(|key| key.kid == "HCLTransferKey")
            .ok_or(KeyReleaseError::MissingTransferKeyInRuntimeClaims)?;

        tracing::info!(
            "Extracted transfer key from runtime claims: kid={}",
            transfer_key.kid
        );

        // Convert the JWK RSA key to a usable RSA public key
        let rsa_public_key = RsaPublicKey::new(
            rsa::BigUint::from_bytes_be(&transfer_key.n),
            rsa::BigUint::from_bytes_be(&transfer_key.e),
        )
        .map_err(KeyReleaseError::ConvertJwkRsaFailed)?;

        // Generate the JWT response using the extracted RSA key
        self.generate_jwt_with_rsa_key(rsa_public_key)
    }

    /// Generate a mock JWT response for testing KEY_RELEASE_REQUEST
    pub(crate) fn generate_jwt_with_rsa_key(
        &self,
        public_key: RsaPublicKey,
    ) -> Result<String, KeyReleaseError> {
        use openhcl_attestation_protocol::igvm_attest::akv;

        let secret_key = self
            .secret_key
            .as_ref()
            .ok_or(KeyReleaseError::SecretKeyNotInitialized)?;
        let mut rng = OsRng;

        // Generate or reuse the Key Encryption Key (KEK) for AES-KW
        let mut kek_bytes = [0u8; 32];
        rsa::rand_core::RngCore::fill_bytes(&mut rng, &mut kek_bytes);
        let kek = KekAes256::from(kek_bytes);

        // Wrap the target RSA key using AES-KW - pad to expected 256 bytes
        let wrapped_key = kek
            .wrap_with_padding_vec(
                secret_key
                    .to_pkcs8_der()
                    .map_err(KeyReleaseError::RsaToPkcs8Error)?
                    .as_bytes(),
            )
            .map_err(KeyReleaseError::AesKeyWrapError)?;

        // Encrypt the KEK using RSA-OAEP
        let padding = Oaep::new::<Sha1>();
        let encrypted_kek = public_key
            .encrypt(&mut rng, padding, &kek_bytes)
            .map_err(KeyReleaseError::RsaEncryptionError)?;

        // Create the PKCS#11 RSA-AES-KEY-WRAP payload: RSA-encrypted KEK + AES-wrapped key
        let pkcs11_payload = [encrypted_kek, wrapped_key].concat();

        // Create JWT header
        let header = akv::AkvKeyReleaseJwtHeader {
            alg: "RS256".to_string(),
            x5c: vec![],
        };
        // Header is a base64-url encoded JSON object
        let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&header).map_err(KeyReleaseError::JsonSerializeError)?);

        // Create JWT body with the PKCS#11 payload
        let key_hsm = akv::AkvKeyReleaseKeyBlob {
            ciphertext: pkcs11_payload,
        };

        let body = akv::AkvKeyReleaseJwtBody {
            response: akv::AkvKeyReleaseResponse {
                key: akv::AkvKeyReleaseKeyObject {
                    key: akv::AkvJwk {
                        key_hsm: serde_json::to_string(&key_hsm)
                            .map_err(KeyReleaseError::JsonSerializeError)?
                            .as_bytes()
                            .to_vec(),
                    },
                },
            },
        };
        let body_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&body).map_err(KeyReleaseError::JsonSerializeError)?);

        // Create a mock signature (empty for testing)
        let signature_b64 = "";

        // Return properly formatted JWT: header.body.signature
        Ok(format!("{}.{}.{}", header_b64, body_b64, signature_b64))
    }
}
