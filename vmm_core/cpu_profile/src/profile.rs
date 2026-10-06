// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pinned CPU profiles: the complete guest-visible CPU surface of one CPU
//! generation.
//!
//! A [`CpuProfile`] lists every CPUID leaf and subleaf a guest can observe in
//! the basic and extended ranges, with exact values and a mask of the bits it
//! pins; the XSAVE features and layout; the guest physical address width; and
//! pinned MSR values. The bits it does not pin are the ones OpenVMM sets per
//! VM, which [`vm_owned_bits`] and [`runtime_owned_bits`] define, so a VM's
//! effective CPUID is a pure function of the profile, its topology, and the
//! time ABI's identity leaves.
//!
//! The canonical encoding is compact canonical JSON (object keys sorted by
//! their UTF-8 bytes, no whitespace), and the profile digest is its SHA-256.
//! Pinned profiles are stored as pretty canonical JSON. Decoding accepts only
//! the canonical bytes, so a profile has exactly one encoding and one digest.
//!
//! Every serialized struct declares its fields in the byte order of their
//! keys, so serializing it directly gives the canonical form (tests check
//! this against the sorting encoder). Keep new fields in that order.

use crate::Hex32;
use crate::Hex64;
use crate::canonical;
use crate::cpuid;
use crate::cpuid::CpuidEntry;
use crate::cpuid::EXTENDED_LEAF_BASE;
use crate::cpuid::XsaveComponent;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::fingerprint::IA32_ARCH_CAPABILITIES;
use crate::signature::HostCpuSignature;
use crate::signature::decode_signature;
use crate::vendor::CpuVendor;
use serde::Deserialize;
use serde::Serialize;

/// The schema of the profile documents this crate reads and writes.
pub const SCHEMA: &str = "openvmm-cpu-profile/v1";

/// The leaves that OpenVMM synthesizes entirely from the VM's topology, which
/// profiles therefore omit.
pub const VM_OWNED_LEAVES: [u32; 2] = [0xb, 0x1f];

/// The leaves whose subleaf 0 reports the highest subleaf in EAX.
const MAX_SUBLEAF_IN_EAX_LEAVES: [u32; 6] = [0x7, 0x14, 0x17, 0x18, 0x1d, 0x24];

/// `CPUID.(7,0):EDX[29]`, the presence of `IA32_ARCH_CAPABILITIES`.
const LEAF7_EDX_ARCH_CAPABILITIES: u32 = 1 << 29;

/// The CPU generation that a profile serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    /// The CPU models of the generation.
    pub cpus: Vec<GenerationCpu>,
    /// The generation name used in logs and reports, such as `icelake-sp`.
    pub name: String,
}

impl Generation {
    /// Returns whether the host CPU, of the profile's `vendor`, belongs to
    /// the generation.
    pub fn contains(&self, vendor: &str, host: &HostCpuSignature) -> bool {
        host.vendor().as_slice() == vendor.as_bytes()
            && self.cpus.iter().any(|cpu| {
                let (family, model, stepping) =
                    decode_signature(vendor.as_bytes(), host.signature());
                cpu.family == family
                    && cpu.model == model
                    && (cpu.steppings[0]..=cpu.steppings[1]).contains(&stepping)
            })
    }
}

/// A CPU model of a generation: a display family and model, and the range of
/// steppings it covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationCpu {
    /// The display family.
    pub family: u32,
    /// The display model.
    pub model: u32,
    /// The first and last stepping, inclusive. Some model numbers span
    /// generations: family 6 model 85 is Skylake-SP up to stepping 4, Cascade
    /// Lake at steppings 5 to 7, and Cooper Lake at steppings 10 and 11.
    pub steppings: [u32; 2],
}

/// One CPUID leaf, or one subleaf of an indexed leaf, with its values and the
/// mask of the bits they define.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuidLeafValue {
    /// The leaf, the input value of `EAX`.
    pub leaf: Hex32,
    /// The bits of each output register that the values define.
    pub mask: [Hex32; 4],
    /// The subleaf, the input value of `ECX`, or `None` for a leaf whose
    /// output does not depend on `ECX`.
    pub subleaf: Option<Hex32>,
    /// The output values of `EAX`, `EBX`, `ECX`, and `EDX`. Bits outside the
    /// mask are zero.
    pub value: [Hex32; 4],
}

impl CpuidLeafValue {
    pub(crate) fn new(leaf: u32, subleaf: Option<u32>, value: [u32; 4], mask: [u32; 4]) -> Self {
        Self {
            leaf: Hex32(leaf),
            subleaf: subleaf.map(Hex32),
            value: value.map(Hex32),
            mask: mask.map(Hex32),
        }
    }

