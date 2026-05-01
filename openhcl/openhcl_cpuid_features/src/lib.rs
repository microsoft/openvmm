// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared CPUID feature detection for OpenHCL components.

// xtask-fmt allow-target-arch cpu-intrinsic
#![cfg(target_arch = "x86_64")]

use hvdef::HvEnlightenmentInformation;
use x86defs::cpuid::VersionAndFeaturesEcx;

/// Cached CPUID feature detection results.
#[derive(Clone, Copy, Debug)]
pub struct CpuidFeatures {
    enlightenment_info: HvEnlightenmentInformation,
    version_features: VersionAndFeaturesEcx,
}

impl CpuidFeatures {
    /// Queries host CPUID and builds a snapshot of relevant feature leaves.
    pub fn new() -> Self {
        let result =
            safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION, 0);
        let enlightenment_info = HvEnlightenmentInformation::from_cpuid([
            result.eax, result.ebx, result.ecx, result.edx,
        ]);

        let result = safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VersionAndFeatures.0, 0);
        let version_features = VersionAndFeaturesEcx::from(result.ecx);

        Self {
            enlightenment_info,
            version_features,
        }
    }

    /// Returns whether Hyper-V recommends MMIO access hypercalls.
    pub fn use_hypercall_for_mmio_access(&self) -> bool {
        self.enlightenment_info.use_hypercall_for_mmio_access()
    }

    /// Returns whether x2APIC is supported by the host CPU.
    pub fn x2_apic_supported(&self) -> bool {
        self.version_features.x2_apic()
    }

    /// Returns whether lower VTL guest request interception is supported.
    pub fn supports_lower_vtl_guest_request(&self) -> bool {
        self.enlightenment_info.lower_vtl_guest_request_support()
    }

    /// Returns whether restore partition time on resume is supported.
    pub fn supports_restore_partition_time(&self) -> bool {
        self.enlightenment_info.restore_time_on_resume()
    }
}

impl Default for CpuidFeatures {
    fn default() -> Self {
        Self::new()
    }
}
