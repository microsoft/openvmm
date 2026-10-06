// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU vendors that profiles serve.

use std::fmt;

/// A CPU vendor that profiles serve.
///
/// Intel and AMD enumerate their topology, caches, and speculation controls
/// in different CPUID leaves, so the bits that OpenVMM sets per VM
/// ([`vm_owned_bits`](crate::vm_owned_bits)), the derivation policy
/// ([`derive`](mod@crate::derive)), and the CPUID bits of the Hyper-V
/// processor features ([`hv_banks`](crate::hv_banks)) depend on the vendor.
/// Hygon's CPUs, which follow AMD's layout, have no profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CpuVendor {
    /// Intel, `GenuineIntel`.
    Intel,
    /// AMD, `AuthenticAMD`.
    Amd,
}

impl CpuVendor {
    /// Every vendor that profiles serve.
    pub const ALL: [Self; 2] = [Self::Intel, Self::Amd];

    /// Returns the vendor whose 12-byte CPUID vendor string is `vendor`, if
    /// profiles serve it.
    pub fn from_cpuid_vendor(vendor: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|known| known.cpuid_vendor().as_bytes() == vendor)
    }

    /// Returns the 12-character CPUID vendor string, such as `GenuineIntel`.
    pub const fn cpuid_vendor(self) -> &'static str {
        match self {
            Self::Intel => "GenuineIntel",
            Self::Amd => "AuthenticAMD",
        }
    }

    /// Returns the vendor as profile IDs spell it: `intel` or `amd`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Intel => "intel",
            Self::Amd => "amd",
        }
    }
}

impl fmt::Display for CpuVendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.cpuid_vendor())
    }
}

#[cfg(test)]
mod tests {
    use super::CpuVendor;
    use test_with_tracing::test;

    #[test]
    fn maps_the_cpuid_vendor_strings_that_profiles_serve() {
        assert_eq!(
            CpuVendor::from_cpuid_vendor(b"GenuineIntel"),
            Some(CpuVendor::Intel)
        );
        assert_eq!(
            CpuVendor::from_cpuid_vendor(b"AuthenticAMD"),
            Some(CpuVendor::Amd)
        );
        for vendor in [&b"HygonGenuine"[..], b"CentaurHauls", b"GenuineInte", b""] {
            assert_eq!(CpuVendor::from_cpuid_vendor(vendor), None);
        }
        assert_eq!(
            CpuVendor::ALL.map(|vendor| (vendor.id(), vendor.to_string())),
            [
                ("intel", "GenuineIntel".to_owned()),
                ("amd", "AuthenticAMD".to_owned())
            ]
        );
    }
}