    /// Returns the sort key: the leaf, then the subleaf, with a
    /// subleaf-independent entry first.
    pub fn key(&self) -> (u32, Option<u32>) {
        (self.leaf.0, self.subleaf.map(|subleaf| subleaf.0))
    }

    /// Returns the output values.
    pub fn values(&self) -> [u32; 4] {
        self.value.map(|value| value.0)
    }

    /// Returns the masks.
    pub fn masks(&self) -> [u32; 4] {
        self.mask.map(|mask| mask.0)
    }
}

/// An MSR value that a profile pins: the guest reads `value` in the bits of
/// `mask`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedMsr {
    /// The MSR index.
    pub index: Hex32,
    /// The bits the profile pins. The backend defines the others.
    pub mask: Hex64,
    /// The pinned value. Bits outside the mask are zero.
    pub value: Hex64,
}

/// Where a profile came from. Informative; part of the digest like every
/// other field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// How the profile was derived.
    pub method: String,
    /// The host fingerprints it was derived from, by backend.
    pub sources: Vec<ProvenanceSource>,
}

/// The fingerprints of one backend that a profile was derived from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceSource {
    /// The backend: `kvm`, `mshv`, or `whp`.
    pub backend: String,
    /// The number of host fingerprints.
    pub hosts: u32,
    /// The distinct surface digests of those fingerprints, sorted.
    pub surface_digests: Vec<String>,
}

/// A CPU profile: the complete guest-visible CPU surface of one CPU
/// generation, shared by every backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuProfile {
    pub(crate) cpuid: Vec<CpuidLeafValue>,
    pub(crate) description: String,
    pub(crate) generation: Generation,
    pub(crate) id: String,
    pub(crate) msrs: Vec<PinnedMsr>,
    pub(crate) physical_address_width: u32,
    pub(crate) provenance: Provenance,
    pub(crate) schema: String,
    pub(crate) vendor: String,
    pub(crate) xcr0: Hex64,
    pub(crate) xsave_components: Vec<XsaveComponent>,
    pub(crate) xss: Hex64,
}

impl CpuProfile {
    /// Returns the profile ID, `<vendor>.<generation>.v<revision>`, such as
    /// `intel.icelake-sp.v1`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the human-readable description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the 12-character CPUID vendor string, such as `GenuineIntel`.
    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    /// Returns the vendor of the profile's CPUs, which decides the layout of
    /// the fields that OpenVMM sets per VM ([`vm_owned_bits`]).
    pub fn cpu_vendor(&self) -> CpuVendor {
        // Validation accepts only the vendors that profiles serve, and a
        // pinned profile's vendor is its known generation's, which the
        // catalog's tests check.
        CpuVendor::from_cpuid_vendor(self.vendor.as_bytes())
            .expect("a valid profile has a vendor that profiles serve")
    }

    /// Returns the CPU generation the profile serves.
    pub fn generation(&self) -> &Generation {
        &self.generation
    }

    /// Returns the CPUID leaves, sorted by leaf and subleaf.
    pub fn cpuid(&self) -> &[CpuidLeafValue] {
        &self.cpuid
    }

    /// Returns the output of CPUID for `leaf` and `subleaf`: the profile
    /// values, which are zero in the bits OpenVMM sets per VM, or zeros for a
    /// leaf or subleaf the profile does not list.
    pub fn lookup(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
        find(&self.cpuid, leaf, subleaf).map_or([0; 4], CpuidLeafValue::values)
    }

    /// Returns the XCR0 bits the guest may enable.
    pub fn xcr0(&self) -> u64 {
        self.xcr0.0
    }

    /// Returns the IA32_XSS bits the guest may enable.
    pub fn xss(&self) -> u64 {
        self.xss.0
    }

    /// Returns the layout of every enabled XSAVE state component beyond x87
    /// and SSE.
    pub fn xsave_components(&self) -> &[XsaveComponent] {
        &self.xsave_components
    }

    /// Returns the guest physical address width in bits.
    pub fn physical_address_width(&self) -> u8 {
        self.physical_address_width as u8
    }

    /// Returns the pinned MSR values, sorted by index.
    pub fn msrs(&self) -> &[PinnedMsr] {
        &self.msrs
    }

    /// Returns the pinned value and mask of MSR `index`, if the profile pins
    /// it.
    pub fn msr(&self, index: u32) -> Option<(u64, u64)> {
        self.msrs
            .iter()
            .find(|msr| msr.index.0 == index)
            .map(|msr| (msr.value.0, msr.mask.0))
    }

    /// Returns where the profile came from.
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Returns the canonical encoding: compact canonical JSON.
    ///
    /// Offline only, for tools and tests: a pinned profile's encoding is the
    /// constant [`pinned_record`](crate::pinned_record) returns.
    pub fn encode(&self) -> Vec<u8> {
        canonical::to_compact_ordered(self)
    }

