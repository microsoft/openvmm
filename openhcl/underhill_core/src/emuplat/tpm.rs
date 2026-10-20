// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::EventLogId;
use openhcl_attestation_protocol::igvm_attest::get::AK_CERT_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use std::sync::Arc;
use thiserror::Error;
use tpm_resources::AkCertRequest;
use tpm_resources::AkCertResponse;
use tpm_resources::RequestAkCert;
use tpm_resources::TpmLogEvent;
use tpm_resources::TpmLogger;
use underhill_attestation::AttestationType;

#[derive(Debug, Error)]
pub enum TpmAttestationError {
    #[error("failed to get a hardware attestation report")]
    GetAttestationReport(#[source] tee_call::Error),
    #[error("failed to create the IgvmAttest AK_CERT request")]
    CreateAkCertRequest(#[source] underhill_attestation::IgvmAttestError),
    #[error("AK_CERT request size {size} exceeds the GET limit {max_size}")]
    RequestTooLarge { size: usize, max_size: usize },
}

fn validate_request_size(
    size: usize,
    is_attestation_report: bool,
) -> Result<(), TpmAttestationError> {
    let max_size = get_protocol::IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE;
    if !is_attestation_report && size > max_size {
        return Err(TpmAttestationError::RequestTooLarge { size, max_size });
    }
    Ok(())
}

/// An implementation of [`RequestAkCert`].
pub struct TpmRequestAkCertHelper {
    get_client: GuestEmulationTransportClient,
    tee_call: Option<Arc<dyn tee_call::TeeCall>>,
    attestation_type: AttestationType,
    attestation_vm_config: AttestationVmConfig,
    attestation_agent_data: Option<Vec<u8>>,
}

impl TpmRequestAkCertHelper {
    pub fn new(
        get_client: GuestEmulationTransportClient,
        tee_call: Option<Arc<dyn tee_call::TeeCall>>,
        attestation_type: AttestationType,
        attestation_vm_config: AttestationVmConfig,
        attestation_agent_data: Option<Vec<u8>>,
    ) -> Self {
        Self {
            get_client,
            tee_call,
            attestation_type,
            attestation_vm_config,
            attestation_agent_data,
        }
    }
}

#[async_trait::async_trait]
impl RequestAkCert for TpmRequestAkCertHelper {
    fn create_ak_cert_request(
        &self,
        ak_pub_modulus: &[u8],
        ak_pub_exponent: &[u8],
        ek_pub_modulus: &[u8],
        ek_pub_exponent: &[u8],
        guest_input: &[u8],
        is_attestation_report: bool,
        existing_cert: Option<&[u8]>,
    ) -> Result<AkCertRequest, Box<dyn std::error::Error + Send + Sync>> {
        let metadata = if !is_attestation_report {
            existing_cert.and_then(|cert| {
                match underhill_attestation::parse_ak_cert_metadata(cert) {
                    Ok(metadata) => Some(metadata),
                    Err(error) => {
                        tracelimit::warn_ratelimited!(
                            error = &error as &dyn std::error::Error,
                            "Failed to parse existing AK certificate; requesting a replacement"
                        );
                        None
                    }
                }
            })
        } else {
            None
        };
        let had_metadata = metadata.is_some();
        let tee_type = match self.attestation_type {
            AttestationType::Snp => Some(tee_call::TeeType::Snp),
            AttestationType::Tdx => Some(tee_call::TeeType::Tdx),
            AttestationType::Cca => Some(tee_call::TeeType::Cca),
            AttestationType::Vbs => Some(tee_call::TeeType::Vbs),
            AttestationType::Host => None,
        };
        let ak_cert_request_helper =
            underhill_attestation::IgvmAttestRequestHelper::prepare_ak_cert_request(
                tee_type,
                ak_pub_exponent,
                ak_pub_modulus,
                ek_pub_exponent,
                ek_pub_modulus,
                &self.attestation_vm_config,
                guest_input,
                metadata,
            );

        let version = if is_attestation_report {
            // If this is an attestation report, use the version 1, the stable structure exposed
            // to the guest via NV index.
            IgvmAttestRequestVersion::VERSION_1
        } else {
            // Otherwise, this is an AK cert request. Use the latest version to get the latest features.
            IGVM_ATTEST_REQUEST_CURRENT_VERSION
        };

        validate_request_size(
            ak_cert_request_helper.request_size(version),
            is_attestation_report,
        )?;
        if had_metadata && !ak_cert_request_helper.has_ak_cert_metadata() {
            tracelimit::warn_ratelimited!(
                "AK certificate metadata exceeds the GET request limit; requesting a replacement"
            );
        }

        let attestation_report = if let Some(tee_call) = &self.tee_call {
            tee_call
                .get_attestation_report(ak_cert_request_helper.get_runtime_claims_hash())
                .map_err(TpmAttestationError::GetAttestationReport)?
                .report
        } else {
            vec![]
        };

        let request = ak_cert_request_helper
            .create_request(version, &attestation_report)
            .map_err(TpmAttestationError::CreateAkCertRequest)?;

        Ok(AkCertRequest {
            data: request,
            has_cert: ak_cert_request_helper.has_ak_cert_metadata(),
        })
    }

    async fn request_ak_cert(
        &self,
        request: Vec<u8>,
    ) -> Result<AkCertResponse, Box<dyn std::error::Error + Send + Sync + 'static>> {
        let agent_data = self.attestation_agent_data.clone().unwrap_or_default();
        let result = self
            .get_client
            .igvm_attest(agent_data, request, AK_CERT_RESPONSE_BUFFER_SIZE)
            .await?;
        let payload = if !result.response.is_empty() {
            match underhill_attestation::parse_ak_cert_response(&result.response)? {
                underhill_attestation::AkCertResponse::Certificate(cert) => {
                    AkCertResponse::Certificate(cert)
                }
                underhill_attestation::AkCertResponse::NoRenewal => AkCertResponse::NoRenewal,
            }
        } else {
            // Let the caller to handle the empty response.
            AkCertResponse::Certificate(vec![])
        };

        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn ak_cert_request_size_limit() {
        let max_size = get_protocol::IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE;
        assert!(validate_request_size(max_size, false).is_ok());
        assert!(matches!(
            validate_request_size(max_size + 1, false),
            Err(TpmAttestationError::RequestTooLarge { size, max_size: limit })
                if size == max_size + 1 && limit == max_size
        ));
        // Guest NV reports do not travel over GET and retain their V1 behavior.
        assert!(validate_request_size(max_size + 1, true).is_ok());
    }
}

/// An implementation of [`TpmLogger`].
pub struct GetTpmLogger {
    get_client: GuestEmulationTransportClient,
}

impl GetTpmLogger {
    pub fn new(get_client: GuestEmulationTransportClient) -> Self {
        Self { get_client }
    }
}

fn convert_to_get_event_id(event: TpmLogEvent) -> EventLogId {
    match event {
        TpmLogEvent::AkCertRenewalFailed => EventLogId::CERTIFICATE_RENEWAL_FAILED,
        TpmLogEvent::IdentityChangeFailed => EventLogId::TPM_IDENTITY_CHANGE_FAILED,
        TpmLogEvent::InvalidState => EventLogId::TPM_INVALID_STATE,
    }
}

#[async_trait::async_trait]
impl TpmLogger for GetTpmLogger {
    async fn log_event_and_flush(&self, event: TpmLogEvent) {
        self.get_client
            .event_log_fatal(convert_to_get_event_id(event))
            .await
    }

    fn log_event(&self, event: TpmLogEvent) {
        self.get_client.event_log(convert_to_get_event_id(event));
    }
}

pub mod resources {
    use super::GetTpmLogger;
    use super::TpmRequestAkCertHelper;
    use async_trait::async_trait;
    use guest_emulation_transport::resolver::GetClientKind;
    use mesh::MeshPayload;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
    use std::sync::Arc;
    use tpm_resources::RequestAkCertKind;
    use tpm_resources::ResolvedRequestAkCert;
    use tpm_resources::ResolvedTpmLogger;
    use tpm_resources::TpmLoggerKind;
    use underhill_attestation::AttestationType;
    use vm_resource::AsyncResolveResource;
    use vm_resource::IntoResource;
    use vm_resource::PlatformResource;
    use vm_resource::ResolveError;
    use vm_resource::ResourceId;
    use vm_resource::ResourceResolver;
    use vm_resource::declare_static_async_resolver;

    #[derive(MeshPayload)]
    pub struct GetTpmRequestAkCertHelperHandle {
        attestation_type: AttestationType,
        attestation_vm_config: AttestationVmConfig,
        attestation_agent_data: Option<Vec<u8>>,
    }

    impl GetTpmRequestAkCertHelperHandle {
        pub fn new(
            attestation_type: AttestationType,
            attestation_vm_config: AttestationVmConfig,
            attestation_agent_data: Option<Vec<u8>>,
        ) -> Self {
            Self {
                attestation_type,
                attestation_vm_config,
                attestation_agent_data,
            }
        }
    }

    impl ResourceId<RequestAkCertKind> for GetTpmRequestAkCertHelperHandle {
        const ID: &'static str = "request_ak_cert";
    }

    pub struct GetTpmRequestAkCertHelperResolver;

    declare_static_async_resolver! {
        GetTpmRequestAkCertHelperResolver,
        (RequestAkCertKind, GetTpmRequestAkCertHelperHandle)
    }

    #[async_trait]
    impl AsyncResolveResource<RequestAkCertKind, GetTpmRequestAkCertHelperHandle>
        for GetTpmRequestAkCertHelperResolver
    {
        type Output = ResolvedRequestAkCert;
        type Error = ResolveError;

        async fn resolve(
            &self,
            resolver: &ResourceResolver,
            handle: GetTpmRequestAkCertHelperHandle,
            _: &(),
        ) -> Result<Self::Output, Self::Error> {
            let get = resolver
                .resolve::<GetClientKind, _>(PlatformResource.into_resource(), ())
                .await?;

            let tee_call: Option<Arc<dyn tee_call::TeeCall>> = match handle.attestation_type {
                AttestationType::Snp => Some(Arc::new(tee_call::SnpCall)),
                // The AK cert request path only issues `get_attestation_report`
                // and never derives hardware keys, so hardware-bound seal key
                // enablement is irrelevant here.
                AttestationType::Tdx => Some(Arc::new(tee_call::TdxCall::new(false))),
                AttestationType::Vbs => Some(Arc::new(tee_call::VbsCall)),
                AttestationType::Cca => {
                    tracing::warn!("CCA: resolve: tee_call is not implemented yet");
                    None
                }
                AttestationType::Host => None,
            };

            Ok(TpmRequestAkCertHelper::new(
                get,
                tee_call,
                handle.attestation_type,
                handle.attestation_vm_config,
                handle.attestation_agent_data,
            )
            .into())
        }
    }

    #[derive(MeshPayload)]
    pub struct GetTpmLoggerHandle;

    impl ResourceId<TpmLoggerKind> for GetTpmLoggerHandle {
        const ID: &'static str = "tpm_logger";
    }

    pub struct GetTpmLoggerResolver;

    declare_static_async_resolver! {
        GetTpmLoggerResolver,
        (TpmLoggerKind, GetTpmLoggerHandle)
    }

    #[async_trait]
    impl AsyncResolveResource<TpmLoggerKind, GetTpmLoggerHandle> for GetTpmLoggerResolver {
        type Output = ResolvedTpmLogger;
        type Error = ResolveError;

        async fn resolve(
            &self,
            resolver: &ResourceResolver,
            GetTpmLoggerHandle: GetTpmLoggerHandle,
            _: &(),
        ) -> Result<Self::Output, Self::Error> {
            let get = resolver
                .resolve::<GetClientKind, _>(PlatformResource.into_resource(), ())
                .await?;

            Ok(GetTpmLogger::new(get).into())
        }
    }
}
