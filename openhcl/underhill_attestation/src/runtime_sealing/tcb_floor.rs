// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SNP-only comparison of trusted local observations, never VMGS metadata.
//!
//! Components are ordered only within an identical supported report/CPU domain.
//! Unknown domains require exact TCB equality. An accepted increase must survive
//! later derivation, verification, or persistence failures.

use super::Error;
use super::ErrorInner;
use x86defs::snp::SNP_CPUID_FAMILY_MILAN_GENOA;
use x86defs::snp::SNP_CPUID_FAMILY_TURIN;
use x86defs::snp::SNP_CPUID_MODELS_MILAN_GENOA;
use x86defs::snp::SNP_CPUID_MODELS_TURIN_90_AF;
use x86defs::snp::SNP_CPUID_MODELS_TURIN_C0_CF;
use x86defs::snp::SnpReport;
use x86defs::snp::SnpTcbVersionLegacy;
use x86defs::snp::SnpTcbVersionTurin;
use zerocopy::FromBytes;

// V2 lacks CPUID fields; v6 adds extended TCB semantics not modeled here.
const SNP_REPORT_VERSIONS_WITH_COMPONENT_ORDERING: core::ops::RangeInclusive<u32> = 3..=5;

#[derive(Debug)]
pub(super) struct SnpTcbFloor {
    pub(super) tcb_version: u64,
    pub(super) domain: SnpDomain,
}

/// CPU family/model selects the TCB layout; report version gates the semantics
/// we support. Preserve unknown domains without guessing their interpretation.
/// See AMD 56860 rev. 1.59, section 2.3, tables 4 and 5.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SnpDomain {
    pub(super) version: u32,
    pub(super) cpuid: Option<[u8; 2]>,
}

#[derive(Debug, PartialEq, Eq)]
enum SnpTcbLayout {
    Legacy,
    Turin,
}

impl SnpDomain {
    fn tcb_layout(&self) -> Option<SnpTcbLayout> {
        if !SNP_REPORT_VERSIONS_WITH_COMPONENT_ORDERING.contains(&self.version) {
            return None;
        }
        let [family, model] = self.cpuid?;
        match family {
            SNP_CPUID_FAMILY_MILAN_GENOA if SNP_CPUID_MODELS_MILAN_GENOA.contains(&model) => {
                Some(SnpTcbLayout::Legacy)
            }
            SNP_CPUID_FAMILY_TURIN
                if SNP_CPUID_MODELS_TURIN_90_AF.contains(&model)
                    || SNP_CPUID_MODELS_TURIN_C0_CF.contains(&model) =>
            {
                Some(SnpTcbLayout::Turin)
            }
            _ => None,
        }
    }
}

impl SnpTcbFloor {
    /// Only accept a report obtained from the trusted local hardware interface.
    /// This checks the layout and typed SVN, not a remote report's signature.
    pub(super) fn from_report(report: &[u8], tcb_version: u64) -> Result<Self, Error> {
        let (report, _) =
            SnpReport::read_from_prefix(report).map_err(|_| Error(ErrorInner::MalformedReport))?;
        if report.reported_tcb != tcb_version {
            return Err(Error(ErrorInner::ReportTcbMismatch));
        }
        Ok(Self {
            tcb_version,
            domain: SnpDomain {
                version: report.version,
                cpuid: (report.version >= 3).then_some([report.cpuid_fam_id, report.cpuid_mod_id]),
            },
        })
    }

    /// Pure comparison; the caller commits an accepted observation before
    /// subsequent fallible work. Higher components cannot offset lower ones.
    pub(super) fn check_successor(&self, observed: &Self) -> Result<(), Error> {
        if self.domain != observed.domain {
            return Err(Error(ErrorInner::TcbIncompatible));
        }
        if self.tcb_version == observed.tcb_version {
            return Ok(());
        }
        let floor = self.tcb_version.to_le_bytes();
        let next = observed.tcb_version.to_le_bytes();
        let layout = self
            .domain
            .tcb_layout()
            .ok_or(Error(ErrorInner::TcbIncompatible))?;
        let meets_floor = match layout {
            SnpTcbLayout::Legacy => {
                let floor = SnpTcbVersionLegacy::read_from_bytes(&floor)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                let next = SnpTcbVersionLegacy::read_from_bytes(&next)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                if next.reserved != floor.reserved {
                    return Err(Error(ErrorInner::TcbIncompatible));
                }
                next.bootloader >= floor.bootloader
                    && next.tee >= floor.tee
                    && next.snp >= floor.snp
                    && next.microcode >= floor.microcode
            }
            SnpTcbLayout::Turin => {
                let floor = SnpTcbVersionTurin::read_from_bytes(&floor)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                let next = SnpTcbVersionTurin::read_from_bytes(&next)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                if next.reserved != floor.reserved {
                    return Err(Error(ErrorInner::TcbIncompatible));
                }
                next.fmc >= floor.fmc
                    && next.bootloader >= floor.bootloader
                    && next.tee >= floor.tee
                    && next.snp >= floor.snp
                    && next.microcode >= floor.microcode
            }
        };
        if !meets_floor {
            return Err(Error(ErrorInner::TcbLowered));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn snp_layout_selection_separates_report_support_from_cpu_encoding() {
        for version in [0, 1, 2, 3, 4, 5, 6, u32::MAX] {
            assert_eq!(
                SnpDomain {
                    version,
                    cpuid: None
                }
                .tcb_layout(),
                None
            );
            for family in [0x18, 0x19, 0x1a, 0x1b] {
                for model in 0..=u8::MAX {
                    let expected = match (version, family, model) {
                        (3..=5, 0x19, 0x00..=0x1f) => Some(SnpTcbLayout::Legacy),
                        (3..=5, 0x1a, 0x90..=0xaf | 0xc0..=0xcf) => Some(SnpTcbLayout::Turin),
                        _ => None,
                    };
                    let domain = SnpDomain {
                        version,
                        cpuid: Some([family, model]),
                    };
                    assert_eq!(domain.tcb_layout(), expected, "{domain:?}");
                }
            }
        }
    }
}
