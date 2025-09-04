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
use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
use rsa::Oaep;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use rsa::pkcs8::EncodePrivateKey;
use rsa::rand_core::OsRng;
use sha1::Sha1;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

pub type IgvmAgentScriptPlan = HashMap<IgvmAttestRequestType, VecDeque<AgentAction>>;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("unsupported igvm attest request type: {0:?}")]
    UnsupportedIgvmAttestRequestType(u32),
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

/// Test IGVM agent includes states that need to be persisted.
#[derive(Debug, Clone, Default)]
pub(crate) struct TestIgvmAgent {
    /// Optional RSA private key used for attestation.
    secret_key: Option<RsaPrivateKey>,
    /// Optional DES key
    des_key: Option<[u8; 32]>,
    /// Optional scripted actions per request type for tests.
    plan: Option<Arc<Mutex<IgvmAgentScriptPlan>>>,
}

/// Possible actions for the IGVM agent to take in response to a request.
#[derive(Debug, Clone)]
pub enum AgentAction {
    RespondSuccess,
    RespondFailure,
    NoResponse,
}

fn test_config_to_plan(test_config: &IgvmAttestTestConfig) -> IgvmAgentScriptPlan {
    let mut plan = IgvmAgentScriptPlan::default();

    match test_config {
        IgvmAttestTestConfig::AkCertRequestFailureAndRetry => {
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([
                    AgentAction::NoResponse,
                    AgentAction::RespondFailure,
                    AgentAction::RespondSuccess,
                ]),
            );
        }
        IgvmAttestTestConfig::AkCertPersistentAcrossBoot => {
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([AgentAction::RespondSuccess, AgentAction::NoResponse]),
            );
        }
    }

    plan
}

impl TestIgvmAgent {
    /// Create an instance with optional `test_config`.
    pub(crate) fn new(test_config: Option<IgvmAttestTestConfig>) -> Self {
        tracing::info!(test_config = ?test_config, "Create test IGVM agent");

        let plan = test_config.map(|config| Arc::new(Mutex::new(test_config_to_plan(&config))));

        Self {
            secret_key: None,
            des_key: None,
            plan,
        }
    }

    /// Install a scripted plan used by tests.
    pub fn set_plan(&mut self, plan: IgvmAgentScriptPlan) {
        self.plan = Some(Arc::new(Mutex::new(plan)));
    }

