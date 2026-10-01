// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host CPU facts for the CPU profile record of a snapshot, and the CPU
//! surface a backend supports.

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
    fn host_cpu_signature_is_reported() {
        assert_ne!(host_cpu_signature(), Some(0));
    }
}
