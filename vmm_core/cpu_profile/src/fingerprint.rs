// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU fingerprint document.
//!
//! A [`CpuFingerprint`] combines the [`HostIdentity`] of the host with the
//! [`BackendFingerprint`] of one hypervisor backend: the guest CPU surface it
//! supports and its time capabilities. It carries two digests:
//!
//! - `surface_digest` covers only the guest CPU surface: the CPUID table, the
//!   XSAVE layout, the feature MSRs, and the feature banks. Hosts that offer
//!   guests the same CPU through the same backend share it, regardless of
//!   their TSC rates, software versions, or capability values.
//! - `digest` covers the whole document except itself.
//!
//! Both are SHA-256 over the compact canonical JSON encoding (object keys
//! sorted, no whitespace). The document itself is written as pretty
//! canonical JSON, so the same host, backend, and tool always produce the
//! same bytes.

use crate::Hex32;
use crate::Hex64;
use crate::canonical;
use crate::cpuid;
use crate::cpuid::CpuidEntry;
use crate::cpuid::XsaveComponent;
use crate::host::HostIdentity;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use thiserror::Error;

/// The schema of the fingerprint documents this crate reads and writes.
pub const SCHEMA: &str = "openvmm-cpu-fingerprint/v1";

/// The index of `IA32_ARCH_CAPABILITIES`.
pub const IA32_ARCH_CAPABILITIES: u32 = 0x10a;

/// An invalid fingerprint document.
#[derive(Debug, Error)]
pub enum FingerprintError {
    /// The document is not valid JSON or does not match the schema's shape.
    #[error("malformed CPU fingerprint")]
    Json(#[source] serde_json::Error),
    /// The document declares another schema.
    #[error("unsupported CPU fingerprint schema {0:?}, expected {SCHEMA:?}")]
    Schema(String),
    /// A recorded digest does not match the content.
    #[error("CPU fingerprint {field} mismatch: recorded {recorded}, computed {computed}")]
    Digest {
        /// The digest field.
        field: &'static str,
        /// The digest the document records.
        recorded: String,
        /// The digest of the document's content.
        computed: String,
    },
}

/// The tool that produced a fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolIdentity {
    /// The tool name, such as `openvmm`.
    pub name: String,
    /// The tool version, including its source revision when known.
    pub version: String,
}

/// A host CPU fingerprint for one hypervisor backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuFingerprint {
    /// The document schema, [`SCHEMA`].
    pub schema: String,
    /// The tool that produced the fingerprint.
    pub tool: ToolIdentity,
    /// The host CPU, OS, and hypervisor.
    pub host: HostIdentity,
    /// The guest CPU surface and time capabilities of the backend.
    pub backend: BackendFingerprint,
    /// The digest of the guest CPU surface, see
    /// [`BackendFingerprint::surface_digest`].
    pub surface_digest: String,
    /// The digest of the whole document except this field.
    pub digest: String,
}

impl CpuFingerprint {
    /// Assembles a fingerprint and computes its digests.
    pub fn new(tool: ToolIdentity, host: HostIdentity, backend: BackendFingerprint) -> Self {
        let mut this = Self {
            schema: SCHEMA.to_owned(),
            tool,
            host,
            backend,
            surface_digest: String::new(),
            digest: String::new(),
        };
        this.surface_digest = this.backend.surface_digest();
        this.digest = this.content_digest();
        this
    }

    /// Returns the fingerprint as pretty canonical JSON, with a final
    /// newline.
    pub fn to_json(&self) -> String {
        canonical::to_pretty(&to_value(self))
    }

    /// Parses a fingerprint and verifies its schema and digests.
    pub fn from_json(json: &str) -> Result<Self, FingerprintError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(FingerprintError::Json)?;
        let schema = value.get("schema").and_then(|schema| schema.as_str());
        if schema != Some(SCHEMA) {
            return Err(FingerprintError::Schema(
                schema.unwrap_or_default().to_owned(),
            ));
        }
        let this: Self = serde_json::from_value(value).map_err(FingerprintError::Json)?;
        this.verify()?;
        Ok(this)
    }

    /// Verifies that the recorded digests match the content.
    pub fn verify(&self) -> Result<(), FingerprintError> {
        let check = |field, recorded: &str, computed: String| {
            if recorded == computed {
                Ok(())
            } else {
                Err(FingerprintError::Digest {
                    field,
                    recorded: recorded.to_owned(),
                    computed,
                })
            }
        };
        check(
            "surface_digest",
            &self.surface_digest,
            self.backend.surface_digest(),
        )?;
        check("digest", &self.digest, self.content_digest())
    }

    fn content_digest(&self) -> String {
        let mut value = to_value(self);
        value
            .as_object_mut()
            .expect("a fingerprint serializes as an object")
            .remove("digest");
        canonical::digest(&value)
    }
}