    pub(crate) fn handle_request(&mut self, request_bytes: &[u8]) -> Result<(Vec<u8>, u32), Error> {
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

        // If a plan is provided and has a queued action for this request type,
        // execute it. This allows tests to force success/no-response, etc.
        // Take next scripted action, if any, in a separate scope to avoid holding the lock
        // across calls that may mutably borrow self.
        if let Some(action) = (|| {
            self.plan
                .as_ref()
                .and_then(|plan| plan.lock().ok())
                .and_then(|mut map| {
                    map.get_mut(&request.header.request_type)
                        .and_then(|q| q.pop_front())
                })
        })() {
            match action {
                AgentAction::NoResponse => {
                    tracing::info!(?request.header.request_type, "Test plan: NoResponse");
                    return Ok((vec![], 0));
                }
                AgentAction::RespondSuccess => {
                    tracing::info!(?request.header.request_type, "Test plan: RespondSuccess");
                    match request.header.request_type {
                        IgvmAttestRequestType::WRAPPED_KEY_REQUEST => {
                            self.initialize_keys()?;
                            let data = self
                                .generate_mock_wrapped_key_response()
                                .map_err(Error::WrappedKeyError)?;
                            let header = IgvmAttestWrappedKeyResponseHeader {
                                data_size: (data.len()
                                    + size_of::<IgvmAttestWrappedKeyResponseHeader>())
                                    as u32,
                                version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                                error_info: IgvmErrorInfo::default(),
                            };
                            let payload = [header.as_bytes(), &data].concat();
                            return Ok((payload.clone(), payload.len() as u32));
                        }
                        IgvmAttestRequestType::KEY_RELEASE_REQUEST => {
                            if self.secret_key.is_none() {
                                // Ensure keys exist so we can generate a valid JWT response
                                self.initialize_keys()?;
                            }
                            let jwt = self
                                .generate_mock_key_release_response(
                                    &request_bytes[size_of::<IgvmAttestRequest>()..],
                                )
                                .map_err(Error::KeyReleaseError)?;
                            let data = jwt.as_bytes().to_vec();
                            let header = IgvmAttestKeyReleaseResponseHeader {
                                data_size: (data.len()
                                    + size_of::<IgvmAttestKeyReleaseResponseHeader>())
                                    as u32,
                                version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                                error_info: IgvmErrorInfo::default(),
                            };
                            let payload = [header.as_bytes(), &data].concat();
                            return Ok((payload.clone(), payload.len() as u32));
                        }
                        IgvmAttestRequestType::AK_CERT_REQUEST => {
                            let data = vec![0xab; 2500];
                            let header = IgvmAttestAkCertResponseHeader {
                                data_size: (data.len()
                                    + size_of::<IgvmAttestAkCertResponseHeader>())
                                    as u32,
                                version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                                error_info: IgvmErrorInfo::default(),
                            };
                            let payload = [header.as_bytes(), &data].concat();
                            return Ok((payload.clone(), payload.len() as u32));
                        }
                        ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
                    }
                }
                AgentAction::RespondFailure => {
                    tracing::info!(?request.header.request_type, "Test plan: RespondFailure");
                    match request.header.request_type {
                        IgvmAttestRequestType::WRAPPED_KEY_REQUEST => {
                            let header = IgvmAttestWrappedKeyResponseHeader {
                                data_size: size_of::<IgvmAttestWrappedKeyResponseHeader>() as u32,
                                version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                                error_info: IgvmErrorInfo {
                                    error_code: 0x1234,
                                    http_status_code: 400,
                                    igvm_signal: IgvmSignal::default().with_retry(false),
                                    reserved: [0; 3],
                                },
                            };
                            let payload = header.as_bytes().to_vec();
                            return Ok((payload.clone(), payload.len() as u32));
                        }
                        IgvmAttestRequestType::KEY_RELEASE_REQUEST => {
                            let header = IgvmAttestKeyReleaseResponseHeader {
                                data_size: size_of::<IgvmAttestKeyReleaseResponseHeader>() as u32,
                                version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                                error_info: IgvmErrorInfo {
                                    error_code: 0x1234,
                                    http_status_code: 400,
                                    igvm_signal: IgvmSignal::default().with_retry(false),
                                    reserved: [0; 3],
                                },
                            };
                            let payload = header.as_bytes().to_vec();
                            return Ok((payload.clone(), payload.len() as u32));
                        }
                        IgvmAttestRequestType::AK_CERT_REQUEST => {
                            let header = IgvmAttestAkCertResponseHeader {
                                data_size: size_of::<IgvmAttestAkCertResponseHeader>() as u32,
                                version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                                error_info: IgvmErrorInfo {
                                    error_code: 0x1234,
                                    http_status_code: 400,
                                    igvm_signal: IgvmSignal::default().with_retry(false),
                                    reserved: [0; 3],
                                },
                            };
                            let payload = header.as_bytes().to_vec();
                            return Ok((payload.clone(), payload.len() as u32));
                        }
                        ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
                    }
                }
            }
        }

        // If no plan is provided, fall back to the default behavior that
        // always return valid responses.
        let (response, length) = match request.header.request_type {
            IgvmAttestRequestType::AK_CERT_REQUEST => {
                tracing::info!("Send a response for AK_CERT_REQEUST");

                let data = vec![0xab; 2500];
                let header = IgvmAttestAkCertResponseHeader {
                    data_size: (data.len() + size_of::<IgvmAttestAkCertResponseHeader>()) as u32,
                    version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
                    error_info: IgvmErrorInfo::default(),
                };
                let payload = [header.as_bytes(), &data].concat();
                let payload_len = payload.len() as u32;

                (payload, payload_len)
            }
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
                    self.initialize_keys()?;
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

        Ok((response, length))
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
