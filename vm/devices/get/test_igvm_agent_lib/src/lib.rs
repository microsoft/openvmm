// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Test IGVM Agent
//!
//! This module contains a test version of the IGVM agent for handling
//! attestation requests in VMM tests.

//! NOTE: This is a test implementation and should not be used in production.

use base64::Engine;
use crypto::rsa::RsaKeyPair;
use crypto::rsa::RsaPublicKey;
use get_resources::ged::IgvmAttestTestConfig;
use inspect::Inspect;
use openhcl_attestation_protocol::igvm_attest::get::AK_CERT_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_SCHEMA_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestKeyReleaseResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseEnvelope;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseExtensions;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestWrappedKeyResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
use openhcl_attestation_protocol::igvm_attest::get::KEY_RELEASE_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::WRAPPED_KEY_RESPONSE_BUFFER_SIZE;
use std::collections::HashMap;
use std::collections::VecDeque;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Default mock context hash: canonical lowercase hex of bytes 0 through 31.
/// This is test metadata, not an authenticated policy digest.
pub const DEFAULT_KEY_RELEASE_CONTEXT_HASH: &str =
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported igvm attest request type: {0:?}")]
    UnsupportedIgvmAttestRequestType(u32),
    #[error("failed to initialize keys for attestation")]
    KeyInitializationFailed(#[source] crypto::rsa::RsaError),
    #[error("failed to generate random bytes")]
    GetRandomFailed(#[source] getrandom::Error),
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
    #[error("failed to serialize mock response envelope")]
    ResponseEnvelopeSerialize(#[source] serde_json::Error),
    #[error("mock response payload is not UTF-8")]
    ResponsePayloadUtf8(#[source] std::string::FromUtf8Error),
    #[error("mock response size {size} exceeds response buffer size {maximum_size}")]
    ResponseTooLarge { size: usize, maximum_size: usize },
}

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum WrappedKeyError {
    #[error("RSA encryption error")]
    RsaEncryptionError(#[source] crypto::rsa::RsaError),
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
    ConvertJwkRsaFailed(#[source] crypto::rsa::RsaError),
    #[error("Secret key not initialized")]
    SecretKeyNotInitialized,
    #[error("failed to convert RSA key to PKCS8 format")]
    RsaToPkcs8Error(#[source] crypto::rsa::RsaError),
    #[error("RSA encryption error")]
    RsaEncryptionError(#[source] crypto::rsa::RsaError),
    #[error("JSON serialization error")]
    JsonSerializeError(#[source] serde_json::Error),
    #[error("failed to generate random bytes")]
    GetRandomFailed(#[source] getrandom::Error),
    #[error("AES key wrap error")]
    AesKeyWrap(#[source] crypto::aes_kwp::AesKeyWrapError),
}

/// Test IGVM agent includes states that need to be persisted.
pub struct TestIgvmAgent {
    /// VM name for log correlation.
    vm_name: String,
    /// Optional RSA private key used for attestation.
    secret_key: Option<RsaKeyPair>,
    /// Optional DES key
    des_key: Option<[u8; 32]>,
    /// Optional scripted actions per request type for tests.
    plan: Option<IgvmAgentTestPlan>,
    /// Track whether the plan has been installed to prevent multiple installations.
    plan_installed: bool,
    /// Latest structurally valid request of each type, including no-response actions.
    last_requests: HashMap<IgvmAttestRequestType, IgvmAgentRecordedRequest>,
}

/// Request state retained for assertions by tests with access to the agent.
#[derive(Debug, Clone)]
pub struct IgvmAgentRecordedRequest {
    /// Request data version, independent of the outer attestation header version.
    pub version: IgvmAttestRequestVersion,
    /// Exact runtime-claims JSON bytes, retained without parsing or reserialization.
    pub runtime_claims: Vec<u8>,
}

/// Possible actions for the IGVM agent to take in response to a request.
#[derive(Debug, Clone)]
pub enum IgvmAgentAction {
    /// Emit a successful response matching the request version. V3 key-release
    /// responses include [`DEFAULT_KEY_RELEASE_CONTEXT_HASH`]; wrapped-key
    /// extensions are empty and AK responses never use V3.
    RespondSuccess,
    /// Emit an unenveloped V2 success, including for V3 key requests.
    /// V1 requests still receive V1 responses.
    RespondSuccessV2,
    /// Emit a V3 success for a V3 key request with the supplied metadata.
    /// Legacy requests and AK certificates retain their request-aware V1/V2 format.
    RespondSuccessV3 {
        /// Used only for key-release responses. `None` omits the hash. Strings
        /// are sent verbatim (including malformed hashes) to test validation.
        key_release_context_hash: Option<String>,
    },
    /// Emit a response that indicates a protocol error.
    RespondFailure,
    /// Emit a response that indicates a protocol error with skip_hw_unsealing signal.
    RespondFailureSkipHwUnsealing,
    /// Skip responding to simulate a timeout (consumed once).
    NoResponse,
    /// Skip responding for this and all subsequent requests of the same type.
    /// Unlike [`NoResponse`](Self::NoResponse), this action is never consumed
    /// from the queue.
    AlwaysNoResponse,
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
        IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended => {
            // Hyper-V VMs go through an `initial_reboot` and may generate
            // multiple background AK_CERT_REQUEST calls during the initial
            // boot and the reboot.  Six failures ensure the SUCCESS action
            // is never consumed during boot, so it remains available for
            // the guest test.
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::RespondSuccess,
                ]),
            );
        }
        IgvmAttestTestConfig::AkCertPersistentAcrossBoot => {
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::AlwaysNoResponse,
                ]),
            );
        }
        IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended => {
            // Hyper-V VMs go through an `initial_reboot` that can consume
            // the first success action.  The extra RespondSuccess ensures
            // the cert is still provisioned after the reboot, so the
            // subsequent boot can validate that the cert is served from
            // the persistent cache.
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::AlwaysNoResponse,
                ]),
            );
        }
        IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing => {
            // Hyper-V VMs go through an `initial_reboot`, consuming two
            // KEY_RELEASE requests (one during initial boot, one during
            // reboot) before the test code starts.
            plan.insert(
                IgvmAttestRequestType::KEY_RELEASE_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::RespondFailureSkipHwUnsealing,
                    IgvmAgentAction::AlwaysNoResponse,
                ]),
            );
        }
        IgvmAttestTestConfig::KeyReleaseFailure => {
            // Hyper-V VMs go through an `initial_reboot`, consuming two
            // KEY_RELEASE requests (one during initial boot, one during
            // reboot) before the test code starts.
            plan.insert(
                IgvmAttestRequestType::KEY_RELEASE_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::RespondFailure,
                    IgvmAgentAction::AlwaysNoResponse,
                ]),
            );
        }
        IgvmAttestTestConfig::StateRefresh => {
            // The `state_refresh_request` behavior is driven by the GSP
            // RPC handler (see `test_igvm_agent_rpc_server`), not by the
            // attest plan. Serve AK cert requests across boots so the
            // guest has a valid AK to read and compare across reboots.
            plan.insert(
                IgvmAttestRequestType::AK_CERT_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::RespondSuccess,
                    IgvmAgentAction::AlwaysNoResponse,
                ]),
            );
        }
    }

    plan
}

