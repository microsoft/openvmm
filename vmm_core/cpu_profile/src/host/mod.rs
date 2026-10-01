// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The identity of the host: its CPU, its OS, and the hypervisor that the
//! host OS itself runs under, if any.
//!
//! The host CPU identity comes from the CPUID instruction as the host OS sees
//! it, which on a root partition or a nested host is itself virtualized. Only
//! fields that do not depend on the executing processor are recorded.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use crate::Hex32;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// A failure to identify the host.
#[derive(Debug, Error)]
pub enum HostIdentityError {
    /// The host is not an x86-64 machine.
    #[error("host CPU identification requires an x86-64 host")]
    UnsupportedArchitecture,
}

/// The identity of the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    /// The host CPU.
    pub cpu: HostCpu,
    /// The host OS.
    pub os: HostOs,
    /// The hypervisor that the host OS runs under: the hypervisor of a root
    /// partition, or the outer hypervisor of a nested host.
    pub hypervisor: Option<HostHypervisor>,
}

/// The host CPU.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCpu {
    /// The vendor string of CPUID leaf 0, such as `GenuineIntel`.
    pub vendor: String,
    /// The processor signature, CPUID.1:EAX.
    pub signature: Hex32,
    /// The display family decoded from the signature.
    pub family: u32,
    /// The display model decoded from the signature.
    pub model: u32,
    /// The stepping decoded from the signature.
    pub stepping: u32,
    /// The brand string of CPUID leaves 0x80000002 through 0x80000004.
    pub brand: String,
    /// The microcode revisions that the host OS reports, in lowercase hex.
    /// More than one value means that the processors disagree. It is empty
    /// when the OS does not report a revision.
    pub microcode: Vec<String>,
    /// The host OS sees an invariant TSC, `CPUID.0x80000007:EDX[8]`.
    pub invariant_tsc: bool,
    /// The host OS sees the TSC-deadline timer, `CPUID.1:ECX[24]`.
    pub tsc_deadline: bool,
    /// The host OS sees `IA32_TSC_ADJUST`, `CPUID.(7,0):EBX[1]`.
    pub tsc_adjust: bool,
}

/// The host OS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOs {
    /// The OS family, such as `linux` or `windows`.
    pub kind: String,
    /// The OS release: the kernel release on Linux, and
    /// `major.minor.build.revision` on Windows.
    pub release: Option<String>,
    /// The OS version: the kernel version string on Linux, and the product
    /// name and display version on Windows.
    pub version: Option<String>,
    /// The CPU flags that the Linux kernel reports for its first processor,
    /// sorted.
    pub cpu_flags: Vec<String>,
    /// The current Linux clocksource.
    pub clocksource: Option<String>,
    /// The available Linux clocksources, sorted.
    pub available_clocksources: Vec<String>,
}

/// The hypervisor that the host OS runs under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostHypervisor {
    /// The vendor signature of CPUID leaf 0x40000000, such as `Microsoft Hv`.
    pub vendor: String,
    /// The maximum hypervisor leaf, CPUID.0x40000000:EAX.
    pub max_leaf: Hex32,
    /// The interface signature of CPUID leaf 0x40000001, such as `Hv#1`.
    pub interface: String,
    /// For the `Hv#1` interface, the hypervisor version of CPUID leaf
    /// 0x40000002 as `major.minor.build.service`.
    pub version: Option<String>,
}

/// The information that the host OS reports about itself and the CPU.
#[derive(Default)]
struct OsInfo {
    os: Option<HostOs>,
    microcode: Vec<String>,
}

impl HostIdentity {
    /// Identifies the host this process runs on.
    pub fn collect() -> Result<Self, HostIdentityError> {
        let mut cpuid = host_cpuid()?;
        let info = os_info();
        let (cpu, hypervisor) = identify_cpu(&mut cpuid, info.microcode);
        let os = info.os.unwrap_or_else(|| HostOs {
            kind: std::env::consts::OS.to_owned(),
            release: None,
            version: None,
            cpu_flags: Vec::new(),
            clocksource: None,
            available_clocksources: Vec::new(),
        });
        Ok(Self {
            cpu,
            os,
            hypervisor,
        })
    }
}