/// The guest CPU surface and time capabilities that one backend supports on
/// one host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendFingerprint {
    /// The backend: `kvm`, `mshv`, or `whp`.
    pub name: String,
    /// How the backend reported its CPUID table.
    pub method: String,
    /// The supported CPUID table, normalized by [`cpuid::normalize`].
    pub cpuid: Vec<CpuidEntry>,
    /// The XSAVE features and layout that the CPUID table describes.
    pub xsave: XsaveInfo,
    /// The feature MSRs and the MSRs the backend supports.
    pub msrs: MsrInfo,
    /// The time capabilities of the backend.
    pub time: TimeCapabilities,
    /// Processor feature banks, which together with the CPUID table define
    /// the guest CPU surface of hypervisors configured through banks.
    pub feature_banks: BTreeMap<String, Hex64>,
    /// Other raw capability and property values reported by the backend.
    pub values: BTreeMap<String, Hex64>,
    /// Other CPUID tables the backend reports, such as the Hyper-V
    /// enlightenments it can emulate.
    pub extra_cpuid: BTreeMap<String, Vec<CpuidEntry>>,
    /// Backend queries that failed, with their errors.
    pub unavailable: BTreeMap<String, String>,
}

impl BackendFingerprint {
    /// Returns a backend fingerprint for the CPUID table `cpuid`, obtained
    /// through `method`.
    ///
    /// The table is normalized, and the XSAVE information and the CPUID time
    /// bits are derived from it.
    pub fn new(name: &str, method: &str, mut cpuid: Vec<CpuidEntry>) -> Self {
        cpuid::normalize(&mut cpuid);
        let xsave = XsaveInfo::from_cpuid(&cpuid);
        let time = TimeCapabilities::from_cpuid(&cpuid);
        Self {
            name: name.to_owned(),
            method: method.to_owned(),
            cpuid,
            xsave,
            msrs: MsrInfo::default(),
            time,
            feature_banks: BTreeMap::new(),
            values: BTreeMap::new(),
            extra_cpuid: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        }
    }

    /// Records a raw capability or property value.
    pub fn set_value(&mut self, name: impl Into<String>, value: u64) {
        self.values.insert(name.into(), Hex64(value));
    }

    /// Records a processor feature bank.
    pub fn set_feature_bank(&mut self, name: impl Into<String>, value: u64) {
        self.feature_banks.insert(name.into(), Hex64(value));
    }

    /// Records another CPUID table, normalized.
    pub fn set_extra_cpuid(&mut self, name: impl Into<String>, mut entries: Vec<CpuidEntry>) {
        cpuid::normalize(&mut entries);
        self.extra_cpuid.insert(name.into(), entries);
    }

    /// Records that the query `name` failed with `error`.
    pub fn set_unavailable(
        &mut self,
        name: impl Into<String>,
        error: &(dyn std::error::Error + 'static),
    ) {
        self.unavailable.insert(name.into(), error_chain(error));
    }

    /// Records the outcome of the query `name`: its value as
    /// [`values`](Self::values) when it succeeded, and its error as
    /// [`unavailable`](Self::unavailable) when it failed.
    pub fn record<E: std::error::Error + 'static>(
        &mut self,
        name: &str,
        result: Result<u64, E>,
    ) -> Option<u64> {
        match result {
            Ok(value) => {
                self.set_value(name, value);
                Some(value)
            }
            Err(error) => {
                self.set_unavailable(name, &error);
                None
            }
        }
    }

    /// Records the feature MSRs and their values, sorted by index, and
    /// derives [`MsrInfo::arch_capabilities`] from them.
    pub fn set_feature_msrs(&mut self, msrs: impl IntoIterator<Item = (u32, u64)>) {
        let mut msrs = msrs
            .into_iter()
            .map(|(index, value)| MsrEntry {
                index: Hex32(index),
                value: Hex64(value),
            })
            .collect::<Vec<_>>();
        msrs.sort_by_key(|msr| msr.index);
        msrs.dedup_by_key(|msr| msr.index);
        self.msrs.arch_capabilities = msrs
            .iter()
            .find(|msr| msr.index.0 == IA32_ARCH_CAPABILITIES)
            .map(|msr| msr.value);
        self.msrs.feature_msrs = msrs;
    }