impl TestIgvmAgent {
    /// Create an instance associated with the given VM name.
    ///
    /// The `vm_name` is included in all tracing output so that log
    /// messages from the library can be correlated with a specific VM.
    pub fn new(vm_name: impl Into<String>) -> Self {
        Self {
            vm_name: vm_name.into(),
            secret_key: None,
            des_key: None,
            plan: None,
            plan_installed: false,
            last_requests: HashMap::new(),
        }
    }

    /// Latest structurally valid request of this type, even if its action failed
    /// or requested no response. Replaced on each request; no unbounded history.
    /// The record is available to in-process tests, not automatically through RPC.
    pub fn last_request(
        &self,
        request_type: IgvmAttestRequestType,
    ) -> Option<&IgvmAgentRecordedRequest> {
        self.last_requests.get(&request_type)
    }

    /// Install a scripted plan used by tests based on the setting.
    /// Can be called multiple times but will only install the plan once per instance.
    pub fn install_plan_from_setting(&mut self, setting: &IgvmAgentTestSetting) {
        // Only install the plan once per agent instance
        if self.plan_installed {
            return;
        }

        tracing::info!(vm_name = %self.vm_name, "install the scripted plan for test IGVM Agent");

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
    ///
    /// [`IgvmAgentAction::AlwaysNoResponse`] is sticky: it is returned but
    /// never removed from the queue, so every subsequent call for the same
    /// request type will keep returning it.
    pub fn take_next_action(
        &mut self,
        request_type: IgvmAttestRequestType,
    ) -> Option<IgvmAgentAction> {
        // Fast path: no plan installed.
        let plan = self.plan.as_mut()?;
        let queue = plan.get_mut(&request_type)?;
        match queue.front()? {
            IgvmAgentAction::AlwaysNoResponse => Some(IgvmAgentAction::AlwaysNoResponse),
            _ => queue.pop_front(),
        }
    }

    /// Frame the complete serialized body; never truncate to fit a response buffer.
    fn frame_response(
        request_type: IgvmAttestRequestType,
        version: IgvmAttestResponseVersion,
        data: &[u8],
        error_info: IgvmErrorInfo,
    ) -> Result<(Vec<u8>, u32), Error> {
        let maximum_size = match request_type {
            IgvmAttestRequestType::AK_CERT_REQUEST => AK_CERT_RESPONSE_BUFFER_SIZE,
            IgvmAttestRequestType::KEY_RELEASE_REQUEST => KEY_RELEASE_RESPONSE_BUFFER_SIZE,
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST => WRAPPED_KEY_RESPONSE_BUFFER_SIZE,
            ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
        };
        let header_size = if version == IgvmAttestResponseVersion::VERSION_1 {
            size_of::<IgvmAttestCommonResponseHeader>()
        } else {
            size_of::<IgvmAttestCommonResponseHeader>() + size_of::<IgvmErrorInfo>()
        };
        let size = header_size
            .checked_add(data.len())
            .ok_or(Error::ResponseTooLarge {
                size: usize::MAX,
                maximum_size,
            })?;
        if size > maximum_size {
            return Err(Error::ResponseTooLarge { size, maximum_size });
        }
        let data_size =
            u32::try_from(size).map_err(|_| Error::ResponseTooLarge { size, maximum_size })?;

        let mut response = if version == IgvmAttestResponseVersion::VERSION_1 {
            IgvmAttestCommonResponseHeader { data_size, version }
                .as_bytes()
                .to_vec()
        } else {
            // V2 and V3 retain the same 32-byte header and all error/signal bits.
            match request_type {
                IgvmAttestRequestType::WRAPPED_KEY_REQUEST => IgvmAttestWrappedKeyResponseHeader {
                    data_size,
                    version,
                    error_info,
                }
                .as_bytes()
                .to_vec(),
                IgvmAttestRequestType::KEY_RELEASE_REQUEST => IgvmAttestKeyReleaseResponseHeader {
                    data_size,
                    version,
                    error_info,
                }
                .as_bytes()
                .to_vec(),
                IgvmAttestRequestType::AK_CERT_REQUEST => IgvmAttestAkCertResponseHeader {
                    data_size,
                    version,
                    error_info,
                }
                .as_bytes()
                .to_vec(),
                ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
            }
        };
        response.extend_from_slice(data);
        Ok((response, data_size))
    }

    /// V1 failures are empty; V2/V3 failures carry only the error-info header.
    fn build_failure_response(
        request_type: IgvmAttestRequestType,
        version: IgvmAttestResponseVersion,
        error_code: u32,
        igvm_signal: IgvmSignal,
    ) -> Result<(Vec<u8>, u32), Error> {
        if version == IgvmAttestResponseVersion::VERSION_1 {
            return Ok((Vec::new(), 0));
        }
        Self::frame_response(
            request_type,
            version,
            &[],
            IgvmErrorInfo {
                error_code,
                http_status_code: 400,
                igvm_signal,
                reserved: [0; 3],
            },
        )
    }

    /// Request handler. Successful output contains the entire response, and its
    /// reported length includes the header and any JSON envelope expansion.
    /// Responses exceeding the protocol's per-request buffer limit return an
    /// error. The transport remains responsible for validating the actual buffer
    /// capacity, which is not included in `request_bytes`.
    pub fn handle_request(&mut self, request_bytes: &[u8]) -> Result<(Vec<u8>, u32), Error> {
        let _span = tracing::info_span!("igvm_agent", vm_name = %self.vm_name).entered();

        let request = IgvmAttestRequestBase::read_from_prefix(request_bytes)
            .map_err(|_| Error::InvalidIgvmAttestRequest)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let request_type = request.header.request_type;
        let expected_version = match request_type {
            IgvmAttestRequestType::AK_CERT_REQUEST => IgvmAttestRequestVersion::VERSION_2,
            IgvmAttestRequestType::KEY_RELEASE_REQUEST
            | IgvmAttestRequestType::WRAPPED_KEY_REQUEST => IGVM_ATTEST_REQUEST_CURRENT_VERSION,
            ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
        };
        let mut response_version = match request.request_data.version {
            IgvmAttestRequestVersion::VERSION_1 => IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestRequestVersion::VERSION_2 => IgvmAttestResponseVersion::VERSION_2,
            IgvmAttestRequestVersion::VERSION_3
                if request_type != IgvmAttestRequestType::AK_CERT_REQUEST =>
            {
                IgvmAttestResponseVersion::VERSION_3
            }
            found => {
                return Err(Error::InvalidIgvmAttestRequestVersion {
                    found,
                    expected: expected_version,
                });
            }
        };

        // V1 has no extension: claims immediately follow the base. V2/V3 use
        // the same capability bitmap and runtime-claims layout.
        let remaining = &request_bytes[size_of::<IgvmAttestRequestBase>()..];
        let (use_rsa_aes_key_wrap_384, remaining) =
            if request.request_data.version == IgvmAttestRequestVersion::VERSION_1 {
                (false, remaining)
            } else {
                let (ext, remaining) = IgvmAttestRequestDataExt::read_from_prefix(remaining)
                    .map_err(|_| Error::InvalidIgvmAttestRequest)?;
                (ext.capability_bitmap.use_rsa_aes_key_wrap_384(), remaining)
            };
        let runtime_claims_bytes = remaining
            .get(..request.request_data.variable_data_size as usize)
            .ok_or(Error::InvalidIgvmAttestRequest)?;
        self.last_requests.insert(
            request_type,
            IgvmAgentRecordedRequest {
                version: request.request_data.version,
                runtime_claims: runtime_claims_bytes.to_vec(),
            },
        );

        // An absent or exhausted plan falls back to request-aware success.
        let action = self
            .take_next_action(request_type)
            .unwrap_or(IgvmAgentAction::RespondSuccess);
        tracing::info!(?request_type, ?action, "IGVM agent action");
        let key_release_context_hash = match action {
            IgvmAgentAction::NoResponse | IgvmAgentAction::AlwaysNoResponse => {
                return Ok((vec![], 0));
            }
            IgvmAgentAction::RespondFailure => {
                return Self::build_failure_response(
                    request_type,
                    response_version,
                    0x1234,
                    IgvmSignal::default().with_retry(false),
                );
            }
            IgvmAgentAction::RespondFailureSkipHwUnsealing => {
                return Self::build_failure_response(
                    request_type,
                    response_version,
                    0x5678,
                    IgvmSignal::default()
                        .with_retry(false)
                        .with_skip_hw_unsealing(true),
                );
            }
            IgvmAgentAction::RespondSuccess => Some(DEFAULT_KEY_RELEASE_CONTEXT_HASH.to_owned()),
            IgvmAgentAction::RespondSuccessV2 => {
                if response_version == IgvmAttestResponseVersion::VERSION_3 {
                    response_version = IgvmAttestResponseVersion::VERSION_2;
                }
                None
            }
            IgvmAgentAction::RespondSuccessV3 {
                key_release_context_hash,
            } => key_release_context_hash,
        };

        // Keep the existing mock certificate, CPS JSON, JWT and RSA/AES wrapping
        // unchanged. Only V3 key responses wrap that original payload in JSON.
        let mut error_info = IgvmErrorInfo::default();
        let data = match request_type {
            IgvmAttestRequestType::AK_CERT_REQUEST => vec![0xab; 2500],
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST => {
                self.initialize_keys()?;
                self.generate_mock_wrapped_key_response()
                    .map_err(Error::WrappedKeyError)?
            }
            IgvmAttestRequestType::KEY_RELEASE_REQUEST => {
                if self.secret_key.is_none() {
                    self.initialize_keys()?;
                }
                error_info.igvm_signal =
                    IgvmSignal::default().with_rsa_aes_key_wrap_384_used(use_rsa_aes_key_wrap_384);
                self.generate_mock_key_release_response(
                    runtime_claims_bytes,
                    use_rsa_aes_key_wrap_384,
                )
                .map_err(Error::KeyReleaseError)?
                .into_bytes()
            }
            ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
        };
        let data = if response_version == IgvmAttestResponseVersion::VERSION_3 {
            let envelope_request_type = match request_type {
                IgvmAttestRequestType::KEY_RELEASE_REQUEST => {
                    IgvmAttestResponseRequestType::KeyRelease
                }
                IgvmAttestRequestType::WRAPPED_KEY_REQUEST => {
                    IgvmAttestResponseRequestType::WrappedKey
                }
                ty => return Err(Error::UnsupportedIgvmAttestRequestType(ty.0)),
            };
            serde_json::to_vec(&IgvmAttestResponseEnvelope {
                schema_version: IGVM_ATTEST_RESPONSE_SCHEMA_VERSION,
                request_type: envelope_request_type,
                payload: String::from_utf8(data).map_err(Error::ResponsePayloadUtf8)?,
                extensions: IgvmAttestResponseExtensions {
                    key_release_context_hash: if request_type
                        == IgvmAttestRequestType::KEY_RELEASE_REQUEST
                    {
                        key_release_context_hash
                    } else {
                        None
                    },
                },
            })
            .map_err(Error::ResponseEnvelopeSerialize)?
        } else {
            data
        };

        Self::frame_response(request_type, response_version, &data, error_info)
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

        let private_key = RsaKeyPair::generate(2048).map_err(Error::KeyInitializationFailed)?;
        let mut des_key = [0u8; 32];

        self.secret_key = Some(private_key);

        getrandom::fill(&mut des_key).map_err(Error::GetRandomFailed)?;
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
        let encrypted_des = secret_key
            .oaep_encrypt(&des_key, crypto::HashAlgorithm::Sha256)
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
        use_rsa_aes_key_wrap_384: bool,
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
        let rsa_public_key = RsaPublicKey::from_components(&transfer_key.n, &transfer_key.e)
            .map_err(KeyReleaseError::ConvertJwkRsaFailed)?;

        // Generate the JWT response using the extracted RSA key
        self.generate_jwt_with_rsa_key(rsa_public_key, use_rsa_aes_key_wrap_384)
    }

    /// Generate a mock JWT response for testing KEY_RELEASE_REQUEST
    #[expect(deprecated)]
    fn generate_jwt_with_rsa_key(
        &self,
        public_key: RsaPublicKey,
        use_rsa_aes_key_wrap_384: bool,
    ) -> Result<String, KeyReleaseError> {
        use openhcl_attestation_protocol::igvm_attest::akv;

        let secret_key = self
            .secret_key
            .as_ref()
            .ok_or(KeyReleaseError::SecretKeyNotInitialized)?;

        // Generate the KEK (32 bytes) and wrap the private key using internal wrapper
        let mut kek_bytes = [0u8; 32];
        getrandom::fill(&mut kek_bytes).map_err(KeyReleaseError::GetRandomFailed)?;
        let priv_key_der = secret_key
            .to_pkcs8_der()
            .map_err(KeyReleaseError::RsaToPkcs8Error)?;
        let wrapped_key = crypto::aes_kwp::AesKeyWrap::new(&kek_bytes)
            .and_then(|kw| kw.wrapper()?.wrap(priv_key_der.as_bytes()))
            .map_err(KeyReleaseError::AesKeyWrap)?;

        // Encrypt the KEK using RSA-OAEP. Use the SHA-384 variant when the
        // guest requested it (RSA_AES_KEY_WRAP_384), otherwise the default
        // scheme with inner RSA-OAEP using SHA-1.
        let oaep_hash_algorithm = if use_rsa_aes_key_wrap_384 {
            crypto::HashAlgorithm::Sha384
        } else {
            crypto::HashAlgorithm::Sha1
        };
        let encrypted_kek = public_key
            .oaep_encrypt(&kek_bytes, oaep_hash_algorithm)
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

#[cfg(test)]
mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::akv;
    use openhcl_attestation_protocol::igvm_attest::cps;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestHashType;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestReportType;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestData;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmCapabilityBitMap;
    use openhcl_attestation_protocol::igvm_attest::get::encode_key_release_context_hash;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationTpmVersion;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RuntimeClaims;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    const KEY_REQUEST_TYPES: [IgvmAttestRequestType; 2] = [
        IgvmAttestRequestType::KEY_RELEASE_REQUEST,
        IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
    ];
    const VERSIONS: [(IgvmAttestRequestVersion, IgvmAttestResponseVersion); 3] = [
        (
            IgvmAttestRequestVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_1,
        ),
        (
            IgvmAttestRequestVersion::VERSION_2,
            IgvmAttestResponseVersion::VERSION_2,
        ),
        (
            IgvmAttestRequestVersion::VERSION_3,
            IgvmAttestResponseVersion::VERSION_3,
        ),
    ];

    #[test]
    fn default_context_hash_is_canonical_hex() {
        use openhcl_attestation_protocol::igvm_attest::get::decode_key_release_context_hash;

        let hash = std::array::from_fn(|i| i as u8);
        assert_eq!(DEFAULT_KEY_RELEASE_CONTEXT_HASH.len(), 64);
        assert_eq!(
            decode_key_release_context_hash(DEFAULT_KEY_RELEASE_CONTEXT_HASH),
            Ok(hash)
        );
        assert_eq!(
            encode_key_release_context_hash(&hash),
            DEFAULT_KEY_RELEASE_CONTEXT_HASH
        );
    }

    fn vm_config(context_hash: Option<String>) -> AttestationVmConfig {
        AttestationVmConfig {
            current_time: Some(1234),
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console_enabled: false,
            ipmi_enabled: false,
            secure_boot: true,
            tpm_enabled: true,
            tpm_version: AttestationTpmVersion::V185,
            tpm_persisted: true,
            filtered_vpci_devices_allowed: false,
            vm_unique_id: "mock-vm".to_owned(),
            vmgs_provisioner: None,
            hardware_sealing_policy: HardwareSealingPolicy::Hash,
            key_release_context_hash: context_hash,
        }
    }

    // Construct the wire request directly: no attestation helper, hardware report,
    // or underhill_attestation dependency is needed by the mock agent.
    fn request_bytes(
        request_type: IgvmAttestRequestType,
        version: IgvmAttestRequestVersion,
        claims: &[u8],
        use_sha384: bool,
    ) -> Vec<u8> {
        let extension_size = if version == IgvmAttestRequestVersion::VERSION_1 {
            0
        } else {
            size_of::<IgvmAttestRequestDataExt>()
        };
        let size = size_of::<IgvmAttestRequestBase>() + extension_size + claims.len();
        let mut base = IgvmAttestRequestBase::new_zeroed();
        base.header = IgvmAttestRequestHeader::new(size.try_into().unwrap(), request_type, 0);
        base.request_data = IgvmAttestRequestData::new(
            version,
            (size_of::<IgvmAttestRequestData>() + extension_size + claims.len())
                .try_into()
                .unwrap(),
            IgvmAttestReportType::TVM_REPORT,
            IgvmAttestHashType::SHA_256,
            claims.len().try_into().unwrap(),
        );
        let mut bytes = base.as_bytes().to_vec();
        if extension_size != 0 {
            bytes.extend_from_slice(
                IgvmAttestRequestDataExt::new(
                    IgvmCapabilityBitMap::new()
                        .with_error_code(true)
                        .with_use_rsa_aes_key_wrap_384(use_sha384),
                )
                .as_bytes(),
            );
        }
        bytes.extend_from_slice(claims);
        assert_eq!(bytes.len(), size);
        bytes
    }

    fn install_actions(
        agent: &mut TestIgvmAgent,
        request_type: IgvmAttestRequestType,
        actions: impl IntoIterator<Item = IgvmAgentAction>,
    ) {
        agent.install_plan_from_setting(&IgvmAgentTestSetting::TestPlan(HashMap::from([(
            request_type,
            actions.into_iter().collect(),
        )])));
    }

    fn response_parts(
        response: &(Vec<u8>, u32),
        expected_version: IgvmAttestResponseVersion,
        maximum_size: usize,
    ) -> (IgvmErrorInfo, &[u8]) {
        let (bytes, size) = response;
        let (header, rest) = IgvmAttestCommonResponseHeader::read_from_prefix(bytes).unwrap();
        assert_eq!(header.version, expected_version);
        assert_eq!(header.data_size, *size);
        assert_eq!(*size as usize, bytes.len());
        assert!(bytes.len() <= maximum_size);
        if expected_version == IgvmAttestResponseVersion::VERSION_1 {
            assert_eq!(bytes.len() - rest.len(), 8);
            (IgvmErrorInfo::default(), rest)
        } else {
            let (error, body) = IgvmErrorInfo::read_from_prefix(rest).unwrap();
            assert_eq!(bytes.len() - body.len(), 32);
            (error, body)
        }
    }

    fn assert_recorded(
        agent: &TestIgvmAgent,
        request_type: IgvmAttestRequestType,
        version: IgvmAttestRequestVersion,
        claims: &[u8],
    ) {
        let recorded = agent.last_request(request_type).unwrap();
        assert_eq!(recorded.version, version);
        assert_eq!(recorded.runtime_claims, claims);
    }

    #[expect(deprecated)] // Exercise the legacy SHA-1 RSA-AES key-wrap scheme too.
    fn assert_key_payload(
        agent: &TestIgvmAgent,
        transfer_key: &RsaKeyPair,
        payload: &str,
        use_sha384: bool,
    ) {
        // These must be the original compact JWT bytes, not another envelope.
        assert!(payload.starts_with("eyJ"));
        let parts: Vec<_> = payload.split('.').collect();
        assert_eq!(parts.len(), 3);
        let base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: akv::AkvKeyReleaseJwtHeader =
            serde_json::from_slice(&base64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header.alg, "RS256");
        assert!(header.x5c.is_empty());
        assert!(parts[2].is_empty()); // The mock deliberately does not sign JWTs.
        let body: akv::AkvKeyReleaseJwtBody =
            serde_json::from_slice(&base64.decode(parts[1]).unwrap()).unwrap();
        let blob: akv::AkvKeyReleaseKeyBlob =
            serde_json::from_slice(&body.response.key.key.key_hsm).unwrap();
        let original_key = agent.secret_key.as_ref().unwrap().to_pkcs8_der().unwrap();
        let rsa_size = transfer_key.modulus_size();
        assert_eq!(
            blob.ciphertext.len(),
            rsa_size + (original_key.len().div_ceil(8) + 1) * 8
        );
        let (encrypted_kek, wrapped_key) = blob.ciphertext.split_at(rsa_size);
        let kek = transfer_key
            .oaep_decrypt(
                encrypted_kek,
                if use_sha384 {
                    crypto::HashAlgorithm::Sha384
                } else {
                    crypto::HashAlgorithm::Sha1
                },
            )
            .unwrap();
        assert_eq!(kek.len(), 32);
        let unwrapped = crypto::aes_kwp::AesKeyWrap::new(&kek)
            .unwrap()
            .unwrapper()
            .unwrap()
            .unwrap(wrapped_key)
            .unwrap();
        assert_eq!(unwrapped, original_key);
    }

    fn assert_wrapped_payload(agent: &TestIgvmAgent, payload: &str) {
        // V2 must start with CPS JSON, not the V3 schema/payload wrapper.
        assert!(payload.starts_with("{\"DiskEncryptionSettings\":"));
        let blob: cps::VmmdBlob = serde_json::from_str(payload).unwrap();
        let info = blob.disk_encryption_settings.encryption_info;
        assert_eq!(
            info.key_reference,
            serde_json::json!({
                "key_info": { "host": "name" },
                "attestation_info": { "host": "attestation_name" }
            })
        );
        let secret = agent.secret_key.as_ref().unwrap();
        assert_eq!(info.aes_info.ciphertext.len(), secret.modulus_size());
        assert_eq!(
            secret
                .oaep_decrypt(&info.aes_info.ciphertext, crypto::HashAlgorithm::Sha256)
                .unwrap(),
            agent.des_key.unwrap()
        );
    }

    fn exercise_key_responses(request_type: IgvmAttestRequestType, use_sha384: bool) {
        let transfer_key = RsaKeyPair::generate(2048).unwrap();
        let components = transfer_key.to_components();
        let context_hash = encode_key_release_context_hash(&[0x42; 32]);
        let config = vm_config(Some(context_hash.clone()));
        let claims = RuntimeClaims::key_release_request_runtime_claims(
            &components.public_exponent,
            &components.modulus,
            &config,
        );
        // Whitespace makes exact-byte recording distinguishable from reserialization.
        let claims = serde_json::to_vec_pretty(&claims).unwrap();
        for (request_version, default_response_version) in VERSIONS {
            let mut agent = TestIgvmAgent::new("key-response-versions");
            let request = request_bytes(request_type, request_version, &claims, use_sha384);
            let actions = [
                IgvmAgentAction::RespondSuccess,
                IgvmAgentAction::RespondSuccessV2,
                IgvmAgentAction::RespondSuccessV3 {
                    key_release_context_hash: Some(context_hash.clone()),
                },
                IgvmAgentAction::RespondSuccessV3 {
                    key_release_context_hash: None,
                },
                IgvmAgentAction::RespondSuccessV3 {
                    key_release_context_hash: Some("not a digest\"\n".to_owned()),
                },
            ];
            // First exercise an absent plan; the final request exhausts the plan.
            let mut responses = vec![(
                agent.handle_request(&request).unwrap(),
                default_response_version,
                Some(DEFAULT_KEY_RELEASE_CONTEXT_HASH.to_owned()),
            )];
            install_actions(&mut agent, request_type, actions.clone());
            for action in actions {
                let (version, hash) = match action {
                    IgvmAgentAction::RespondSuccess => (
                        default_response_version,
                        Some(DEFAULT_KEY_RELEASE_CONTEXT_HASH.to_owned()),
                    ),
                    IgvmAgentAction::RespondSuccessV2 => (
                        if request_version == IgvmAttestRequestVersion::VERSION_1 {
                            IgvmAttestResponseVersion::VERSION_1
                        } else {
                            IgvmAttestResponseVersion::VERSION_2
                        },
                        None,
                    ),
                    IgvmAgentAction::RespondSuccessV3 {
                        key_release_context_hash,
                    } => (default_response_version, key_release_context_hash),
                    _ => unreachable!(),
                };
                responses.push((agent.handle_request(&request).unwrap(), version, hash));
            }
            responses.push((
                agent.handle_request(&request).unwrap(),
                default_response_version,
                Some(DEFAULT_KEY_RELEASE_CONTEXT_HASH.to_owned()),
            ));
            for (response, version, hash) in responses {
                let hash = if request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST {
                    hash
                } else {
                    None
                };
                let maximum_size = if request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST {
                    KEY_RELEASE_RESPONSE_BUFFER_SIZE
                } else {
                    WRAPPED_KEY_RESPONSE_BUFFER_SIZE
                };
                let (error, body) = response_parts(&response, version, maximum_size);
                let uses_sha384 = use_sha384
                    && request_version != IgvmAttestRequestVersion::VERSION_1
                    && request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST;
                let expected_error = IgvmErrorInfo {
                    igvm_signal: IgvmSignal::default().with_rsa_aes_key_wrap_384_used(uses_sha384),
                    ..Default::default()
                };
                assert_eq!(error.as_bytes(), expected_error.as_bytes());
                let payload = if version == IgvmAttestResponseVersion::VERSION_3 {
                    let envelope: IgvmAttestResponseEnvelope =
                        serde_json::from_slice(body).unwrap();
                    assert_eq!(envelope.schema_version, IGVM_ATTEST_RESPONSE_SCHEMA_VERSION);
                    assert_eq!(
                        envelope.request_type,
                        if request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST {
                            IgvmAttestResponseRequestType::KeyRelease
                        } else {
                            IgvmAttestResponseRequestType::WrappedKey
                        }
                    );
                    assert_eq!(envelope.extensions.key_release_context_hash, hash);
                    let json: serde_json::Value = serde_json::from_slice(body).unwrap();
                    if request_type == IgvmAttestRequestType::WRAPPED_KEY_REQUEST {
                        assert_eq!(json["extensions"], serde_json::json!({}));
                    }
                    assert_eq!(
                        json["extensions"].get("key_release_context_hash").is_some(),
                        hash.is_some()
                    );
                    assert_eq!(serde_json::to_vec(&envelope).unwrap(), body);
                    assert!(body.len() > envelope.payload.len());
                    envelope.payload
                } else {
                    assert!(serde_json::from_slice::<IgvmAttestResponseEnvelope>(body).is_err());
                    std::str::from_utf8(body).unwrap().to_owned()
                };
                if request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST {
                    assert_key_payload(&agent, &transfer_key, &payload, uses_sha384);
                } else {
                    assert_wrapped_payload(&agent, &payload);
                }
            }
            assert_recorded(&agent, request_type, request_version, &claims);
            let recorded: serde_json::Value =
                serde_json::from_slice(&agent.last_request(request_type).unwrap().runtime_claims)
                    .unwrap();
            assert_eq!(
                recorded["vm-configuration"],
                serde_json::to_value(&config).unwrap()
            );
            assert_eq!(
                recorded["vm-configuration"]["key-release-context-hash"],
                context_hash
            );
        }
    }

    #[test]
    fn key_release_versions_actions_claims_and_real_jwt() {
        for use_sha384 in [false, true] {
            exercise_key_responses(IgvmAttestRequestType::KEY_RELEASE_REQUEST, use_sha384);
        }
    }

    #[test]
    fn wrapped_key_versions_actions_claims_and_real_cps_payload() {
        exercise_key_responses(IgvmAttestRequestType::WRAPPED_KEY_REQUEST, false);
    }

    #[test]
    fn ak_cert_stays_v2_even_with_v3_action() {
        let request_type = IgvmAttestRequestType::AK_CERT_REQUEST;
        let config = vm_config(Some(encode_key_release_context_hash(&[0x43; 32])));
        let claims = serde_json::to_vec(&RuntimeClaims::ak_cert_runtime_claims(
            &[1, 0, 1],
            &[0x81; 256],
            &[1, 0, 1],
            &[0x82; 256],
            &config,
            b"user data",
        ))
        .unwrap();
        for (request_version, response_version) in VERSIONS.into_iter().take(2) {
            let mut agent = TestIgvmAgent::new("ak-cert-versions");
            let request = request_bytes(request_type, request_version, &claims, false);
            let actions = [
                IgvmAgentAction::RespondSuccess,
                IgvmAgentAction::RespondSuccessV2,
                IgvmAgentAction::RespondSuccessV3 {
                    key_release_context_hash: config.key_release_context_hash.clone(),
                },
            ];
            let mut responses = vec![agent.handle_request(&request).unwrap()];
            install_actions(&mut agent, request_type, actions.clone());
            for _ in actions {
                responses.push(agent.handle_request(&request).unwrap());
            }
            for response in responses {
                let (error, body) =
                    response_parts(&response, response_version, AK_CERT_RESPONSE_BUFFER_SIZE);
                assert_eq!(error.as_bytes(), IgvmErrorInfo::default().as_bytes());
                assert_eq!(body, vec![0xab; 2500]);
            }
            assert_recorded(&agent, request_type, request_version, &claims);
        }
        let mut agent = TestIgvmAgent::new("reject-ak-v3");
        let request = request_bytes(
            request_type,
            IgvmAttestRequestVersion::VERSION_3,
            &claims,
            false,
        );
        assert!(matches!(
            agent.handle_request(&request),
            Err(Error::InvalidIgvmAttestRequestVersion {
                found: IgvmAttestRequestVersion::VERSION_3,
                expected: IgvmAttestRequestVersion::VERSION_2,
            })
        ));
        assert!(agent.last_request(request_type).is_none());
    }

    #[test]
    fn failures_preserve_error_codes_and_signals_without_envelopes() {
        for request_type in KEY_REQUEST_TYPES
            .into_iter()
            .chain([IgvmAttestRequestType::AK_CERT_REQUEST])
        {
            for (request_version, response_version) in VERSIONS {
                if request_type == IgvmAttestRequestType::AK_CERT_REQUEST
                    && request_version == IgvmAttestRequestVersion::VERSION_3
                {
                    continue;
                }
                let mut agent = TestIgvmAgent::new("failure-signals");
                install_actions(
                    &mut agent,
                    request_type,
                    [
                        IgvmAgentAction::RespondFailure,
                        IgvmAgentAction::RespondFailureSkipHwUnsealing,
                    ],
                );
                let request = request_bytes(request_type, request_version, b"{}", true);
                for (code, skip) in [(0x1234, false), (0x5678, true)] {
                    let response = agent.handle_request(&request).unwrap();
                    assert_recorded(&agent, request_type, request_version, b"{}");
                    if response_version == IgvmAttestResponseVersion::VERSION_1 {
                        // No header or signal bits can be carried by a V1 failure.
                        assert_eq!(response, (Vec::new(), 0));
                        continue;
                    }
                    let (error, body) =
                        response_parts(&response, response_version, AK_CERT_RESPONSE_BUFFER_SIZE);
                    assert!(body.is_empty());
                    let expected = IgvmErrorInfo {
                        error_code: code,
                        http_status_code: 400,
                        igvm_signal: IgvmSignal::default().with_skip_hw_unsealing(skip),
                        reserved: [0; 3],
                    };
                    assert_eq!(error.as_bytes(), expected.as_bytes());
                }
            }
        }
    }

    #[test]
    fn no_response_records_exact_latest_claims_per_request_type() {
        let mut agent = TestIgvmAgent::new("record-requests");
        agent.install_plan_from_setting(&IgvmAgentTestSetting::TestPlan(
            KEY_REQUEST_TYPES
                .into_iter()
                .map(|request_type| {
                    (
                        request_type,
                        VecDeque::from([
                            IgvmAgentAction::NoResponse,
                            IgvmAgentAction::AlwaysNoResponse,
                        ]),
                    )
                })
                .collect(),
        ));
        for (request_version, _) in VERSIONS {
            for request_type in KEY_REQUEST_TYPES {
                let config = vm_config(Some(encode_key_release_context_hash(
                    &[request_version.0 as u8; 32],
                )));
                let claims = serde_json::to_vec_pretty(&RuntimeClaims {
                    keys: vec![],
                    vm_configuration: config,
                    user_data: format!("request {}", request_type.0),
                })
                .unwrap();
                let mut request = request_bytes(request_type, request_version, &claims, false);
                // The declared claims size, not the transport buffer tail, is recorded.
                request.extend_from_slice(b"ignored trailing bytes");
                assert_eq!(agent.handle_request(&request).unwrap(), (vec![], 0));
                assert_recorded(&agent, request_type, request_version, &claims);
                let truncated = request_bytes(request_type, request_version, &claims, false);
                assert!(matches!(
                    agent.handle_request(&truncated[..truncated.len() - 1]),
                    Err(Error::InvalidIgvmAttestRequest)
                ));
                assert_recorded(&agent, request_type, request_version, &claims);
            }
        }
        for request_type in KEY_REQUEST_TYPES {
            assert!(matches!(
                agent.take_next_action(request_type),
                Some(IgvmAgentAction::AlwaysNoResponse)
            ));
            let recorded: RuntimeClaims =
                serde_json::from_slice(&agent.last_request(request_type).unwrap().runtime_claims)
                    .unwrap();
            assert_eq!(recorded.user_data, format!("request {}", request_type.0));
            assert_eq!(
                recorded.vm_configuration.key_release_context_hash,
                Some(encode_key_release_context_hash(&[3; 32]))
            );
        }
    }

    #[test]
    fn framing_accepts_exact_limit_rejects_overflow_and_preserves_error_info() {
        for (request_type, maximum_size) in [
            (
                IgvmAttestRequestType::AK_CERT_REQUEST,
                AK_CERT_RESPONSE_BUFFER_SIZE,
            ),
            (
                IgvmAttestRequestType::KEY_RELEASE_REQUEST,
                KEY_RELEASE_RESPONSE_BUFFER_SIZE,
            ),
            (
                IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
                WRAPPED_KEY_RESPONSE_BUFFER_SIZE,
            ),
        ] {
            for (_, version) in VERSIONS {
                if request_type == IgvmAttestRequestType::AK_CERT_REQUEST
                    && version == IgvmAttestResponseVersion::VERSION_3
                {
                    continue;
                }
                let header_size = if version == IgvmAttestResponseVersion::VERSION_1 {
                    8
                } else {
                    32
                };
                let error = IgvmErrorInfo {
                    error_code: 0x12345678,
                    http_status_code: 503,
                    igvm_signal: IgvmSignal::from(0xa5a5ffff),
                    reserved: [11, 22, 33],
                };
                let expected_error = error.as_bytes().to_vec();
                let body = vec![0xa5; maximum_size - header_size];
                let response =
                    TestIgvmAgent::frame_response(request_type, version, &body, error).unwrap();
                let (actual_error, actual_body) = response_parts(&response, version, maximum_size);
                assert_eq!(response.1 as usize, maximum_size);
                assert_eq!(actual_body, body);
                if version != IgvmAttestResponseVersion::VERSION_1 {
                    assert_eq!(actual_error.as_bytes(), expected_error);
                }
                assert!(matches!(
                    TestIgvmAgent::frame_response(request_type, version, &vec![0; body.len() + 1], IgvmErrorInfo::default()),
                    Err(Error::ResponseTooLarge { size, maximum_size: limit })
                        if size == maximum_size + 1 && limit == maximum_size
                ));
            }
        }
    }

    #[test]
    fn oversized_v3_envelope_is_rejected_not_truncated() {
        let transfer_key = RsaKeyPair::generate(2048).unwrap().to_components();
        let claims = serde_json::to_vec(&RuntimeClaims::key_release_request_runtime_claims(
            &transfer_key.public_exponent,
            &transfer_key.modulus,
            &vm_config(None),
        ))
        .unwrap();
        for request_type in KEY_REQUEST_TYPES {
            let maximum_size = if request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST {
                KEY_RELEASE_RESPONSE_BUFFER_SIZE
            } else {
                WRAPPED_KEY_RESPONSE_BUFFER_SIZE
            };
            let mut agent = TestIgvmAgent::new("oversized-envelope");
            install_actions(
                &mut agent,
                request_type,
                [IgvmAgentAction::RespondSuccessV3 {
                    // Escaping expands this string to more than the entire response buffer.
                    key_release_context_hash: Some("\"".repeat(maximum_size / 2)),
                }],
            );
            let request = request_bytes(
                request_type,
                IgvmAttestRequestVersion::VERSION_3,
                &claims,
                false,
            );
            let result = agent.handle_request(&request);
            if request_type == IgvmAttestRequestType::KEY_RELEASE_REQUEST {
                assert!(matches!(
                    result,
                    Err(Error::ResponseTooLarge { size, maximum_size: limit })
                        if size > maximum_size && limit == maximum_size
                ));
            } else {
                // Even oversized scripted context metadata is irrelevant to
                // wrapped-key responses and must not be emitted.
                let response = result.unwrap();
                let (_, body) = response_parts(
                    &response,
                    IgvmAttestResponseVersion::VERSION_3,
                    maximum_size,
                );
                let envelope: IgvmAttestResponseEnvelope = serde_json::from_slice(body).unwrap();
                assert!(envelope.extensions.key_release_context_hash.is_none());
            }
            assert_recorded(
                &agent,
                request_type,
                IgvmAttestRequestVersion::VERSION_3,
                &claims,
            );
        }
    }
}