// The host CPU is identified with the CPUID instruction of the host, which
// exists only on x86-64 hosts.
// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(target_arch = "x86_64")]
fn host_cpuid() -> Result<impl FnMut(u32, u32) -> [u32; 4], HostIdentityError> {
    Ok(|leaf, subleaf| {
        let result = safe_intrinsics::cpuid(leaf, subleaf);
        [result.eax, result.ebx, result.ecx, result.edx]
    })
}

// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(not(target_arch = "x86_64"))]
fn host_cpuid() -> Result<fn(u32, u32) -> [u32; 4], HostIdentityError> {
    Err(HostIdentityError::UnsupportedArchitecture)
}

#[cfg(target_os = "linux")]
fn os_info() -> OsInfo {
    linux::os_info()
}

#[cfg(windows)]
fn os_info() -> OsInfo {
    windows::os_info()
}

#[cfg(not(any(target_os = "linux", windows)))]
fn os_info() -> OsInfo {
    OsInfo::default()
}

fn signature_string(registers: &[u32]) -> String {
    let bytes = registers
        .iter()
        .flat_map(|register| register.to_le_bytes())
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .trim()
        .to_owned()
}

/// Identifies the CPU, and the hypervisor the host runs under, from the
/// CPUID results of `cpuid`.
fn identify_cpu(
    cpuid: &mut dyn FnMut(u32, u32) -> [u32; 4],
    microcode: Vec<String>,
) -> (HostCpu, Option<HostHypervisor>) {
    let [max_basic, ebx, ecx, edx] = cpuid(0, 0);
    let vendor = signature_string(&[ebx, edx, ecx]);
    let [signature, _, features_ecx, _] = if max_basic >= 1 { cpuid(1, 0) } else { [0; 4] };
    let extended_features_ebx = if max_basic >= 7 { cpuid(7, 0)[1] } else { 0 };
    let max_extended = cpuid(0x8000_0000, 0)[0];
    let brand = if max_extended >= 0x8000_0004 {
        signature_string(
            &(0x8000_0002..=0x8000_0004)
                .flat_map(|leaf| cpuid(leaf, 0))
                .collect::<Vec<_>>(),
        )
    } else {
        String::new()
    };
    let invariant_tsc = max_extended >= 0x8000_0007 && cpuid(0x8000_0007, 0)[3] & (1 << 8) != 0;
    let (family, model, stepping) =
        crate::signature::decode_signature(vendor.as_bytes(), signature);

    let cpu = HostCpu {
        vendor,
        signature: Hex32(signature),
        family,
        model,
        stepping,
        brand,
        microcode,
        invariant_tsc,
        tsc_deadline: features_ecx & (1 << 24) != 0,
        tsc_adjust: extended_features_ebx & (1 << 1) != 0,
    };

    let hypervisor = (features_ecx & (1 << 31) != 0).then(|| {
        let [max_leaf, ebx, ecx, edx] = cpuid(0x4000_0000, 0);
        let interface = if max_leaf >= 0x4000_0001 {
            signature_string(&[cpuid(0x4000_0001, 0)[0]])
        } else {
            String::new()
        };
        let version = (interface == "Hv#1" && max_leaf >= 0x4000_0002).then(|| {
            let [build, major_minor, _, service] = cpuid(0x4000_0002, 0);
            format!(
                "{}.{}.{}.{}",
                major_minor >> 16,
                major_minor & 0xffff,
                build,
                service & 0xff_ffff
            )
        });
        HostHypervisor {
            vendor: signature_string(&[ebx, ecx, edx]),
            max_leaf: Hex32(max_leaf),
            interface,
            version,
        }
    });

    (cpu, hypervisor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn word(text: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*text)
    }

    /// CPUID of a Skylake-SP root partition under Hyper-V.
    fn skylake_root(leaf: u32, _subleaf: u32) -> [u32; 4] {
        let brand = b"Intel(R) Xeon(R) Silver 4114 CPU @ 2.20GHz\0\0\0\0\0\0";
        let brand_word = |i: usize| u32::from_le_bytes(brand[i * 4..i * 4 + 4].try_into().unwrap());
        match leaf {
            0 => [0x16, word(b"Genu"), word(b"ntel"), word(b"ineI")],
            1 => [
                0x0005_0654,
                0x0b20_0800,
                0xfefa_3203 | (1 << 31),
                0x1f8b_fbff,
            ],
            7 => [0, 0x009c_6fbb, 0, 0],
            0x4000_0000 => [0x4000_000c, word(b"Micr"), word(b"osof"), word(b"t Hv")],
            0x4000_0001 => [word(b"Hv#1"), 0, 0, 0],
            0x4000_0002 => [26100, 0x000a_0000, 0, 0x0100_1234],
            0x8000_0000 => [0x8000_0008, 0, 0, 0],
            0x8000_0002..=0x8000_0004 => {
                let base = (leaf - 0x8000_0002) as usize * 4;
                [
                    brand_word(base),
                    brand_word(base + 1),
                    brand_word(base + 2),
                    brand_word(base + 3),
                ]
            }
            0x8000_0007 => [0, 0, 0, 1 << 8],
            _ => [0; 4],
        }
    }

    #[test]
    fn identifies_intel_root_partition() {
        let (cpu, hypervisor) = identify_cpu(&mut skylake_root, vec!["0x2007006".to_owned()]);
        assert_eq!(cpu.vendor, "GenuineIntel");
        assert_eq!((cpu.family, cpu.model, cpu.stepping), (6, 85, 4));
        assert_eq!(cpu.brand, "Intel(R) Xeon(R) Silver 4114 CPU @ 2.20GHz");
        assert_eq!(cpu.microcode, ["0x2007006"]);
        assert!(cpu.invariant_tsc);
        assert!(!cpu.tsc_deadline);
        assert!(cpu.tsc_adjust);

        let hypervisor = hypervisor.unwrap();
        assert_eq!(hypervisor.vendor, "Microsoft Hv");
        assert_eq!(hypervisor.interface, "Hv#1");
        assert_eq!(hypervisor.max_leaf, Hex32(0x4000_000c));
        assert_eq!(hypervisor.version.as_deref(), Some("10.0.26100.4660"));
    }

    #[test]
    fn decodes_extended_families_and_models() {
        let mut emerald_rapids = |leaf: u32, _: u32| -> [u32; 4] {
            match leaf {
                0 => [1, word(b"Genu"), word(b"ntel"), word(b"ineI")],
                1 => [0x000c_06f2, 0, 0, 0],
                _ => [0; 4],
            }
        };
        let (cpu, hypervisor) = identify_cpu(&mut emerald_rapids, Vec::new());
        assert_eq!((cpu.family, cpu.model, cpu.stepping), (6, 207, 2));
        assert!(hypervisor.is_none());
        assert!(cpu.brand.is_empty());

        let mut zen4 = |leaf: u32, _: u32| -> [u32; 4] {
            match leaf {
                0 => [1, word(b"Auth"), word(b"cAMD"), word(b"enti")],
                1 => [0x00a1_0f11, 0, 0, 0],
                _ => [0; 4],
            }
        };
        let (cpu, _) = identify_cpu(&mut zen4, Vec::new());
        assert_eq!(cpu.vendor, "AuthenticAMD");
        assert_eq!((cpu.family, cpu.model, cpu.stepping), (0x19, 0x11, 1));
    }

    #[test]
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    fn collects_this_host() {
        let identity = HostIdentity::collect().unwrap();
        assert!(!identity.cpu.vendor.is_empty());
        assert_eq!(identity.os.kind, std::env::consts::OS);
        // Collection is deterministic.
        assert_eq!(HostIdentity::collect().unwrap(), identity);
    }
}