    /// Records the MSRs the backend supports, sorted.
    pub fn set_supported_msrs(&mut self, msrs: impl IntoIterator<Item = u32>) {
        let mut msrs = msrs.into_iter().map(Hex32).collect::<Vec<_>>();
        msrs.sort_unstable();
        msrs.dedup();
        self.msrs.supported = msrs;
    }

    /// Returns the digest of the guest CPU surface: the backend name, the
    /// CPUID table, the XSAVE information, the feature MSRs, and the feature
    /// banks.
    pub fn surface_digest(&self) -> String {
        #[derive(Serialize)]
        struct Surface<'a> {
            backend: &'a str,
            cpuid: &'a [CpuidEntry],
            xsave: &'a XsaveInfo,
            feature_msrs: &'a [MsrEntry],
            arch_capabilities: Option<Hex64>,
            feature_banks: &'a BTreeMap<String, Hex64>,
        }

        canonical::digest(&to_value(&Surface {
            backend: &self.name,
            cpuid: &self.cpuid,
            xsave: &self.xsave,
            feature_msrs: &self.msrs.feature_msrs,
            arch_capabilities: self.msrs.arch_capabilities,
            feature_banks: &self.feature_banks,
        }))
    }
}

/// The XSAVE features and layout of a CPUID table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XsaveInfo {
    /// The supported XCR0 bits, CPUID.(0xd,0):EDX:EAX.
    pub xcr0_supported: Hex64,
    /// The supported IA32_XSS bits, CPUID.(0xd,1):EDX:ECX.
    pub xss_supported: Hex64,
    /// The XSAVE instruction extensions, CPUID.(0xd,1):EAX: XSAVEOPT,
    /// XSAVEC, XGETBV with ECX=1, XSAVES, and XFD.
    pub extensions: Hex32,
    /// The size of a standard-format area for every supported user state
    /// component as the CPU reports it, CPUID.(0xd,0):ECX.
    pub max_standard_size: u32,
    /// The size of a standard-format area for every supported user state
    /// component, computed from the component layout.
    pub standard_size: u32,
    /// The size of a compacted-format area for every supported state
    /// component, computed from the component layout.
    pub compacted_size: u32,
    /// The supported state components beyond x87 and SSE.
    pub components: Vec<XsaveComponent>,
}

impl XsaveInfo {
    /// Derives the XSAVE information from a CPUID table.
    pub fn from_cpuid(entries: &[CpuidEntry]) -> Self {
        let (xcr0, xss) = cpuid::xsave_supported(entries);
        let components = cpuid::xsave_components(entries);
        let first = cpuid::lookup(entries, 0xd, 0).unwrap_or_default();
        let second = cpuid::lookup(entries, 0xd, 1).unwrap_or_default();
        Self {
            xcr0_supported: Hex64(xcr0),
            xss_supported: Hex64(xss),
            extensions: Hex32(second[0]),
            max_standard_size: first[2],
            standard_size: cpuid::xsave_standard_size(&components, xcr0),
            compacted_size: cpuid::xsave_compacted_size(&components, xcr0 | xss),
            components,
        }
    }
}

/// An MSR index and value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MsrEntry {
    /// The MSR index.
    pub index: Hex32,
    /// The MSR value.
    pub value: Hex64,
}

/// The MSRs of a backend.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MsrInfo {
    /// The feature MSRs and the values the backend reports for guests, such
    /// as the KVM MSR-based features, sorted by index.
    pub feature_msrs: Vec<MsrEntry>,
    /// The MSRs the backend can save and restore for a virtual processor,
    /// sorted.
    pub supported: Vec<Hex32>,
    /// The `IA32_ARCH_CAPABILITIES` value exposed to guests, when the backend
    /// reports one.
    pub arch_capabilities: Option<Hex64>,
}

