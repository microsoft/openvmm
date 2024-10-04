// Copyright (C) Microsoft Corporation. All rights reserved.

use guest_emulation_transport::GuestEmulationTransportClient;
use thiserror::Error;
use tpm::ak_cert::GetAttestationReport;
use tpm::ak_cert::RequestAkCert;
use underhill_attestation::AttestationVmConfig;

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum TpmGetAttestationReportError {
    #[error("failed to get a hardware attestation report")]
    GetAttestationReport(#[source] tee_call::Error),
    #[error("failed to create IgvmAttest AK_CERT request")]
    CreateIgvmAttestAkCertRequest(#[source] underhill_attestation::IgvmAttestError),
}

/// An implementation of [`GetAttestationReport`].
pub struct TpmGetAttestationReportHelper {
    tee_call: Box<dyn tee_call::TeeCall>,
    attestation_vm_config: AttestationVmConfig,
}

impl TpmGetAttestationReportHelper {
    pub fn new(
        tee_call: Box<dyn tee_call::TeeCall>,
        attestation_vm_config: AttestationVmConfig,
    ) -> Self {
        Self {
            tee_call,
            attestation_vm_config,
        }
    }
}

impl GetAttestationReport for TpmGetAttestationReportHelper {
    fn get_attestation_report(
        &self,
        ak_pub_modulus: &[u8],
        ak_pub_exponent: &[u8],
        ek_pub_modulus: &[u8],
        ek_pub_exponent: &[u8],
        guest_input: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let igvm_attest_request_helper =
            underhill_attestation::IgvmAttestRequestHelper::prepare_ak_cert_request(
                self.tee_call.tee_type(),
                ak_pub_exponent,
                ak_pub_modulus,
                ek_pub_exponent,
                ek_pub_modulus,
                &self.attestation_vm_config,
                guest_input,
            );

        let result = self
            .tee_call
            .get_attestation_report(&igvm_attest_request_helper.runtime_claims_hash)
            .map_err(TpmGetAttestationReportError::GetAttestationReport)?;

        let request = igvm_attest_request_helper
            .create_request(&result.report)
            .map_err(TpmGetAttestationReportError::CreateIgvmAttestAkCertRequest)?;

        // Treat the request as the attestation report which will be exposed to the guest
        // (via nv index).
        Ok(request)
    }
}

/// An implementation of [`RequestAkCert`].
#[derive(Clone)]
pub struct TpmRequestAkCertHelper {
    get_client: GuestEmulationTransportClient,
    attestation_agent_data: Option<Vec<u8>>,
}

impl TpmRequestAkCertHelper {
    pub fn new(
        get_client: GuestEmulationTransportClient,
        attestation_agent_data: Option<Vec<u8>>,
    ) -> Self {
        Self {
            get_client,
            attestation_agent_data,
        }
    }
}

#[async_trait::async_trait]
impl RequestAkCert for TpmRequestAkCertHelper {
    async fn request_ak_cert(
        &self,
        attestation_report: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync + 'static>> {
        // Attestation report should be `Some` for CVM.
        // TODO TVM: Allow attestation report in `None` when shared memory is supported
        // (required by GET callout).
        let Some(attestation_report) = attestation_report else {
            return Ok(vec![]);
        };

        let agent_data = self.attestation_agent_data.clone().unwrap_or_default();
        let result = self
            .get_client
            .igvm_attest(agent_data, attestation_report)
            .await?;
        let payload = underhill_attestation::parse_ak_cert_response(&result.response)?;

        Ok(payload)
    }

    fn clone_box(&self) -> Box<dyn RequestAkCert> {
        Box::new(self.clone())
    }
}

pub mod resources {
    use super::TpmGetAttestationReportHelper;
    use super::TpmRequestAkCertHelper;
    use async_trait::async_trait;
    use guest_emulation_transport::resolver::GetClientKind;
    use mesh::MeshPayload;
    use tpm::ak_cert::ResolvedGetAttestationReport;
    use tpm::ak_cert::ResolvedRequestAkCert;
    use tpm_resources::GetAttestationReportKind;
    use tpm_resources::RequestAkCertKind;
    use underhill_attestation::AttestationType;
    use underhill_attestation::AttestationVmConfig;
    use vm_resource::declare_static_async_resolver;
    use vm_resource::AsyncResolveResource;
    use vm_resource::IntoResource;
    use vm_resource::PlatformResource;
    use vm_resource::ResolveError;
    use vm_resource::ResourceId;
    use vm_resource::ResourceResolver;

    #[derive(MeshPayload)]
    pub struct GetTpmGetAttestationReportHelperHandle {
        attestation_type: AttestationType,
        attestation_vm_config: AttestationVmConfig,
    }

    impl GetTpmGetAttestationReportHelperHandle {
        pub fn new(
            attestation_type: AttestationType,
            attestation_vm_config: AttestationVmConfig,
        ) -> Self {
            Self {
                attestation_type,
                attestation_vm_config,
            }
        }
    }

    impl ResourceId<GetAttestationReportKind> for GetTpmGetAttestationReportHelperHandle {
        const ID: &'static str = "get_attestation_report";
    }

    pub struct GetTpmGetAttestationReportHelperResolver;

    declare_static_async_resolver! {
        GetTpmGetAttestationReportHelperResolver,
        (GetAttestationReportKind, GetTpmGetAttestationReportHelperHandle)
    }

    /// Error while resolving a [`GetAttestationReportKind`].
    #[derive(Debug, thiserror::Error)]
    #[error("TeeCall unimplemented for {0:?} attestation type")]
    pub struct TeeCallUnimplemented(pub AttestationType);

    #[async_trait]
    impl AsyncResolveResource<GetAttestationReportKind, GetTpmGetAttestationReportHelperHandle>
        for GetTpmGetAttestationReportHelperResolver
    {
        type Output = ResolvedGetAttestationReport;
        type Error = TeeCallUnimplemented;

        async fn resolve(
            &self,
            _resolver: &ResourceResolver,
            handle: GetTpmGetAttestationReportHelperHandle,
            _: &(),
        ) -> Result<Self::Output, Self::Error> {
            let tee_call: Box<dyn tee_call::TeeCall> = match handle.attestation_type {
                AttestationType::Snp => Box::new(tee_call::SnpCall),
                AttestationType::Tdx => Box::new(tee_call::TdxCall),
                ty @ (AttestationType::Host | AttestationType::Unsupported) => {
                    Err(TeeCallUnimplemented(ty))?
                }
            };

            Ok(TpmGetAttestationReportHelper::new(tee_call, handle.attestation_vm_config).into())
        }
    }

    #[derive(MeshPayload)]
    pub struct GetTpmRequestAkCertHelperHandle {
        attestation_agent_data: Option<Vec<u8>>,
    }

    impl GetTpmRequestAkCertHelperHandle {
        pub fn new(attestation_agent_data: Option<Vec<u8>>) -> Self {
            Self {
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

            Ok(TpmRequestAkCertHelper::new(get, handle.attestation_agent_data).into())
        }
    }
}
