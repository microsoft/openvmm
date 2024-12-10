// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guest_emulation_transport::GuestEmulationTransportClient;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::igvm_attest::get::AK_CERT_RESPONSE_BUFFER_SIZE;
use thiserror::Error;
use tpm::ak_cert::RequestAkCert;
use underhill_attestation::AttestationType;

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum TpmAttestationError {
    #[error("failed to get a hardware attestation report")]
    GetAttestationReport(#[source] tee_call::Error),
    #[error("failed to create the IgvmAttest AK_CERT request")]
    CreateAkCertRequest(#[source] underhill_attestation::IgvmAttestError),
}

/// An implementation of [`RequestAkCert`].
#[derive(Clone)]
pub struct TpmRequestAkCertHelper {
    get_client: GuestEmulationTransportClient,
    tee_call: Option<Box<dyn tee_call::TeeCall>>,
    attestation_type: AttestationType,
    attestation_vm_config: AttestationVmConfig,
    attestation_agent_data: Option<Vec<u8>>,
}

impl TpmRequestAkCertHelper {
    pub fn new(
        get_client: GuestEmulationTransportClient,
        tee_call: Option<Box<dyn tee_call::TeeCall>>,
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
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let tee_type = match self.attestation_type {
            AttestationType::Snp => Some(tee_call::TeeType::Snp),
            AttestationType::Tdx => Some(tee_call::TeeType::Tdx),
            AttestationType::Host => None,
            AttestationType::VbsUnsupported => panic!("VBS not supported yet"), // TODO VBS
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
            );

        let attestation_report = if let Some(tee_call) = &self.tee_call {
            tee_call
                .get_attestation_report(&ak_cert_request_helper.runtime_claims_hash)
                .map_err(TpmAttestationError::GetAttestationReport)?
                .report
        } else {
            vec![]
        };

        let request = ak_cert_request_helper
            .create_request(&attestation_report)
            .map_err(TpmAttestationError::CreateAkCertRequest)?;

        // The request will be exposed to the guest (via nv index) for isolated VMs.
        Ok(request)
    }

    async fn request_ak_cert(
        &self,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync + 'static>> {
        let agent_data = self.attestation_agent_data.clone().unwrap_or_default();
        let result = self
            .get_client
            .igvm_attest(agent_data, request, AK_CERT_RESPONSE_BUFFER_SIZE)
            .await?;
        let payload = underhill_attestation::parse_ak_cert_response(&result.response)?;

        Ok(payload)
    }

    fn clone_box(&self) -> Box<dyn RequestAkCert> {
        Box::new(self.clone())
    }
}

pub mod resources {
    use super::TpmRequestAkCertHelper;
    use async_trait::async_trait;
    use guest_emulation_transport::resolver::GetClientKind;
    use mesh::MeshPayload;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
    use tpm::ak_cert::ResolvedRequestAkCert;
    use tpm_resources::RequestAkCertKind;
    use underhill_attestation::AttestationType;
    use vm_resource::declare_static_async_resolver;
    use vm_resource::AsyncResolveResource;
    use vm_resource::IntoResource;
    use vm_resource::PlatformResource;
    use vm_resource::ResolveError;
    use vm_resource::ResourceId;
    use vm_resource::ResourceResolver;

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

            let tee_call: Option<Box<dyn tee_call::TeeCall>> = match handle.attestation_type {
                AttestationType::Snp => Some(Box::new(tee_call::SnpCall)),
                AttestationType::Tdx => Some(Box::new(tee_call::TdxCall)),
                AttestationType::Host => None,
                AttestationType::VbsUnsupported => panic!("VBS not supported yet"), // TODO VBS,
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
}
