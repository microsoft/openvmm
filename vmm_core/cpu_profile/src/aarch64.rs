// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-independent ARM CPU policy, canonical identity and cold selection.
//! Counter Hz and firmware cache geometry are deliberately not CPU identity.

use crate::ProfileError as Error;
use crate::ProfileErrorCode as Code;
use aarch64defs::SystemReg;
use serde::Deserialize;
use serde::Serialize;
use std::sync::OnceLock;

/// Guest-visible model requirements, excluding native variant and revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CpuModel {
    /// MIDR fields that identify the supported model class.
    pub mask: u64,
    /// Required values under `mask`.
    pub value: u64,
}

impl CpuModel {
    /// Whether a native guest-visible MIDR belongs to this model class.
    pub fn matches(self, midr: u64) -> bool {
        midr & self.mask == self.value
    }
}

struct CatalogEntry {
    vendor: String,
    generation: String,
    host_match: CpuModel,
    profile: CpuProfile,
}

impl CatalogEntry {
    fn host_id(&self) -> String {
        format!("{}.{}.v1", self.vendor, crate::derive::HOST_GENERATION)
    }

    fn supports_host_model(&self, model: CpuModel) -> bool {
        model.mask == self.profile.model.mask
            && model.value & !model.mask == 0
            && self.host_match.matches(model.value)
    }

    fn profile(&self) -> Result<CpuProfile, Error> {
        self.profile.validate()?;
        Ok(self.profile.clone())
    }

    fn value(&self, register: SystemReg) -> Result<u64, Error> {
        self.profile
            .id_registers
            .iter()
            .find(|r| r.register == register)
            .map(|r| r.value)
            .ok_or_else(|| Error::new(Code::ProfileDigest, "incomplete ARM catalog inventory"))
    }

