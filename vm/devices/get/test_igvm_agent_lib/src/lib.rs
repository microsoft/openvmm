// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test IGVM Agent
//!
//! This module contains a test version of the IGVM agent for handling
//! attestation requests in VMM tests.

//! NOTE: This is a test implementation and should not be used in production.

mod test_crypto;

use crate::test_crypto::DummyRng;
use crate::test_crypto::TestSha1;
use crate::test_crypto::aes_key_wrap_with_padding;
use base64::Engine;
use get_resources::ged::IgvmAttestTestConfig;
use inspect::Inspect;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestKeyReleaseResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestWrappedKeyResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
use rsa::Oaep;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use rsa::pkcs8::EncodePrivateKey;
use rsa::rand_core::OsRng;
use rsa::rand_core::RngCore;
use rsa::rand_core::SeedableRng;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::VecDeque;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported igvm attest request type: {0:?}")]
    UnsupportedIgvmAttestRequestType(u32),
    #[error("failed to initialize keys for attestation")]
    KeyInitializationFailed(#[source] rsa::Error),
    #[error("keys not initialized")]
    KeysNotInitialized,
    #[error("invalid igvm attest request version - expected {expected:?}, found {found:?}")]
    InvalidIgvmAttestRequestVersion {
        found: IgvmAttestRequestVersion,
        expected: IgvmAttestRequestVersion,
    },
    #[error("invalid igvm attest request")]
    InvalidIgvmAttestRequest,
    #[error("failed to generate mock wrapped key response")]
    WrappedKeyError(#[source] WrappedKeyError),
    #[error("failed to generate mock key release response")]
    KeyReleaseError(#[source] KeyReleaseError),
}

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum WrappedKeyError {
    #[error("RSA encryption error")]
    RsaEncryptionError(#[source] rsa::Error),
    #[error("JSON serialization error")]
    JsonSerializeError(#[source] serde_json::Error),
    #[error("DES key not initialized")]
    DesKeyNotInitialized,
    #[error("Secret key not initialized")]
    SecretKeyNotInitialized,
}

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum KeyReleaseError {
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
    #[error("RSA encryption error")]
    RsaEncryptionError(#[source] rsa::Error),
    #[error("JSON serialization error")]
    JsonSerializeError(#[source] serde_json::Error),
}

/// Test IGVM agent includes states that need to be persisted.
#[derive(Debug, Clone, Default)]
pub struct TestIgvmAgent {
    /// Optional RSA private key used for attestation.
    secret_key: Option<RsaPrivateKey>,
    /// Optional DES key
    des_key: Option<[u8; 32]>,
    /// Optional scripted actions per request type for tests.
    plan: Option<IgvmAgentTestPlan>,
    /// Track whether the plan has been installed to prevent multiple installations.
    plan_installed: bool,
}

/// Possible actions for the IGVM agent to take in response to a request.
#[derive(Debug, Clone)]
pub enum IgvmAgentAction {
    /// Emit a successful response payload.
    RespondSuccess,
    /// Emit a response that indicates a protocol error.
    RespondFailure,
    /// Skip responding to simulate a timeout.
    NoResponse,
}

/// IGVM Agent test plan specifying scripted actions for a request type.
pub type IgvmAgentTestPlan = HashMap<IgvmAttestRequestType, VecDeque<IgvmAgentAction>>;

/// Settings used to configure the IGVM agent for tests.
#[derive(Debug, Clone)]
pub enum IgvmAgentTestSetting {
    /// Use a pre-defined test configuration that maps to a plan.
    TestConfig(IgvmAttestTestConfig),
    /// Use a manually provided plan.
    TestPlan(IgvmAgentTestPlan),
}

impl Inspect for IgvmAgentTestSetting {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        match self {
            Self::TestConfig(cfg) => {
                resp.field("TestConfig", cfg);
            }
            Self::TestPlan(plan) => {
                let len = plan.len();
                resp.field("TestPlan len", len);
            }
        }
    }
}

fn test_config_to_plan(test_config: &IgvmAttestTestConfig) -> IgvmAgentTestPlan {
    let mut plan = IgvmAgentTestPlan::default();

    match test_config {
        IgvmAttestTestConfig::AkCertRequestFailureAndRetry => {
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondSuccess,
                ]),
            );
        }
        IgvmAttestTestConfig::AkCertPersistentAcrossBoot => {
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([IgvmAgentAction::RespondSuccess, IgvmAgentAction::NoResponse]),
            );
        }
    }

    plan
}

impl TestIgvmAgent {
    /// Create an instance.
    pub fn new() -> Self {
        Self {
            secret_key: None,
            des_key: None,
            plan: None,
            plan_installed: false,
        }
    }

    /// Install a scripted plan used by tests based on the setting.
    /// Can be called multiple times but will only install the plan once per instance.
    pub fn install_plan_from_setting(&mut self, setting: &IgvmAgentTestSetting) {
        // Only install the plan once per agent instance
        if self.plan_installed {
            return;
        }

        tracing::info!("install the scripted plan for test IGVM Agent");

        match setting {
            IgvmAgentTestSetting::TestPlan(plan) => {
                self.plan = Some(plan.clone());
            }
            IgvmAgentTestSetting::TestConfig(config) => {
                self.plan = Some(test_config_to_plan(config));
            }
        }

        self.plan_installed = true;
    }

    /// Take the next scripted action for the given request type, if any.
    pub fn take_next_action(
        &mut self,
        request_type: IgvmAttestRequestType,
    ) -> Option<IgvmAgentAction> {
        // Fast path: no plan installed.
        let plan = self.plan.as_mut()?;
        plan.get_mut(&request_type)?.pop_front()
    }

    /// Request handler.
    pub fn handle_request(&mut self, request_bytes: &[u8]) -> Result<(Vec<u8>, u32), Error> {
        let request = IgvmAttestRequestBase::read_from_prefix(request_bytes)
            .map_err(|_| Error::InvalidIgvmAttestRequest)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        // Validate and extract runtime claims
        // The version must be the current version to ensure the presence of the extension data structure.
        if request.request_data.version != IGVM_ATTEST_REQUEST_CURRENT_VERSION {
            return Err(Error::InvalidIgvmAttestRequestVersion {
                found: request.request_data.version,
                expected: IGVM_ATTEST_REQUEST_CURRENT_VERSION,
            })?;
        }

        // The runtime claims are appended after the fixed-size IgvmAttestRequestBase and IgvmAttestRequestDataExt structures.
        let runtime_claims_start =
            size_of::<IgvmAttestRequestBase>() + size_of::<IgvmAttestRequestDataExt>();
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

        let (response, length) = if let Some(action) =
            self.take_next_action(request.header.request_type)
        {
            // If a plan is provided and has a queued action for this request type,
            // execute it. This allows tests to force success/no-response, etc.
            match action {
                IgvmAgentAction::NoResponse => {
                    tracing::info!(?request.header.request_type, "Test plan: NoResponse");
                    (vec![], 0)
                }
                IgvmAgentAction::RespondSuccess => {
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
                            let payload_len = payload.len() as u32;

                            (payload, payload_len)
                        }
                        IgvmAttestRequestType::KEY_RELEASE_REQUEST => {
                            if self.secret_key.is_none() {
                                // Ensure keys exist so we can generate a valid JWT response
                                self.initialize_keys()?;
                            }
                            let jwt = self
                                .generate_mock_key_release_response(runtime_claims_bytes)
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
                            let payload_len = payload.len() as u32;

                            (payload, payload_len)
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
                            let payload_len = payload.len() as u32;

                            (payload, payload_len)
                        }
                        ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
                    }
                }
                IgvmAgentAction::RespondFailure => {
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
                            let payload_len = payload.len() as u32;

                            (payload, payload_len)
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
                            let payload_len = payload.len() as u32;

                            (payload, payload_len)
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
                            let payload_len = payload.len() as u32;

                            (payload.clone(), payload_len)
                        }
                        ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
                    }
                }
            }
        } else {
            // If no plan is provided, fall back to the default behavior that
            // always return valid responses.
            match request.header.request_type {
                IgvmAttestRequestType::AK_CERT_REQUEST => {
                    tracing::info!("Send a response for AK_CERT_REQUEST");

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
            }
        };

        Ok((response, length))
    }

    fn initialize_keys(&mut self) -> Result<(), Error> {
        if self.secret_key.is_some() && self.des_key.is_some() {
            // Keys are already initialized, nothing to do.
            return Ok(());
        }

        if self.secret_key.is_some() || self.des_key.is_some() {
            // If one key is initialized, the other must be too.
            return Err(Error::KeysNotInitialized);
        }

        let seed = 1234u64.to_le_bytes();
        let mut rng = DummyRng::from_seed(seed);
        let private_key =
            RsaPrivateKey::new(&mut rng, 2048).map_err(Error::KeyInitializationFailed)?;
        let mut des_key = [0u8; 32];

        self.secret_key = Some(private_key);

        RngCore::fill_bytes(&mut rng, &mut des_key);
        self.des_key = Some(des_key);

        Ok(())
    }

    fn generate_mock_wrapped_key_response(&self) -> Result<Vec<u8>, WrappedKeyError> {
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
            ciphertext: encrypted_des.clone(),
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
    fn generate_mock_key_release_response(
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
    fn generate_jwt_with_rsa_key(
        &self,
        public_key: RsaPublicKey,
    ) -> Result<String, KeyReleaseError> {
        use openhcl_attestation_protocol::igvm_attest::akv;

        let secret_key = self
            .secret_key
            .as_ref()
            .ok_or(KeyReleaseError::SecretKeyNotInitialized)?;
        let mut rng = OsRng;

        // Generate the KEK (32 bytes) and wrap the private key using internal wrapper
        let mut kek_bytes = [0u8; 32];
        RngCore::fill_bytes(&mut rng, &mut kek_bytes);
        let priv_key_der = secret_key
            .to_pkcs8_der()
            .map_err(KeyReleaseError::RsaToPkcs8Error)?;
        let wrapped_key = aes_key_wrap_with_padding(&kek_bytes, priv_key_der.as_bytes());

        // Encrypt the KEK using RSA-OAEP
        let padding = Oaep::new::<TestSha1>();
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
