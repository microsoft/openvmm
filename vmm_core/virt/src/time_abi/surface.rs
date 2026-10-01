// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host CPU facts for the CPU profile record of a snapshot, the CPU surface a
//! backend supports, and the interim CPU profile IDs that backends still
//! recognize.

/// The prefix of every interim CPU profile ID.
const INTERIM_CPU_PROFILE_PREFIX: &str = "interim.host.";

/// Returns the ID of the interim CPU profile of `hypervisor`, which named the
/// backend's own CPU features before CPU profiles. Core no longer selects or
/// restores interim profiles; backends that still recognize the ID drop it
/// together with this function.
pub fn interim_cpu_profile_id(hypervisor: &str) -> String {
    format!("{INTERIM_CPU_PROFILE_PREFIX}{hypervisor}.v1")
}

/// Returns whether `id` names an interim CPU profile rather than a pinned
/// one. See [`interim_cpu_profile_id`].
pub fn is_interim_cpu_profile(id: &str) -> bool {
    id.starts_with(INTERIM_CPU_PROFILE_PREFIX)
}

/// Returns the host's display signature, CPUID.1:EAX, as the VMM's host OS
/// sees it, or `None` on a host that is not x86-64.
pub fn host_cpu_signature() -> Option<u32> {
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    {
        Some(safe_intrinsics::cpuid(1, 0).eax)
    }
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// The CPU surface a backend supports on this host, as
/// [`TimeAbiBackend::supported_cpu_surface`](super::TimeAbiBackend::supported_cpu_surface)
/// reports it: what `cpu_profile::HostCpuSurface` holds, in this crate's
/// types.
#[derive(Debug, Clone, Default)]
pub struct SupportedCpuSurface {
    /// Every CPUID leaf and subleaf the backend can expose, with every
    /// feature it can expose set. Masks are ignored.
    pub cpuid: Vec<crate::CpuidLeaf>,
    /// The widest guest physical address the backend supports, in bits.
    pub physical_address_width: u8,
    /// The MSR values the backend can present to a guest.
    pub msrs: Vec<SupportedMsrValue>,
}

/// The values of one MSR that a backend can present to a guest.
#[derive(Debug, Clone, Copy)]
pub struct SupportedMsrValue {
    /// The MSR index.
    pub index: u32,
    /// The bits the backend can present set.
    pub supported: u64,
    /// The bits the backend can present clear, even where `supported` has
    /// them.
    pub controllable: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interim_cpu_profiles() {
        assert_eq!(interim_cpu_profile_id("kvm"), "interim.host.kvm.v1");
        assert!(is_interim_cpu_profile("interim.host.mshv.v1"));
        for id in ["auto", "intel.icelake-sp.v1", "", "interim"] {
            assert!(!is_interim_cpu_profile(id), "{id}");
        }
        assert_ne!(host_cpu_signature(), Some(0));
    }
}