    fn parange(&self) -> Result<u8, Error> {
        Ok((self.value(SystemReg::ID_AA64MMFR0_EL1)? & 0xf) as u8)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelMatch {
    mask: crate::Hex64,
    value: crate::Hex64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDocument {
    vendor: String,
    model_mask: crate::Hex64,
    host_match: ModelMatch,
    sha256: String,
    profile: Document,
}

fn catalog() -> Result<&'static [CatalogEntry], Error> {
    static CATALOG: OnceLock<Result<Vec<CatalogEntry>, Error>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            [
                include_str!("../profiles/qualcomm.oryon-x1.v1.json"),
                include_str!("../profiles/nvidia.n1x.v1.json"),
            ]
            .into_iter()
            .map(load_catalog_entry)
            .collect()
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

fn load_catalog_entry(json: &str) -> Result<CatalogEntry, Error> {
    let record: CatalogDocument = serde_json::from_str(json).map_err(|e| {
        Error::new(
            Code::ProfileDigest,
            format!("invalid ARM catalog record: {e}"),
        )
    })?;
    let doc = record.profile;
    let (generation, revision) = doc
        .id
        .rsplit_once(".v")
        .ok_or_else(|| Error::new(Code::ProfileDigest, "ARM catalog profile has no revision"))?;
    let name = generation
        .strip_prefix(&record.vendor)
        .and_then(|s| s.strip_prefix('.'))
        .ok_or_else(|| Error::new(Code::ProfileDigest, "ARM catalog profile vendor mismatch"))?;
    if !crate::profile::is_id_component(name)
        || name == crate::derive::HOST_GENERATION
        || !crate::profile::is_revision(revision)
    {
        return Err(Error::new(
            Code::ProfileDigest,
            "invalid ARM catalog generation or revision",
        ));
    }
    let generation = generation.to_owned();
    let host_match = CpuModel {
        mask: record.host_match.mask.0,
        value: record.host_match.value.0,
    };
    if doc.schema != "nvx.arm64.cpu.v1"
        || doc.ownership != OWNERSHIP
        || doc.state != STATE_POLICY
        || !crate::profile::is_id_component(&record.vendor)
        || record.model_mask.0 == 0
        || u64::from(doc.guest_model.0) & !record.model_mask.0 != 0
        || host_match.mask == 0
        || host_match.value & !host_match.mask != 0
        || host_match.mask & !record.model_mask.0 != 0
        || !host_match.matches(u64::from(doc.guest_model.0))
    {
        return Err(Error::new(
            Code::ProfileDigest,
            "unsupported ARM catalog metadata",
        ));
    }
    let fields = doc
        .fields
        .iter()
        .map(|field| {
            let register = REGISTERS
                .iter()
                .find(|(_, name)| *name == field.register)
                .ok_or_else(|| Error::new(Code::ProfileDigest, "unknown ARM catalog register"))?
                .0;
            Ok(CpuIdRegister {
                register,
                mask: field.mask.0,
                value: field.value.0,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let profile = CpuProfile {
        id: doc.id,
        sha256: [0; 32],
        model: CpuModel {
            mask: record.model_mask.0,
            value: u64::from(doc.guest_model.0),
        },
        physical_address_bits: doc.physical_address_bits,
        cache_policy: doc.cache,
        id_registers: fields,
    }
    .identify()?;
    if crate::canonical::format_digest(&profile.sha256) != record.sha256 {
        return Err(Error::new(
            Code::ProfileDigest,
            "ARM catalog canonical digest mismatch",
        ));
    }
    Ok(CatalogEntry {
        vendor: record.vendor,
        generation,
        host_match,
        profile,
    })
}

/// Decodes the architectural `ID_AA64MMFR0_EL1.PARange` field.
pub fn physical_address_bits(parange: u8) -> Option<u8> {
    [32, 36, 40, 42, 44, 48, 52, 56]
        .get(usize::from(parange))
        .copied()
}

/// CPU-owned defined fields; the complement is reserved and must remain zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuIdRegister {
    /// Architectural system register, independent of provider identifiers.
    pub register: SystemReg,
    /// Owned architectural fields.
    pub mask: u64,
    /// Required values, zero outside the mask.
    pub value: u64,
}

/// Explicit ownership of cache operating properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    /// Truthful native operating properties; no fixed geometry or sharing.
    NativeInformational,
}

/// Canonical ARM profile and its immutable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuProfile {
    /// Lowercase identity, at most 32 bytes for the existing a8 binding.
    pub id: String,
    /// SHA-256 of the compact canonical document.
    pub sha256: [u8; 32],
    /// Model class required on every retained VP before any VP runs.
    pub model: CpuModel,
    /// Architectural physical address width selected by CPU policy.
    pub physical_address_bits: u8,
    /// Cache field ownership, not firmware geometry.
    pub cache_policy: CachePolicy,
    /// Complete, ordered CPU-ID inventory.
    pub id_registers: Vec<CpuIdRegister>,
}

/// Architectural fingerprint of a native provider, before policy filtering.
/// A host profile is derived from these values, not from the pinned document.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    /// Native guest-visible model; not a claim about a physical host MIDR.
    pub midr: u64,
    /// Native architectural ID values before policy filtering.
    pub registers: Vec<CpuIdRegister>,
    /// Actual native counter frequency, excluded from CPU identity.
    pub counter_hz: u64,
    /// Usable GPA width read from the native probe partition before profile
    /// programming. This backend limit is not architectural PARange or CPU
    /// identity; admission reads back the profile-configured width separately.
    pub backend_usable_gpa_bits: u32,
}

const REGISTERS: [(SystemReg, &str); 15] = [
    (SystemReg::ID_AA64PFR0_EL1, "ID_AA64PFR0_EL1"),
    (SystemReg::ID_AA64PFR1_EL1, "ID_AA64PFR1_EL1"),
    (SystemReg::ID_AA64PFR2_EL1, "ID_AA64PFR2_EL1"),
    (SystemReg::ID_AA64ISAR0_EL1, "ID_AA64ISAR0_EL1"),
    (SystemReg::ID_AA64ISAR1_EL1, "ID_AA64ISAR1_EL1"),
    (SystemReg::ID_AA64ISAR2_EL1, "ID_AA64ISAR2_EL1"),
    (SystemReg::ID_AA64MMFR0_EL1, "ID_AA64MMFR0_EL1"),
    (SystemReg::ID_AA64MMFR1_EL1, "ID_AA64MMFR1_EL1"),
    (SystemReg::ID_AA64MMFR2_EL1, "ID_AA64MMFR2_EL1"),
    (SystemReg::ID_AA64MMFR3_EL1, "ID_AA64MMFR3_EL1"),
    (SystemReg::ID_AA64MMFR4_EL1, "ID_AA64MMFR4_EL1"),
    (SystemReg::ID_AA64DFR0_EL1, "ID_AA64DFR0_EL1"),
    (SystemReg::ID_AA64DFR1_EL1, "ID_AA64DFR1_EL1"),
    (SystemReg::ID_AA64ZFR0_EL1, "ID_AA64ZFR0_EL1"),
    (SystemReg::ID_AA64SMFR0_EL1, "ID_AA64SMFR0_EL1"),
];

const OWNERSHIP: &str = "defined_id_fields=cpu_or_policy;reserved=zero;mpidr_gic=VMM;midr_rv_revidr=native;counter_hz=time";
const STATE_POLICY: &str = "native_fp_simd_pac;no_pmu_sve_sme_mte_nested";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    cache: CachePolicy,
    fields: Vec<Field>,
    guest_model: crate::Hex32,
    id: String,
    ownership: String,
    physical_address_bits: u8,
    schema: String,
    state: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Field {
    mask: crate::Hex64,
    register: String,
    value: crate::Hex64,
}

impl CpuProfile {
    /// Encodes using the same canonical JSON writer as x86 profiles.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let fields = self
            .id_registers
            .iter()
            .map(|r| {
                let name = REGISTERS
                    .iter()
                    .find(|(reg, _)| *reg == r.register)
                    .ok_or_else(|| {
                        Error::new(
                            Code::ProfileDigest,
                            format!("unknown ARM profile register {:?}", r.register),
                        )
                    })?
                    .1;
                Ok(Field {
                    mask: crate::Hex64(r.mask),
                    register: name.into(),
                    value: crate::Hex64(r.value),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let doc = Document {
            cache: self.cache_policy,
            fields,
            guest_model: crate::Hex32(
                u32::try_from(self.model.value)
                    .map_err(|_| Error::new(Code::ProfileDigest, "ARM model exceeds MIDR width"))?,
            ),
            id: self.id.clone(),
            ownership: OWNERSHIP.into(),
            physical_address_bits: self.physical_address_bits,
            schema: "nvx.arm64.cpu.v1".into(),
            state: STATE_POLICY.into(),
        };
        let value = serde_json::to_value(doc)
            .map_err(|e| Error::new(Code::ProfileDigest, e.to_string()))?;
        Ok(crate::canonical::to_compact(&value).into_bytes())
    }

    fn identify(mut self) -> Result<Self, Error> {
        self.sha256 = crate::canonical::sha256(&self.encode()?);
        Ok(self)
    }

    /// Checks the entire ownership inventory and the canonical digest.
    pub fn validate(&self) -> Result<(), Error> {
        if self.id.is_empty()
            || self.id.len() > 32
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.".contains(&c))
        {
            return Err(Error::new(
                Code::ProfileUnknown,
                "invalid ARM CPU-profile identity",
            ));
        }
        let entry = catalog()?
            .iter()
            .find(|entry| {
                self.id == entry.profile.id
                    || (self.id == entry.host_id() && entry.supports_host_model(self.model))
            })
            .ok_or_else(|| {
                Error::new(Code::ProfileUnknown, "ARM profile is not in this catalog")
            })?;
        if self.id_registers.len() != REGISTERS.len()
            || entry.profile.id_registers.len() != REGISTERS.len()
        {
            return Err(Error::new(
                Code::ProfileDigest,
                "ARM canonical profile has incomplete ownership",
            ));
        }
        for ((r, expected), &(register, _)) in self
            .id_registers
            .iter()
            .zip(&entry.profile.id_registers)
            .zip(&REGISTERS)
        {
            if r.register != register
                || expected.register != register
                || r.mask == 0
                || r.mask != expected.mask
                || r.value & !r.mask != 0
            {
                return Err(Error::new(
                    Code::ProfileDigest,
                    "ARM canonical register inventory/mask mismatch",
                ));
            }
        }
        self.validate_policy(entry)?;
        if self.sha256 != crate::canonical::sha256(&self.encode()?) {
            return Err(Error::new(
                Code::ProfileDigest,
                "ARM canonical profile digest mismatch",
            ));
        }
        Ok(())
    }

    fn validate_policy(&self, entry: &CatalogEntry) -> Result<(), Error> {
        let model_supported = if self.id == entry.profile.id {
            self.model == entry.profile.model
        } else {
            entry.supports_host_model(self.model)
        };
        if !model_supported
            || self.physical_address_bits != entry.profile.physical_address_bits
            || physical_address_bits(entry.parange()?) != Some(self.physical_address_bits)
        {
            return Err(Error::new(
                Code::ProfileDigest,
                "ARM model/address requirements differ from the catalog record",
            ));
        }
        let value = |register| {
            self.id_registers
                .iter()
                .find(|r| r.register == register)
                .map(|r| r.value)
                .ok_or_else(|| Error::new(Code::ProfileDigest, "incomplete ARM policy inventory"))
        };
        let pfr0 = value(SystemReg::ID_AA64PFR0_EL1)?;
        let pfr1 = value(SystemReg::ID_AA64PFR1_EL1)?;
        let isar1 = value(SystemReg::ID_AA64ISAR1_EL1)?;
        let isar2 = value(SystemReg::ID_AA64ISAR2_EL1)?;
        if pfr0 & 0x0000_000f_0000_ff00 != 0
            || pfr1 & 0x0f00_0f00 != 0
            || value(SystemReg::ID_AA64DFR0_EL1)? & 0xf00 != 0
            || value(SystemReg::ID_AA64ZFR0_EL1)? != 0
            || value(SystemReg::ID_AA64SMFR0_EL1)? != 0
            || value(SystemReg::ID_AA64MMFR0_EL1)? & 0xf != u64::from(entry.parange()?)
            || (isar1 >> 4) & 0xf != (entry.value(SystemReg::ID_AA64ISAR1_EL1)? >> 4) & 0xf
            || (isar1 >> 24) & 0xf != (entry.value(SystemReg::ID_AA64ISAR1_EL1)? >> 24) & 0xf
            || isar1 & 0xf000_0f00 != 0
            || isar2 & 0x0f00_ff00 != 0
            || (value(SystemReg::ID_AA64MMFR3_EL1)? >> 60) & 0xf
                != (entry.value(SystemReg::ID_AA64MMFR3_EL1)? >> 60) & 0xf
        {
            return Err(Error::new(
                Code::ProfileUnsupported,
                "ARM state policy requires catalog PAC/address fields and no nested ELs, PMU, SVE, SME or MTE",
            ));
        }
        if self.id == entry.profile.id {
            if self.sha256 != entry.profile.sha256
                || self.id_registers != entry.profile.id_registers
            {
                return Err(Error::new(
                    Code::ProfileDigest,
                    "pinned ARM policy differs from its catalog record",
                ));
            }
        } else if !crate::is_host_profile_id(&self.id) {
            return Err(Error::new(
                Code::ProfileUnknown,
                "ARM profile is not in this catalog",
            ));
        }
        Ok(())
    }
}

/// Returns the pinned architectural policy, not provider-bank constants.
pub fn pinned() -> Result<CpuProfile, Error> {
    catalog()?
        .first()
        .ok_or_else(|| Error::new(Code::ProfileUnknown, "ARM CPU catalog is empty"))?
        .profile()
}

/// Selects a matching pinned profile, or derives a supported native host
/// profile for `host` and for `auto` when no pinned model matches.
pub fn select(requested: &str, native: &Fingerprint) -> Result<CpuProfile, Error> {
    let catalog = catalog()?;
    if requested == crate::HOST {
        return derive_host(native);
    }
    if requested != crate::AUTO && !catalog.iter().any(|entry| entry.profile.id == requested) {
        return Err(Error::new(
            Code::ProfileUnknown,
            format!("unknown ARM CPU profile {requested}"),
        ));
    }
    let entry = if requested == crate::AUTO {
        matching_pinned(catalog, native.midr)?
    } else {
        catalog
            .iter()
            .find(|entry| entry.profile.id == requested && entry.profile.model.matches(native.midr))
    };
    if requested == crate::AUTO
        && crate::auto_falls_back_to_host(
            entry.is_some(),
            catalog
                .iter()
                .any(|entry| entry.host_match.matches(native.midr)),
        )
    {
        return derive_host(native);
    }
    match entry {
        Some(entry) => entry.profile(),
        None if requested == crate::AUTO => Err(Error::new(
            Code::ProfileHostUnknown,
            "no supported ARM pinned profile or native-host CPU policy covers this CPU",
        )),
        None => Err(Error::new(
            Code::CpuGeneration,
            "native ARM CPU does not match the requested pinned profile",
        )),
    }
}

fn matching_pinned(catalog: &[CatalogEntry], midr: u64) -> Result<Option<&CatalogEntry>, Error> {
    crate::catalog::latest_matching_profile(
        catalog
            .iter()
            .filter(|entry| entry.profile.model.matches(midr)),
        |entry| (entry.profile.model, entry.generation.as_str()),
        |entry| entry.profile.id.as_str(),
    )
    .map_err(|_| {
        Error::new(
            Code::ProfileHostUnknown,
            format!("native ARM CPU {midr:#x} matches more than one catalog model class"),
        )
    })
}

/// Derives only policy-permitted native state; does not rename pinned data.
pub fn derive_host(native: &Fingerprint) -> Result<CpuProfile, Error> {
    let catalog = catalog()?;
    let entry = catalog
        .iter()
        .find(|entry| entry.host_match.matches(native.midr))
        .ok_or_else(|| {
            Error::new(
                Code::ProfileHostUnknown,
                "no supported ARM native-host CPU policy covers this CPU",
            )
        })?;
    if native.registers.len() != entry.profile.id_registers.len() {
        return Err(Error::new(
            Code::CpuGeneration,
            "host ARM profile generation/inventory is unsupported",
        ));
    }
    let mut fields = Vec::new();
    for requirement in &entry.profile.id_registers {
        let reg = requirement.register;
        let mask = requirement.mask;
        let mut values = native.registers.iter().filter(|r| r.register == reg);
        let r = values
            .next()
            .ok_or_else(|| Error::new(Code::CpuSurface, format!("missing native {reg:?}")))?;
        if values.next().is_some() || r.value & !mask != 0 {
            return Err(Error::new(
                Code::CpuSurface,
                format!("duplicate or reserved native {reg:?}"),
            ));
        }
        let mut value = r.value;
        if reg == SystemReg::ID_AA64PFR0_EL1 {
            value &= !0xff00;
        }
        if reg == SystemReg::ID_AA64DFR0_EL1 {
            value &= !0xf00;
        }
        if reg == SystemReg::ID_AA64MMFR0_EL1 {
            let pa_bits = physical_address_bits((value & 0xf) as u8)
                .ok_or_else(|| Error::new(Code::ProfileUnsupported, "unknown native PARange"))?;
            if pa_bits < entry.profile.physical_address_bits {
                return Err(Error::new(
                    Code::ProfileUnsupported,
                    "native PARange cannot support the catalog address width",
                ));
            }
            value = (value & !0xf) | u64::from(entry.parange()?);
        }
        fields.push(CpuIdRegister {
            register: reg,
            mask,
            value,
        });
    }
    // Host identity is a separate policy-filtered document. RV remains native
    // provenance, not an observation-frozen compatibility restriction.
    let profile = CpuProfile {
        id: entry.host_id(),
        sha256: [0; 32],
        model: CpuModel {
            mask: entry.profile.model.mask,
            value: native.midr & entry.profile.model.mask,
        },
        physical_address_bits: entry.profile.physical_address_bits,
        cache_policy: CachePolicy::NativeInformational,
        id_registers: fields,
    }
    .identify()?;
    profile.validate()?;
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn native() -> Fingerprint {
        let mut registers = pinned().unwrap().id_registers;
        for r in &mut registers {
            if r.register == SystemReg::ID_AA64PFR0_EL1 {
                r.value |= 0x1100;
            }
            if r.register == SystemReg::ID_AA64DFR0_EL1 {
                r.value |= 0x400;
            }
            if r.register == SystemReg::ID_AA64MMFR0_EL1 {
                r.value = (r.value & !0xf) | 2;
            }
        }
        Fingerprint {
            midr: 0x511f0011,
            registers,
            counter_hz: 19_200_000,
            backend_usable_gpa_bits: 39,
        }
    }

    #[test]
    fn auto_and_host_are_canonical_filtered_distinct_and_keep_native_pac() {
        let n = native();
        let auto = select("auto", &n).unwrap();
        let host = select("host", &n).unwrap();
        auto.validate().unwrap();
        host.validate().unwrap();
        assert_eq!(auto.id, pinned().unwrap().id);
        assert_ne!(auto.id, host.id);
        assert_ne!(auto.sha256, host.sha256);
        assert_eq!(auto.id_registers, host.id_registers);
        assert!(
            String::from_utf8(auto.encode().unwrap())
                .unwrap()
                .contains("nvx.arm64.cpu.v1")
        );
        let pac = auto
            .id_registers
            .iter()
            .find(|r| r.register == SystemReg::ID_AA64ISAR1_EL1)
            .unwrap();
        assert_eq!((pac.value >> 4) & 15, 5);
        assert_eq!((pac.value >> 24) & 15, 1);
        let mut changed = n.clone();
        changed.counter_hz *= 2;
        assert_eq!(select("auto", &changed).unwrap().sha256, auto.sha256);
        changed.midr = 0x51af001e;
        select("auto", &changed).unwrap();
    }

    #[test]
    fn cpu_selection_is_independent_of_counter_admission() {
        let mut native = native();
        let auto = select(crate::AUTO, &native).unwrap();
        let host = select(crate::HOST, &native).unwrap();
        native.counter_hz = 0;
        assert_eq!(select(crate::AUTO, &native).unwrap(), auto);
        assert_eq!(select(crate::HOST, &native).unwrap(), host);
        assert_eq!(
            crate::canonical::format_digest(&auto.sha256),
            "sha256:20960eafb1f3fb34304b876506e4345bd3fcfb626635d9927ac362898854394d"
        );
    }

    #[test]
    fn n1x_pinned_policy_matches_only_the_observed_model_class() {
        let mut n = native();
        n.midr = 0x410fd851;
        let selected = select(crate::AUTO, &n).unwrap();
        assert_eq!(selected.id, "nvidia.n1x.v1");
        selected.validate().unwrap();
        assert_eq!(
            crate::canonical::format_digest(&selected.sha256),
            "sha256:5c819de73c40022de50f199a232c264095fd1d9493ef40e65207b73ff028024a"
        );
        assert_eq!(selected.model.mask, 0xff0f_ffd0);
        assert_eq!(selected.model.value, 0x410f_d850);
        assert_eq!(selected.physical_address_bits, 36);
        assert_eq!(select("nvidia.n1x.v1", &n).unwrap(), selected);
        n.midr = 0x410fd871;
        assert_eq!(select(crate::AUTO, &n).unwrap(), selected);
        assert_eq!(select("nvidia.n1x.v1", &n).unwrap(), selected);
        // The heterogeneous N1X pair differs only in part-number bit 1.
        // Ignoring that bit must not admit any of the other 4094 part numbers.
        for part in 0u64..=0xfff {
            assert_eq!(
                selected.model.matches(0x410f0001 | (part << 4)),
                part == 0xd85 || part == 0xd87,
                "unexpected model match for part {part:#x}"
            );
        }
        n.midr = 0x410fd861;
        assert_eq!(
            select(crate::AUTO, &n).unwrap_err().code,
            Code::ProfileHostUnknown
        );
        assert_eq!(
            select("nvidia.n1x.v1", &n).unwrap_err().code,
            Code::CpuGeneration
        );
        for midr in [0x510f_d851, 0x410e_d851] {
            n.midr = midr;
            assert_eq!(
                select("nvidia.n1x.v1", &n).unwrap_err().code,
                Code::CpuGeneration
            );
        }
        n.midr = 0x511f0011;
        assert_eq!(select(crate::AUTO, &n).unwrap().id, "qualcomm.oryon-x1.v1");
    }

    #[test]
    fn n1x_policy_does_not_admit_extended_state_or_digest_tampering() {
        let mut n = native();
        n.midr = 0x410fd851;
        let selected = select(crate::AUTO, &n).unwrap();
        for (register, value) in [
            (SystemReg::ID_AA64ZFR0_EL1, 1),
            (SystemReg::ID_AA64SMFR0_EL1, 1),
        ] {
            let mut changed = selected.clone();
            changed
                .id_registers
                .iter_mut()
                .find(|r| r.register == register)
                .unwrap()
                .value = value;
            let changed = changed.identify().unwrap();
            assert_eq!(
                changed.validate().unwrap_err().code,
                Code::ProfileUnsupported
            );
        }
        let mut changed = selected;
        changed.sha256[0] ^= 1;
        assert_eq!(changed.validate().unwrap_err().code, Code::ProfileDigest);
    }

    #[test]
    fn backend_gpa_limit_is_not_architectural_cpu_identity() {
        let mut native = native();
        let auto = select(crate::AUTO, &native).unwrap();
        let host = select(crate::HOST, &native).unwrap();
        native.backend_usable_gpa_bits = 48;
        assert_eq!(select(crate::AUTO, &native).unwrap(), auto);
        assert_eq!(select(crate::HOST, &native).unwrap(), host);
        assert_eq!(auto.physical_address_bits, 36);
        assert_eq!(host.physical_address_bits, 36);
        let mmfr0 = host
            .id_registers
            .iter()
            .find(|r| r.register == SystemReg::ID_AA64MMFR0_EL1)
            .unwrap();
        assert_eq!(mmfr0.value & 0xf, 1);
        assert!(
            !String::from_utf8(host.encode().unwrap())
                .unwrap()
                .contains("backend_usable_gpa")
        );
    }

    #[test]
    fn auto_prefers_pinned_then_derives_a_host_profile_for_an_uncatalogued_model() {
        let mut n = native();
        let pinned = pinned().unwrap();
        assert_eq!(select(crate::AUTO, &n).unwrap(), pinned);
        n.midr ^= 0x10;
        assert!(!pinned.model.matches(n.midr));
        let auto = select(crate::AUTO, &n).unwrap();
        let host = select(crate::HOST, &n).unwrap();
        assert_eq!(auto, host);
        auto.validate().unwrap();
        assert!(crate::is_host_profile_id(&auto.id));
        assert_ne!(auto.id, pinned.id);
        assert_ne!(auto.sha256, pinned.sha256);
        assert!(auto.model.matches(n.midr));
        assert_ne!(auto.model, pinned.model);
        assert_eq!(auto.physical_address_bits, pinned.physical_address_bits);
        assert_eq!(auto.id_registers, pinned.id_registers);
        assert_eq!(
            select(&pinned.id, &n).unwrap_err().code,
            Code::CpuGeneration
        );
        n.counter_hz = 0;
        assert_eq!(select(crate::AUTO, &n).unwrap(), auto);
    }

    #[test]
    fn auto_fallback_still_requires_a_supported_host_policy_and_native_state() {
        let mut n = native();
        n.midr ^= 0x10;
        n.registers
            .iter_mut()
            .find(|r| r.register == SystemReg::ID_AA64PFR1_EL1)
            .unwrap()
            .value |= 0x100;
        assert_eq!(
            select(crate::AUTO, &n).unwrap_err().code,
            Code::ProfileUnsupported
        );
        n = native();
        n.midr = 0x410f_d030;
        assert_eq!(
            select(crate::AUTO, &n).unwrap_err().code,
            Code::ProfileHostUnknown
        );
        assert_eq!(
            select(crate::HOST, &n).unwrap_err().code,
            Code::ProfileHostUnknown
        );
    }

    #[test]
    fn arm_matching_uses_shared_latest_revision_and_ambiguity_rules() {
        let base = pinned().unwrap();
        let record = |profile: CpuProfile| CatalogEntry {
            vendor: catalog().unwrap()[0].vendor.clone(),
            generation: profile.id.rsplit_once(".v").unwrap().0.into(),
            host_match: catalog().unwrap()[0].host_match,
            profile,
        };
        let mut newer = base.clone();
        newer.id = format!("{}.v10", base.id.rsplit_once(".v").unwrap().0);
        let records = [record(base.clone()), record(newer)];
        assert!(matching_pinned(&records, 0x410f_d030).unwrap().is_none());
        assert_eq!(
            matching_pinned(&records, native().midr)
                .unwrap()
                .unwrap()
                .profile
                .id,
            records[1].profile.id
        );
        let mut overlap = base.clone();
        overlap.model.mask &= !0x10;
        overlap.model.value &= overlap.model.mask;
        let records = [record(base), record(overlap)];
        assert_eq!(
            matching_pinned(&records, native().midr).err().unwrap().code,
            Code::ProfileHostUnknown
        );
    }

    #[test]
    fn catalog_model_and_address_requirements_cannot_be_substituted() {
        let mut profile = pinned().unwrap();
        profile.model.value = 0x410f_d030;
        assert!(profile.validate().is_err());
        profile = pinned().unwrap();
        profile.model.mask = u64::MAX;
        assert!(profile.validate().is_err());
        profile = pinned().unwrap();
        profile.physical_address_bits = 40;
        assert!(profile.validate().is_err());
    }

    #[test]
    fn catalog_json_rejects_malformed_metadata_and_canonical_digest() {
        let json = include_str!("../profiles/qualcomm.oryon-x1.v1.json");
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        for (field, value) in [
            ("sha256", serde_json::json!("sha256:invalid")),
            ("vendor", serde_json::json!("not a vendor")),
            ("model_mask", serde_json::json!("0x0000000000000000")),
        ] {
            let mut edited = original.clone();
            edited[field] = value;
            assert!(load_catalog_entry(&edited.to_string()).is_err());
        }
        let mut edited = original;
        edited["profile"]["physical_address_bits"] = serde_json::json!(40);
        assert!(load_catalog_entry(&edited.to_string()).is_err());
        let mut edited: serde_json::Value = serde_json::from_str(json).unwrap();
        edited["host_match"]["mask"] = serde_json::json!("0x0000000000000000");
        assert!(load_catalog_entry(&edited.to_string()).is_err());
    }

    #[test]
    fn host_is_derived_and_auto_selection_leaves_semantic_support_to_the_backend() {
        let mut n = native();
        n.registers
            .iter_mut()
            .find(|r| r.register == SystemReg::ID_AA64ISAR0_EL1)
            .unwrap()
            .value &= !(0xf << 60);
        assert_eq!(select("auto", &n).unwrap(), pinned().unwrap());
        assert_ne!(
            select("host", &n).unwrap().id_registers,
            pinned().unwrap().id_registers
        );
        assert!(select("bogus", &n).is_err());
        n.midr = 0x410fd030;
        assert!(select("host", &n).is_err());
        assert!(select("auto", &n).is_err());
    }

    #[test]
    fn auto_accepts_a_known_maskable_superset_without_changing_the_pinned_profile() {
        let mut n = native();
        let isar1 = n
            .registers
            .iter_mut()
            .find(|r| r.register == SystemReg::ID_AA64ISAR1_EL1)
            .unwrap();
        isar1.value = (isar1.value & !(0xf << 44)) | (2 << 44);
        assert_eq!(select(crate::AUTO, &n).unwrap(), pinned().unwrap());
        let host = select(crate::HOST, &n).unwrap();
        assert!(crate::is_host_profile_id(&host.id));
        assert_eq!(host.id, "qualcomm.host.v1");
        assert_eq!(
            (host
                .id_registers
                .iter()
                .find(|r| r.register == SystemReg::ID_AA64ISAR1_EL1)
                .unwrap()
                .value
                >> 44)
                & 0xf,
            2
        );
        assert_ne!(host.id_registers, pinned().unwrap().id_registers);
    }
}
