// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The identity of a host CPU that profile selection and generation checks
//! use.

use std::fmt;

/// The vendor and signature of a host CPU: the vendor string of CPUID leaf 0
/// and `CPUID.1:EAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCpuSignature {
    vendor: [u8; 12],
    signature: u32,
}

impl HostCpuSignature {
    /// Returns the signature of a CPU with the 12-byte `vendor` string and
    /// `CPUID.1:EAX` `signature`.
    pub const fn new(vendor: [u8; 12], signature: u32) -> Self {
        Self { vendor, signature }
    }

    /// Returns the signature from the outputs of CPUID leaves 0 and 1, as
    /// `[eax, ebx, ecx, edx]`.
    pub fn from_cpuid(leaf0: [u32; 4], leaf1: [u32; 4]) -> Self {
        Self::new(vendor_bytes(leaf0[1], leaf0[3], leaf0[2]), leaf1[0])
    }

    /// Returns the signature of the CPU this process runs on.
    // The host CPU is identified with the CPUID instruction of the host,
    // which exists only on x86-64 hosts.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    pub fn current() -> Self {
        let cpuid = |leaf| {
            let result = safe_intrinsics::cpuid(leaf, 0);
            [result.eax, result.ebx, result.ecx, result.edx]
        };
        Self::from_cpuid(cpuid(0), cpuid(1))
    }

    /// Returns the 12-byte vendor string, such as `GenuineIntel`.
    pub fn vendor(&self) -> [u8; 12] {
        self.vendor
    }

    /// Returns the vendor string, with any non-UTF-8 bytes replaced.
    pub fn vendor_str(&self) -> String {
        String::from_utf8_lossy(&self.vendor).into_owned()
    }

    /// Returns `CPUID.1:EAX`.
    pub fn signature(&self) -> u32 {
        self.signature
    }

    /// Returns the display family.
    pub fn family(&self) -> u32 {
        decode_signature(&self.vendor, self.signature).0
    }

    /// Returns the display model.
    pub fn model(&self) -> u32 {
        decode_signature(&self.vendor, self.signature).1
    }

    /// Returns the stepping.
    pub fn stepping(&self) -> u32 {
        decode_signature(&self.vendor, self.signature).2
    }
}

impl fmt::Display for HostCpuSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (family, model, stepping) = decode_signature(&self.vendor, self.signature);
        write!(
            f,
            "{} family {family} model {model} stepping {stepping} (CPUID.1:EAX {:#010x})",
            self.vendor_str(),
            self.signature
        )
    }
}

/// Returns the 12-byte vendor string from CPUID leaf 0's EBX, EDX, and ECX.
pub(crate) fn vendor_bytes(ebx: u32, edx: u32, ecx: u32) -> [u8; 12] {
    let mut vendor = [0; 12];
    for (chunk, register) in vendor
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip([ebx, edx, ecx])
    {
        *chunk = register.to_le_bytes();
    }
    vendor
}

/// Returns the display family, display model, and stepping of the CPUID.1:EAX
/// `signature` of a CPU from `vendor`.
pub(crate) fn decode_signature(vendor: &[u8], signature: u32) -> (u32, u32, u32) {
    let stepping = signature & 0xf;
    let base_model = (signature >> 4) & 0xf;
    let base_family = (signature >> 8) & 0xf;
    let extended_model = (signature >> 16) & 0xf;
    let extended_family = (signature >> 20) & 0xff;
    let family = if base_family == 0xf {
        base_family + extended_family
    } else {
        base_family
    };
    // Intel extends the model for families 6 and 15; AMD only for family 15.
    let extends_model = base_family == 0xf || (base_family == 0x6 && vendor == b"GenuineIntel");
    let model = if extends_model {
        (extended_model << 4) | base_model
    } else {
        base_model
    };
    (family, model, stepping)
}

#[cfg(test)]
mod tests {
    use super::HostCpuSignature;
    use test_with_tracing::test;

    const INTEL_LEAF0: [u32; 4] = [0x1b, 0x756e_6547, 0x6c65_746e, 0x4965_6e69];

    #[test]
    fn decodes_the_vendor_and_signature() {
        let host = HostCpuSignature::from_cpuid(INTEL_LEAF0, [0x0006_06a6, 0, 0, 0]);
        assert_eq!(&host.vendor(), b"GenuineIntel");
        assert_eq!(host.vendor_str(), "GenuineIntel");
        assert_eq!((host.family(), host.model(), host.stepping()), (6, 106, 6));
        assert_eq!(
            host.to_string(),
            "GenuineIntel family 6 model 106 stepping 6 (CPUID.1:EAX 0x000606a6)"
        );
    }

    #[test]
    fn decodes_extended_families_and_amd_models() {
        let intel = HostCpuSignature::new(*b"GenuineIntel", 0x000c_06f2);
        assert_eq!(
            (intel.family(), intel.model(), intel.stepping()),
            (6, 207, 2)
        );
        let amd = HostCpuSignature::new(*b"AuthenticAMD", 0x00a1_0f11);
        assert_eq!((amd.family(), amd.model(), amd.stepping()), (0x19, 0x11, 1));
        // AMD does not extend the model of family 6.
        let old_amd = HostCpuSignature::new(*b"AuthenticAMD", 0x0001_0661);
        assert_eq!((old_amd.family(), old_amd.model()), (6, 6));
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn identifies_this_host() {
        let host = HostCpuSignature::current();
        assert!(host.family() != 0, "{host}");
    }
}