/// The time capabilities of a backend.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeCapabilities {
    /// The guest TSC rate, in Hz, that the backend reports.
    pub tsc_frequency_hz: Option<u64>,
    /// The local APIC timer rate, in Hz, that the backend reports.
    pub lapic_timer_frequency_hz: Option<u64>,
    /// The backend can expose an invariant TSC, `CPUID.0x80000007:EDX[8]`.
    pub invariant_tsc: bool,
    /// The backend can expose the TSC-deadline timer, `CPUID.1:ECX[24]`.
    pub tsc_deadline: bool,
    /// The backend can expose `IA32_TSC_ADJUST`, `CPUID.(7,0):EBX[1]`.
    pub tsc_adjust: bool,
    /// The backend can expose RDTSCP, `CPUID.0x80000001:EDX[27]`.
    pub rdtscp: bool,
    /// The backend can expose an always-running APIC timer, `CPUID.6:EAX[2]`.
    pub arat: bool,
    /// The backend can set each virtual processor's TSC (or TSC offset)
    /// without trapping RDTSC.
    pub tsc_offset_control: Option<bool>,
    /// The backend can run a guest at a TSC rate other than the host's.
    pub tsc_scaling: Option<bool>,
    /// The backend can freeze partition time while it changes VP state.
    pub time_freeze: Option<bool>,
    /// The MSR interception mechanisms the backend offers, by name.
    pub msr_intercepts: BTreeMap<String, bool>,
}

impl TimeCapabilities {
    /// Returns the capabilities that a CPUID table determines; the backend
    /// fills in the others.
    pub fn from_cpuid(entries: &[CpuidEntry]) -> Self {
        Self {
            invariant_tsc: cpuid::has_bit(entries, 0x8000_0007, 0, 3, 8),
            tsc_deadline: cpuid::has_bit(entries, 0x1, 0, 2, 24),
            tsc_adjust: cpuid::has_bit(entries, 0x7, 0, 1, 1),
            rdtscp: cpuid::has_bit(entries, 0x8000_0001, 0, 3, 27),
            arat: cpuid::has_bit(entries, 0x6, 0, 0, 2),
            ..Default::default()
        }
    }
}

