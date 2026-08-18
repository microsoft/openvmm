// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86-specific translation of loader imports to KVM SNP page types.

use crate::snp::SnpError;
use virt::InitialPageImportType;

/// Returns the KVM SNP launch page type for a loader import.
///
/// Imports without a direct SNP launch representation are rejected rather
/// than being measured with an unintended page type.
pub fn snp_launch_page_type(
    import_type: InitialPageImportType,
) -> Result<kvm::SevSnpPageType, SnpError> {
    match import_type {
        // KVM owns its runtime VMSA and has no SNP launch-update VMSA page
        // type. This reserved guest page carries loader state to OpenVMM only;
        // accept it as normal RAM after applying that state to the KVM vCPU.
        InitialPageImportType::Normal | InitialPageImportType::VpContext => {
            Ok(kvm::SevSnpPageType::Normal)
        }
        InitialPageImportType::NormalUnmeasured => Ok(kvm::SevSnpPageType::Unmeasured),
        InitialPageImportType::Secrets => Ok(kvm::SevSnpPageType::Secrets),
        InitialPageImportType::Cpuid => Ok(kvm::SevSnpPageType::Cpuid),
        InitialPageImportType::Shared | InitialPageImportType::CpuidExtendedState => {
            Err(SnpError::UnsupportedPageImportType(import_type))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn maps_supported_imports_to_kvm_page_types() {
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Normal).unwrap(),
            kvm::SevSnpPageType::Normal
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::NormalUnmeasured).unwrap(),
            kvm::SevSnpPageType::Unmeasured
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Secrets).unwrap(),
            kvm::SevSnpPageType::Secrets
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Cpuid).unwrap(),
            kvm::SevSnpPageType::Cpuid
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::VpContext).unwrap(),
            kvm::SevSnpPageType::Normal
        );
    }

    #[test]
    fn rejects_unsupported_imports() {
        for import_type in [
            InitialPageImportType::Shared,
            InitialPageImportType::CpuidExtendedState,
        ] {
            assert!(matches!(
                snp_launch_page_type(import_type),
                Err(SnpError::UnsupportedPageImportType(actual)) if actual == import_type
            ));
        }
    }
}