    /// Returns the profile as pretty canonical JSON, with a final newline:
    /// the form in which profiles are pinned. Offline only.
    pub fn to_pretty_json(&self) -> String {
        canonical::to_pretty_ordered(self)
    }

    /// Returns the profile digest: the SHA-256 of [`Self::encode`].
    ///
    /// Offline only, for tools and tests: a pinned profile's digest is the
    /// constant [`pinned_record`](crate::pinned_record) returns.
    pub fn digest(&self) -> [u8; 32] {
        canonical::sha256(&self.encode())
    }

    /// Returns the profile digest as `sha256:<hex>`. Offline only, for
    /// `--cpu-fingerprint` and tools.
    pub fn digest_string(&self) -> String {
        canonical::format_digest(&self.digest())
    }

    /// Decodes a profile from its canonical encoding and validates it.
    ///
    /// Bytes that are not exactly the canonical encoding of a valid profile
    /// fail with `E_PROFILE_DIGEST`: they cannot carry a verifiable digest.
    /// Offline only: a restore compares the recorded document with the
    /// pinned constant instead of decoding it.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProfileError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| invalid("the encoding is not UTF-8".to_owned()))?;
        let this = Self::parse(text)?;
        if this.encode() != bytes {
            return Err(invalid(format!(
                "profile {} is not in its canonical encoding",
                this.id
            )));
        }
        Ok(this)
    }

    /// Parses a profile from pretty canonical JSON, the pinned form, and
    /// validates it. Offline only, for the profile generator and tests.
    pub fn from_pretty_json(text: &str) -> Result<Self, ProfileError> {
        let this = Self::parse(text)?;
        if this.to_pretty_json() != text {
            return Err(invalid(format!(
                "profile {} is not in pretty canonical JSON",
                this.id
            )));
        }
        Ok(this)
    }

    fn parse(text: &str) -> Result<Self, ProfileError> {
        let schema_error = |schema: Option<&str>| {
            invalid(format!(
                "unsupported profile schema {:?}, expected {SCHEMA:?}",
                schema.unwrap_or_default()
            ))
        };
        let this: Self = match serde_json::from_str(text) {
            Ok(this) => this,
            Err(error) => {
                // Name an unsupported schema rather than the field that a
                // document of another schema lacks.
                let value: serde_json::Value = serde_json::from_str(text)
                    .map_err(|error| invalid(format!("malformed profile: {error}")))?;
                let schema = value.get("schema").and_then(|schema| schema.as_str());
                if schema != Some(SCHEMA) {
                    return Err(schema_error(schema));
                }
                return Err(invalid(format!("malformed profile: {error}")));
            }
        };
        if this.schema != SCHEMA {
            return Err(schema_error(Some(&this.schema)));
        }
        this.validate()
            .map_err(|message| invalid(format!("profile {:?} is invalid: {message}", this.id)))?;
        Ok(this)
    }

    /// Assembles a profile from its parts and validates it.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        id: String,
        description: String,
        vendor: String,
        generation: Generation,
        mut leaves: Vec<CpuidLeafValue>,
        xcr0: u64,
        xss: u64,
        physical_address_width: u8,
        msrs: Vec<PinnedMsr>,
        provenance: Provenance,
    ) -> Result<Self, String> {
        leaves.sort_by_key(CpuidLeafValue::key);
        let entries = leaves
            .iter()
            .map(|entry| CpuidEntry::new(entry.leaf.0, entry.subleaf.map(|s| s.0), entry.values()))
            .collect::<Vec<_>>();
        let this = Self {
            schema: SCHEMA.to_owned(),
            id,
            description,
            vendor,
            generation,
            xsave_components: cpuid::xsave_components(&entries),
            cpuid: leaves,
            xcr0: Hex64(xcr0),
            xss: Hex64(xss),
            physical_address_width: physical_address_width.into(),
            msrs,
            provenance,
        };
        this.validate()?;
        Ok(this)
    }

    /// Checks every structural invariant of a profile; see the module
    /// documentation and [`SCHEMA`].
    fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!("schema {:?} is not {SCHEMA:?}", self.schema));
        }
        let vendor = self.validate_identity()?;
        self.validate_cpuid(vendor)?;
        self.validate_xsave()?;
        self.validate_msrs()?;
        let width = self.lookup(EXTENDED_LEAF_BASE + 8, 0)[0] & 0xff;
        if self.physical_address_width != width {
            return Err(format!(
                "physical address width {} differs from CPUID 0x80000008 EAX[7:0] {width}",
                self.physical_address_width
            ));
        }
        Ok(())
    }

    /// Checks the profile's identity and returns its vendor.
    fn validate_identity(&self) -> Result<CpuVendor, String> {
        let Some(cpu_vendor) = CpuVendor::from_cpuid_vendor(self.vendor.as_bytes()) else {
            return Err(format!("unsupported vendor {:?}", self.vendor));
        };
        let vendor_short = cpu_vendor.id();
        let name = &self.generation.name;
        if !is_id_component(name) {
            return Err(format!("invalid generation name {name:?}"));
        }
        let revision = self
            .id
            .strip_prefix(vendor_short)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_prefix(name.as_str()))
            .and_then(|rest| rest.strip_prefix(".v"))
            .ok_or_else(|| format!("ID {:?} is not {vendor_short}.{name}.v<revision>", self.id))?;
        if !is_revision(revision) {
            return Err(format!("ID {:?} has an invalid revision", self.id));
        }
        if self.generation.cpus.is_empty() {
            return Err("the generation lists no CPU".to_owned());
        }
        for cpu in &self.generation.cpus {
            let [first, last] = cpu.steppings;
            if first > last || last > 0xf {
                return Err(format!(
                    "invalid stepping range {first}..={last} for family {} model {}",
                    cpu.family, cpu.model
                ));
            }
        }
        let [_, ebx, ecx, edx] = self.lookup(0, 0);
        let vendor = crate::signature::vendor_bytes(ebx, edx, ecx);
        if vendor.as_slice() != self.vendor.as_bytes() {
            return Err(format!(
                "CPUID leaf 0 reports vendor {:?}, not {:?}",
                String::from_utf8_lossy(&vendor),
                self.vendor
            ));
        }
        let signature = HostCpuSignature::new(vendor, self.lookup(1, 0)[0]);
        if !self.generation.contains(&self.vendor, &signature) {
            return Err(format!(
                "CPUID.1:EAX {:#x} is not in generation {name}",
                signature.signature()
            ));
        }
        Ok(cpu_vendor)
    }

    fn validate_cpuid(&self, vendor: CpuVendor) -> Result<(), String> {
        for pair in self.cpuid.windows(2) {
            if pair[0].key() >= pair[1].key() {
                return Err(format!(
                    "CPUID entries are not strictly sorted at {}",
                    describe(&pair[1])
                ));
            }
        }
        let max_basic = self.lookup(0, 0)[0];
        let max_extended = self.lookup(EXTENDED_LEAF_BASE, 0)[0];
        if !(1..cpuid::HYPERVISOR_LEAF_BASE).contains(&max_basic) {
            return Err(format!("invalid maximum basic leaf {max_basic:#x}"));
        }
        if !(EXTENDED_LEAF_BASE..EXTENDED_LEAF_BASE + 0x100).contains(&max_extended) {
            return Err(format!("invalid maximum extended leaf {max_extended:#x}"));
        }
        for entry in &self.cpuid {
            let leaf = entry.leaf.0;
            let in_range = leaf <= max_basic || (EXTENDED_LEAF_BASE..=max_extended).contains(&leaf);
            if !in_range || VM_OWNED_LEAVES.contains(&leaf) {
                return Err(format!(
                    "{} is outside the profile's leaves",
                    describe(entry)
                ));
            }
            if entry.subleaf.is_some() != cpuid::is_indexed_leaf(leaf) {
                return Err(format!(
                    "{} has the wrong subleaf indexing",
                    describe(entry)
                ));
            }
            let value = entry.values();
            let expected_mask = pinned_mask(vendor, leaf, entry.subleaf.map(|s| s.0), value);
            if entry.masks() != expected_mask {
                return Err(format!(
                    "{} has mask {:#x?}, expected {expected_mask:#x?}",
                    describe(entry),
                    entry.masks()
                ));
            }
            if value
                .iter()
                .zip(expected_mask)
                .any(|(value, mask)| value & !mask != 0)
            {
                return Err(format!("{} sets bits outside its mask", describe(entry)));
            }
        }
        let leaves = (0..=max_basic).chain(EXTENDED_LEAF_BASE..=max_extended);
        for leaf in leaves.filter(|leaf| !VM_OWNED_LEAVES.contains(leaf)) {
            let subleaves = self
                .cpuid
                .iter()
                .filter(|entry| entry.leaf.0 == leaf)
                .map(|entry| entry.subleaf.map(|s| s.0))
                .collect::<Vec<_>>();
            let expected = self.expected_subleaves(leaf);
            if subleaves != expected {
                return Err(format!(
                    "CPUID {leaf:#x} lists subleaves {subleaves:?}, expected {expected:?}"
                ));
            }
        }
        Ok(())
    }

    /// Returns the subleaves a dense profile lists for `leaf`: `[None]` for a
    /// leaf that is not indexed, and otherwise the subleaves the architecture
    /// enumerates from the profile's own values.
    fn expected_subleaves(&self, leaf: u32) -> Vec<Option<u32>> {
        if !cpuid::is_indexed_leaf(leaf) {
            return vec![None];
        }
        let last = match leaf {
            // Up to and including the first subleaf with a null cache type:
            // Intel's deterministic cache parameters, and AMD's cache
            // topology.
            0x4 | 0x8000_001d => (0..0x40)
                .find(|&subleaf| self.lookup(leaf, subleaf)[0] & 0x1f == 0)
                .unwrap_or(0x3f),
            0xd => {
                let components = (self.xcr0.0 | self.xss.0) & !3;
                return [0, 1]
                    .into_iter()
                    .chain((2..63).filter(|index| components & (1 << index) != 0))
                    .map(Some)
                    .collect();
            }
            leaf if MAX_SUBLEAF_IN_EAX_LEAVES.contains(&leaf) => self.lookup(leaf, 0)[0].min(0x3f),
            _ => 0,
        };
        (0..=last).map(Some).collect()
    }

    fn validate_xsave(&self) -> Result<(), String> {
        let xcr0 = self.xcr0.0;
        let xss = self.xss.0;
        if xcr0 & 3 != 3 {
            return Err(format!("XCR0 {xcr0:#x} lacks x87 or SSE"));
        }
        if xcr0 & xss != 0 {
            return Err(format!("XCR0 {xcr0:#x} and XSS {xss:#x} overlap"));
        }
        let entries = self
            .cpuid
            .iter()
            .map(|entry| CpuidEntry::new(entry.leaf.0, entry.subleaf.map(|s| s.0), entry.values()))
            .collect::<Vec<_>>();
        if cpuid::xsave_supported(&entries) != (xcr0, xss) {
            return Err("XCR0 and XSS differ from CPUID leaf 0xd".to_owned());
        }
        let components = cpuid::xsave_components(&entries);
        if components != self.xsave_components {
            return Err("the XSAVE components differ from CPUID leaf 0xd".to_owned());
        }
        if components.len() != ((xcr0 | xss) & !3).count_ones() as usize {
            return Err("an enabled XSAVE component has no CPUID leaf 0xd subleaf".to_owned());
        }
        if components
            .iter()
            .any(|component| component.supervisor != (xss & (1 << component.index) != 0))
        {
            return Err("an XSAVE component's supervisor flag disagrees with XSS".to_owned());
        }
        let standard_size = cpuid::xsave_standard_size(&components, xcr0);
        if self.lookup(0xd, 0)[2] != standard_size {
            return Err(format!(
                "CPUID.(0xd,0):ECX is not the standard XSAVE size {standard_size}"
            ));
        }
        Ok(())
    }

    fn validate_msrs(&self) -> Result<(), String> {
        for pair in self.msrs.windows(2) {
            if pair[0].index.0 >= pair[1].index.0 {
                return Err("pinned MSRs are not strictly sorted".to_owned());
            }
        }
        for msr in &self.msrs {
            if msr.mask.0 == 0 || msr.value.0 & !msr.mask.0 != 0 {
                return Err(format!(
                    "MSR {:#x} pins {:#x} under mask {:#x}",
                    msr.index.0, msr.value.0, msr.mask.0
                ));
            }
            if msr.index.0 == IA32_ARCH_CAPABILITIES
                && self.lookup(7, 0)[3] & LEAF7_EDX_ARCH_CAPABILITIES == 0
            {
                return Err("IA32_ARCH_CAPABILITIES is pinned but not enumerated".to_owned());
            }
        }
        Ok(())
    }
}