fn to_value<T: Serialize>(value: &T) -> serde_json::Value {
    // Every type here has string map keys and infallible field
    // serialization.
    serde_json::to_value(value).expect("fingerprint serialization is infallible")
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostCpu;
    use crate::host::HostOs;
    use test_with_tracing::test;

    fn host() -> HostIdentity {
        HostIdentity {
            cpu: HostCpu {
                vendor: "GenuineIntel".to_owned(),
                signature: Hex32(0x0005_0654),
                family: 6,
                model: 85,
                stepping: 4,
                brand: "Intel(R) Xeon(R) Silver 4114 CPU @ 2.20GHz".to_owned(),
                microcode: vec!["0x2007006".to_owned()],
                invariant_tsc: true,
                tsc_deadline: true,
                tsc_adjust: true,
            },
            os: HostOs {
                kind: "linux".to_owned(),
                release: Some("7.0.0-30-generic".to_owned()),
                version: None,
                cpu_flags: vec!["constant_tsc".to_owned(), "nonstop_tsc".to_owned()],
                clocksource: Some("tsc".to_owned()),
                available_clocksources: vec!["hpet".to_owned(), "tsc".to_owned()],
            },
            hypervisor: None,
        }
    }

    fn backend(cpuid_order_reversed: bool) -> BackendFingerprint {
        let mut cpuid = vec![
            CpuidEntry::new(0x0, None, [0xd, 0x756e_6547, 0x6c65_746e, 0x4965_6e69]),
            // The initial APIC ID differs between runs.
            CpuidEntry::new(
                0x1,
                None,
                [
                    0x0005_0654,
                    if cpuid_order_reversed {
                        0x1f00_0800
                    } else {
                        0x0300_0800
                    },
                    1 << 24,
                    0,
                ],
            ),
            CpuidEntry::new(0x7, Some(0), [0, 0x2, 0, 0]),
            CpuidEntry::new(0xd, Some(0), [0x7, 0x240, 0x340, 0]),
            CpuidEntry::new(0xd, Some(1), [0x1, 0x240, 0, 0]),
            CpuidEntry::new(0xd, Some(2), [0x100, 0x240, 0, 0]),
            CpuidEntry::new(0x8000_0000, None, [0x8000_0007, 0, 0, 0]),
            CpuidEntry::new(0x8000_0007, None, [0, 0, 0, 1 << 8]),
        ];
        if cpuid_order_reversed {
            cpuid.reverse();
        }
        let mut backend = BackendFingerprint::new("kvm", "KVM_GET_SUPPORTED_CPUID", cpuid);
        backend.set_feature_msrs([
            (IA32_ARCH_CAPABILITIES, 0x2b),
            (0x8b, 0x0200_7006_0000_0000),
        ]);
        backend.set_supported_msrs([0x3b, 0x10, 0x3b]);
        backend.set_value("kvm.api_version", 12);
        backend.time.tsc_frequency_hz = Some(2_194_843_000);
        backend
            .time
            .msr_intercepts
            .insert("KVM_CAP_X86_MSR_FILTER".to_owned(), true);
        backend
    }

    fn fingerprint() -> CpuFingerprint {
        CpuFingerprint::new(
            ToolIdentity {
                name: "openvmm".to_owned(),
                version: "0.2.0".to_owned(),
            },
            host(),
            backend(false),
        )
    }

    #[test]
    fn derives_backend_sections_from_cpuid() {
        let backend = backend(false);
        assert_eq!(backend.xsave.xcr0_supported, Hex64(0x7));
        assert_eq!(backend.xsave.standard_size, 0x340);
        assert_eq!(backend.xsave.compacted_size, 576 + 0x100);
        assert_eq!(backend.xsave.max_standard_size, 0x340);
        assert!(backend.time.invariant_tsc);
        assert!(backend.time.tsc_deadline);
        assert!(backend.time.tsc_adjust);
        assert!(!backend.time.rdtscp);
        assert_eq!(backend.msrs.arch_capabilities, Some(Hex64(0x2b)));
        assert_eq!(backend.msrs.feature_msrs[0].index, Hex32(0x8b));
        assert_eq!(backend.msrs.supported, [Hex32(0x10), Hex32(0x3b)]);
    }

    #[test]
    fn output_is_deterministic() {
        let left = CpuFingerprint::new(fingerprint().tool, host(), backend(false));
        let right = CpuFingerprint::new(fingerprint().tool, host(), backend(true));
        assert_eq!(left.to_json(), right.to_json());
        assert_eq!(left.surface_digest, right.surface_digest);
        assert!(left.digest.starts_with("sha256:"));
        assert_eq!(left.digest.len(), "sha256:".len() + 64);
    }

    #[test]
    fn round_trips_and_verifies() {
        let fingerprint = fingerprint();
        let json = fingerprint.to_json();
        let parsed = CpuFingerprint::from_json(&json).unwrap();
        assert_eq!(parsed, fingerprint);
        assert_eq!(parsed.to_json(), json);
    }

    #[test]
    fn surface_digest_ignores_rates_and_versions() {
        let mut other = backend(false);
        other.time.tsc_frequency_hz = Some(2_194_844_000);
        other.set_value("kvm.api_version", 13);
        let mut other_host = host();
        other_host.os.release = Some("6.6.150".to_owned());
        let other = CpuFingerprint::new(fingerprint().tool, other_host, other);
        let fingerprint = fingerprint();
        assert_eq!(other.surface_digest, fingerprint.surface_digest);
        assert_ne!(other.digest, fingerprint.digest);

        let mut changed = backend(false);
        changed.set_feature_bank("bank0", 1);
        assert_ne!(changed.surface_digest(), backend(false).surface_digest());
    }

    #[test]
    fn rejects_tampering_and_other_schemas() {
        let json = fingerprint().to_json();

        let tampered = json.replace("\"0x000000000000002b\"", "\"0x000000000000002f\"");
        assert_ne!(tampered, json);
        assert!(matches!(
            CpuFingerprint::from_json(&tampered),
            Err(FingerprintError::Digest {
                field: "surface_digest",
                ..
            })
        ));

        let tampered = json.replace("7.0.0-30-generic", "7.0.0-31-generic");
        assert!(matches!(
            CpuFingerprint::from_json(&tampered),
            Err(FingerprintError::Digest {
                field: "digest",
                ..
            })
        ));

        let other_schema = json.replace(SCHEMA, "openvmm-cpu-fingerprint/v2");
        assert!(matches!(
            CpuFingerprint::from_json(&other_schema),
            Err(FingerprintError::Schema(schema)) if schema == "openvmm-cpu-fingerprint/v2"
        ));
        assert!(matches!(
            CpuFingerprint::from_json("[]"),
            Err(FingerprintError::Schema(_))
        ));
        let unknown_field = json.replacen('{', "{\"extra\": 1,", 1);
        assert!(matches!(
            CpuFingerprint::from_json(&unknown_field),
            Err(FingerprintError::Json(_))
        ));
    }

    #[test]
    fn records_failed_queries() {
        let mut backend = backend(false);
        let error = std::io::Error::other("not supported");
        assert_eq!(backend.record("a", Ok::<_, std::io::Error>(5)), Some(5));
        assert_eq!(backend.record("b", Err::<u64, _>(error)), None);
        assert_eq!(backend.values["a"], Hex64(5));
        assert_eq!(backend.unavailable["b"], "not supported");
    }
}