/// Returns whether `component` is a valid vendor or generation component of a
/// profile ID: one or more lowercase ASCII letters, digits, and hyphens.
pub(crate) fn is_id_component(component: &str) -> bool {
    !component.is_empty()
        && component
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Returns whether `revision` is a valid profile revision, the decimal number
/// after the `.v` that ends a profile ID: one or more digits without a leading
/// zero.
pub(crate) fn is_revision(revision: &str) -> bool {
    !revision.is_empty()
        && !revision.starts_with('0')
        && revision.bytes().all(|byte| byte.is_ascii_digit())
}

/// Returns the entry of `entries` that CPUID `leaf` and `subleaf` read, if any.
pub(crate) fn find(entries: &[CpuidLeafValue], leaf: u32, subleaf: u32) -> Option<&CpuidLeafValue> {
    entries.iter().find(|entry| {
        entry.leaf.0 == leaf
            && entry
                .subleaf
                .is_none_or(|entry_subleaf| entry_subleaf.0 == subleaf)
    })
}

/// Returns the bits of CPUID `leaf` that OpenVMM sets from the VM
/// configuration, given the leaf's output `value`, in a profile of `vendor`'s
/// CPUs:
///
/// - `CPUID.1:EBX[31:16]`, the logical processor count and initial APIC ID,
///   and `CPUID.1:ECX[21]`, x2APIC, which the APIC mode decides;
/// - every bit of the topology leaves in [`VM_OWNED_LEAVES`];
/// - on Intel, `CPUID.4:EAX[31:14]`, the core and cache-sharing counts, in
///   every subleaf that describes a cache; and
/// - on AMD, the core count and APIC ID size, `CPUID.0x80000008:ECX[7:0]` and
///   `ECX[15:12]`; the cache-sharing count, `CPUID.0x8000001D:EAX[25:14]`, in
///   every subleaf that describes a cache; and the processor topology of
///   `CPUID.0x8000001E`: the extended APIC ID in `EAX`, the compute unit ID
///   and threads per compute unit in `EBX[15:0]`, and the node ID and nodes
///   per processor in `ECX[10:0]`.
///
/// Each vendor's fields are VM-owned only in its own profiles: the Intel
/// profiles pin AMD's topology fields, which Intel CPUs leave zero.
pub fn vm_owned_bits(vendor: CpuVendor, leaf: u32, value: [u32; 4]) -> [u32; 4] {
    let describes_cache = value[0] & 0x1f != 0;
    match (vendor, leaf) {
        (_, 0x1) => [0, 0xffff_0000, 1 << 21, 0],
        (_, leaf) if VM_OWNED_LEAVES.contains(&leaf) => [!0; 4],
        (CpuVendor::Intel, 0x4) if describes_cache => [0xffff_c000, 0, 0, 0],
        (CpuVendor::Amd, 0x8000_0008) => [0, 0, 0xf0ff, 0],
        (CpuVendor::Amd, 0x8000_001d) if describes_cache => [0x03ff_c000, 0, 0, 0],
        (CpuVendor::Amd, 0x8000_001e) => [!0, 0xffff, 0x7ff, 0],
        _ => [0; 4],
    }
}

/// Returns the bits of CPUID `leaf` and `subleaf` that reflect runtime state
/// rather than the CPU surface: OSXSAVE and OSPKE, which mirror control
/// register bits, and the XSAVE area sizes for the currently enabled XCR0 and
/// IA32_XSS.
pub fn runtime_owned_bits(leaf: u32, subleaf: Option<u32>) -> [u32; 4] {
    match (leaf, subleaf) {
        (0x1, _) => [0, 0, 1 << 27, 0],
        (0x7, Some(0)) => [0, 0, 1 << 4, 0],
        (0xd, Some(0 | 1)) => [0, !0, 0, 0],
        _ => [0; 4],
    }
}

/// Returns the bits of CPUID `leaf` and `subleaf`, with output `value`, that
/// a profile of `vendor`'s CPUs pins: every bit that is neither VM-owned nor
/// runtime-owned.
pub fn pinned_mask(
    vendor: CpuVendor,
    leaf: u32,
    subleaf: Option<u32>,
    value: [u32; 4],
) -> [u32; 4] {
    let vm = vm_owned_bits(vendor, leaf, value);
    let runtime = runtime_owned_bits(leaf, subleaf);
    [0, 1, 2, 3].map(|register| !(vm[register] | runtime[register]))
}

pub(crate) fn describe(entry: &CpuidLeafValue) -> String {
    describe_leaf(entry.leaf.0, entry.subleaf.map(|s| s.0))
}

pub(crate) fn describe_leaf(leaf: u32, subleaf: Option<u32>) -> String {
    match subleaf {
        Some(subleaf) => format!("CPUID {leaf:#x}.{subleaf}"),
        None => format!("CPUID {leaf:#x}"),
    }
}

fn invalid(message: String) -> ProfileError {
    ProfileError::new(ProfileErrorCode::ProfileDigest, message)
}

#[cfg(test)]
fn to_value<T: Serialize>(value: &T) -> serde_json::Value {
    // Every field serializes infallibly with string map keys.
    serde_json::to_value(value).expect("profile serialization is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::profile;
    use serde_json::Value;
    use serde_json::json;
    use test_with_tracing::test;

    const ICELAKE: &str = "intel.icelake-sp.v1";

    /// Decodes `profile` after `edit` changes its JSON form.
    fn decode_edited(edit: impl FnOnce(&mut Value)) -> Result<CpuProfile, ProfileError> {
        let mut value = to_value(profile(ICELAKE));
        edit(&mut value);
        CpuProfile::decode(canonical::to_compact(&value).as_bytes())
    }

    /// Returns the CPUID entry of `leaf` and `subleaf` in a profile's JSON.
    fn entry(value: &mut Value, leaf: u32, subleaf: Option<u32>) -> &mut Value {
        value["cpuid"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| {
                entry["leaf"] == json!(format!("{leaf:#010x}"))
                    && entry["subleaf"]
                        == subleaf.map_or(Value::Null, |s| json!(format!("{s:#010x}")))
            })
            .unwrap()
    }

    fn assert_invalid(result: Result<CpuProfile, ProfileError>, expected: &str) {
        let error = result.unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileDigest, "{error}");
        assert!(error.message.contains(expected), "{error}");
    }

    #[test]
    fn pinned_profiles_round_trip_through_their_canonical_encoding() {
        for profile in crate::pinned_profiles() {
            let encoded = profile.encode();
            let decoded = CpuProfile::decode(&encoded).unwrap();
            assert_eq!(&decoded, profile);
            assert_eq!(decoded.digest(), canonical::sha256(&encoded));
            assert_eq!(
                CpuProfile::from_pretty_json(&profile.to_pretty_json()).unwrap(),
                decoded
            );
        }
    }

    /// The direct serialization that `encode` and `to_pretty_json` use is
    /// canonical only while every struct declares its fields in key order.
    #[test]
    fn direct_serialization_matches_the_sorting_encoder() {
        for profile in crate::pinned_profiles() {
            let value = to_value(profile);
            assert_eq!(
                String::from_utf8(profile.encode()).unwrap(),
                canonical::to_compact(&value),
                "{}",
                profile.id()
            );
            assert_eq!(
                profile.to_pretty_json(),
                canonical::to_pretty(&value),
                "{}",
                profile.id()
            );
        }
        // Fields that the pinned profiles leave at one value.
        let component = XsaveComponent {
            align64: true,
            index: 12,
            offset: 0,
            size: 24,
            supervisor: true,
            xfd: true,
        };
        assert_eq!(
            canonical::to_compact_ordered(&component),
            canonical::to_compact(&to_value(&component)).into_bytes()
        );
    }

    #[test]
    fn decoding_rejects_every_non_canonical_form() {
        let profile = profile(ICELAKE);
        let mut spaced = profile.encode();
        spaced.insert(1, b' ');
        assert_invalid(CpuProfile::decode(&spaced), "canonical");
        assert_invalid(
            CpuProfile::decode(profile.to_pretty_json().as_bytes()),
            "canonical",
        );
        assert_invalid(
            CpuProfile::from_pretty_json(&String::from_utf8(profile.encode()).unwrap()),
            "pretty canonical",
        );
        assert_invalid(CpuProfile::decode(b"\xff"), "UTF-8");
        assert_invalid(CpuProfile::decode(b"{}"), "schema");
        assert_invalid(
            decode_edited(|value| value["extra"] = json!(1)),
            "unknown field",
        );
    }

    #[test]
    fn validation_rejects_malformed_identities() {
        assert_invalid(
            decode_edited(|value| value["id"] = json!("intel.icelake-sp.v01")),
            "invalid revision",
        );
        assert_invalid(
            decode_edited(|value| value["id"] = json!("intel.skylake-sp.v1")),
            "is not intel.icelake-sp.v<revision>",
        );
        assert_invalid(
            decode_edited(|value| value["vendor"] = json!("HygonGenuine")),
            "unsupported vendor",
        );
        // An AMD vendor on an Intel profile's ID and CPUID.
        assert_invalid(
            decode_edited(|value| value["vendor"] = json!("AuthenticAMD")),
            "is not amd.icelake-sp.v<revision>",
        );
        assert_invalid(
            decode_edited(|value| {
                value["vendor"] = json!("AuthenticAMD");
                value["id"] = json!("amd.icelake-sp.v1");
            }),
            "CPUID leaf 0 reports vendor \"GenuineIntel\", not \"AuthenticAMD\"",
        );
        assert_invalid(
            decode_edited(|value| value["generation"]["cpus"][0]["model"] = json!(85)),
            "is not in generation icelake-sp",
        );
        assert_invalid(
            decode_edited(|value| value["generation"]["cpus"][0]["steppings"] = json!([4, 3])),
            "invalid stepping range",
        );
    }

    #[test]
    fn validation_rejects_malformed_cpuid_tables() {
        // A VM-owned field that the profile pins.
        assert_invalid(
            decode_edited(|value| entry(value, 1, None)["mask"][1] = json!("0xffffffff")),
            "has mask",
        );
        // A value outside the mask.
        assert_invalid(
            decode_edited(|value| entry(value, 1, None)["value"][1] = json!("0x00010800")),
            "outside its mask",
        );
        // A missing leaf.
        assert_invalid(
            decode_edited(|value| {
                value["cpuid"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|entry| entry["leaf"] != json!("0x00000003"));
            }),
            "CPUID 0x3 lists subleaves []",
        );
        // A hypervisor leaf, which belongs to the time ABI's identity.
        assert_invalid(
            decode_edited(|value| {
                let cpuid = value["cpuid"].as_array_mut().unwrap();
                let position = cpuid
                    .iter()
                    .position(|entry| entry["leaf"].as_str().unwrap() >= "0x80000000")
                    .unwrap();
                cpuid.insert(
                    position,
                    json!({
                        "leaf": "0x40000000",
                        "subleaf": null,
                        "value": ["0x00000000", "0x00000000", "0x00000000", "0x00000000"],
                        "mask": ["0xffffffff", "0xffffffff", "0xffffffff", "0xffffffff"],
                    }),
                );
            }),
            "outside the profile's leaves",
        );
        // A subleaf beyond leaf 7's maximum.
        assert_invalid(
            decode_edited(|value| entry(value, 7, Some(0))["value"][0] = json!("0x00000001")),
            "CPUID 0x7 lists subleaves",
        );
    }

    #[test]
    fn validation_rejects_inconsistent_xsave_and_msrs() {
        // XCR0 enables components that leaf 0xd does not describe.
        assert_invalid(
            decode_edited(|value| value["xcr0"] = json!("0x00000000000000ff")),
            "CPUID 0xd lists subleaves",
        );
        // Leaf 0xd disagrees with XCR0.
        assert_invalid(
            decode_edited(|value| entry(value, 0xd, Some(0))["value"][0] = json!("0x00000067")),
            "differ from CPUID leaf 0xd",
        );
        assert_invalid(
            decode_edited(|value| value["xsave_components"][0]["offset"] = json!(512)),
            "XSAVE components differ",
        );
        assert_invalid(
            decode_edited(|value| value["physical_address_width"] = json!(52)),
            "physical address width",
        );
        assert_invalid(
            decode_edited(|value| value["msrs"][0]["value"] = json!("0x8000000000000000")),
            "under mask",
        );
        assert_invalid(
            decode_edited(|value| entry(value, 7, Some(0))["value"][3] = json!("0x00000010")),
            "not enumerated",
        );
    }

    #[test]
    fn masks_leave_the_vm_and_runtime_fields_to_openvmm() {
        for vendor in CpuVendor::ALL {
            assert_eq!(
                pinned_mask(vendor, 1, None, [0; 4]),
                [!0, 0x0000_ffff, !(1 << 21 | 1 << 27), !0]
            );
            assert_eq!(
                pinned_mask(vendor, 7, Some(0), [0; 4]),
                [!0, !0, !(1 << 4), !0]
            );
            assert_eq!(pinned_mask(vendor, 0xd, Some(1), [0; 4]), [!0, 0, !0, !0]);
            assert_eq!(pinned_mask(vendor, 0xd, Some(2), [0; 4]), [!0; 4]);
            assert_eq!(pinned_mask(vendor, 0xb, Some(0), [0; 4]), [0; 4]);
        }
        let intel = CpuVendor::Intel;
        assert_eq!(
            pinned_mask(intel, 4, Some(0), [0x121, 0, 0, 0]),
            [0x3fff, !0, !0, !0]
        );
        // The null cache type that ends leaf 4 has no VM fields.
        assert_eq!(pinned_mask(intel, 4, Some(4), [0; 4]), [!0; 4]);
        // Intel profiles pin AMD's topology fields.
        assert_eq!(pinned_mask(intel, 0x8000_0008, None, [0; 4]), [!0; 4]);
        assert_eq!(
            pinned_mask(intel, 0x8000_001d, Some(0), [0x121, 0, 0, 0]),
            [!0; 4]
        );
        assert_eq!(pinned_mask(intel, 0x8000_001e, None, [0; 4]), [!0; 4]);

        // AMD profiles leave AMD's to the VM, and pin leaf 4, which AMD
        // CPUs leave zero.
        let amd = CpuVendor::Amd;
        assert_eq!(pinned_mask(amd, 4, Some(0), [0x121, 0, 0, 0]), [!0; 4]);
        assert_eq!(
            pinned_mask(amd, 0x8000_0008, None, [0; 4]),
            [!0, !0, !0xf0ff, !0]
        );
        assert_eq!(
            pinned_mask(amd, 0x8000_001d, Some(3), [0x163, 0, 0, 0]),
            [0xfc00_3fff, !0, !0, !0]
        );
        assert_eq!(pinned_mask(amd, 0x8000_001d, Some(4), [0; 4]), [!0; 4]);
        assert_eq!(
            pinned_mask(amd, 0x8000_001e, None, [0; 4]),
            [0, 0xffff_0000, 0xffff_f800, !0]
        );
    }

    #[test]
    fn generations_match_by_vendor_family_model_and_stepping() {
        let skylake = profile("intel.skylake-sp.v1").generation();
        let intel = |signature| HostCpuSignature::new(*b"GenuineIntel", signature);
        assert!(skylake.contains("GenuineIntel", &intel(0x0005_0654)));
        // Cascade Lake shares family 6 model 85.
        assert!(!skylake.contains("GenuineIntel", &intel(0x0005_0657)));
        assert!(!skylake.contains(
            "GenuineIntel",
            &HostCpuSignature::new(*b"AuthenticAMD", 0x0005_0654)
        ));
    }
}
