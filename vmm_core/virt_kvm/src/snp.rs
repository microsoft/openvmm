// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SEV-SNP launch support for KVM partitions.
//!
//! The loader writes initial private page contents into the userspace side of
//! guestmemfd-backed slots. This module translates the loader's import types
//! into SNP launch updates, constructs the firmware CPUID page from the BSP's
//! effective KVM CPUID table, validates the initial BSP state, and finalizes
//! the launch context. Runtime shared/private transitions are handled by the
//! memory module rather than the launch path.

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::memory::private_memory_range_from_slots;
use std::os::fd::AsFd;
use virt::InitialPageImportType;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
/// Progress of the one-shot SNP launch sequence.
pub(crate) enum SnpLaunchState {
    /// No launch command has been issued.
    NotStarted,
    /// Launch is in progress.
    Started,
    /// Launch completed successfully.
    Finished,
    /// Launch failed and cannot be retried on this partition.
    Failed,
}

impl virt::AcceptInitialPages for KvmPartition {
    type Error = KvmError;

    fn accept_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Self::Error> {
        self.inner.snp_launch_initial_pages(pages)
    }
}

impl KvmPartitionInner {
    /// Runs the SNP launch sequence once and records its terminal state.
    fn snp_launch_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), KvmError> {
        {
            let mut state = self.snp_launch_state.lock();
            match *state {
                SnpLaunchState::NotStarted => *state = SnpLaunchState::Started,
                SnpLaunchState::Started => return Err(KvmError::SnpLaunchInProgress),
                SnpLaunchState::Finished => return Ok(()),
                SnpLaunchState::Failed => return Err(KvmError::SnpLaunchFailed),
            }
        }

        tracing::info!(page_ranges = pages.len(), "starting SNP launch");
        match self.snp_launch_initial_pages_inner(pages) {
            Ok(()) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Finished;
                tracing::info!("finished SNP launch");
                Ok(())
            }
            Err(err) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Failed;
                tracing::error!(error = &err as &dyn std::error::Error, "failed SNP launch");
                Err(err)
            }
        }
    }

    /// Adds every loader-provided range to the SNP launch context and finalizes it.
    fn snp_launch_initial_pages_inner(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        let sev = self.sev.as_ref().ok_or(KvmError::IsolationNotSupported)?;
        self.kvm.check_sev_snp_launch_extensions()?;
        let mut launch_start = kvm::kvm_sev_snp_launch_start {
            // TODO: This debug-capable policy is for bring-up.
            policy: (1 << 19) | (1 << 17) | (1 << 16),
            ..Default::default()
        };
        tracing::debug!(policy = launch_start.policy, "KVM_SEV_SNP_LAUNCH_START");
        self.kvm
            .sev_snp_launch_start(sev.as_fd(), &mut launch_start)?;

        let memory = self.memory.lock();
        for page in pages {
            let kvm_page_type = crate::arch::snp::snp_launch_page_type(page.import_type)?;

            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)
                .map_err(map_snp_private_range_error)?;
            if page.import_type == InitialPageImportType::Cpuid {
                tracing::debug!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    cpuid_entries = self.bsp_cpuid.len(),
                    "writing SNP CPUID page"
                );
                write_snp_cpuid_page(private_range.hva, page.range.len(), &self.bsp_cpuid)?;
            }
            // IGVM page data with no file payload represents a normal private
            // page containing zeroes, not an SNP hardware ZERO page. Do not
            // infer the page type from contents; add an explicit import type if
            // IGVM gains native hardware-zero-page semantics.
            let gpa = page.range.start();
            tracing::trace!(
                gpa,
                len = page.range.len(),
                ?kvm_page_type,
                import_type = ?page.import_type,
                tag = page.tag,
                "KVM_SEV_SNP_LAUNCH_UPDATE"
            );
            self.kvm.sev_snp_launch_update(
                sev.as_fd(),
                gpa / hvdef::HV_PAGE_SIZE,
                private_range.hva as u64,
                page.range.len(),
                kvm_page_type,
            )?;
        }
        self.prepare_snp_vmsa_register_state()?;
        tracing::debug!("KVM_SEV_SNP_LAUNCH_FINISH");
        self.kvm
            .sev_snp_launch_finish(sev.as_fd(), &mut Default::default())?;
        Ok(())
    }

    /// Prepares the vCPU state that KVM will encode into each SNP VMSA.
    ///
    /// KVM owns the VMSA pages and does not expose them as launch imports.
    /// During `KVM_SEV_SNP_LAUNCH_FINISH`, KVM copies each vCPU's current
    /// register, VMCB, and FPU state into its VMSA before measuring and
    /// encrypting it. Normalize the required XCR0 state for every vCPU and
    /// validate the direct-boot BSP before that state becomes protected.
    ///
    /// AP register state is not validated here because SNP APs are started
    /// later through GHCB AP creation with guest-provided VMSAs.
    fn prepare_snp_vmsa_register_state(&self) -> Result<(), KvmError> {
        for vp in &self.vps {
            let vp_info = vp.vp_info();
            let kvm_vp = self.kvm.vp(vp_info.apic_id);
            let sregs = kvm_vp.get_sregs()?;

            let xcr0 = kvm_vp.get_xcr0()?;
            if xcr0 & x86defs::xsave::XFEATURE_X87 == 0 {
                kvm_vp.set_xcr0(xcr0 | x86defs::xsave::XFEATURE_X87)?;
            }

            if vp_info.base.vp_index.is_bsp() {
                validate_snp_bsp_register_state(&kvm_vp.get_regs()?, &sregs)?;
            }
        }

        Ok(())
    }
}

/// Validates the BSP state that KVM will seal into the initial VMSA.
///
/// This catches loader or register-plumbing errors before launch finish, where
/// they would otherwise surface as a firmware error or a guest that cannot
/// execute the direct Linux entry point.
fn validate_snp_bsp_register_state(
    regs: &kvm::kvm_regs,
    sregs: &kvm::kvm_sregs,
) -> Result<(), KvmError> {
    const REQUIRED_CR0: u64 = x86defs::X64_CR0_PE | x86defs::X64_CR0_PG;
    const REQUIRED_CR4: u64 = x86defs::X64_CR4_PAE;
    const REQUIRED_EFER: u64 =
        x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA | x86defs::X64_EFER_NXE;

    if sregs.cr0 & REQUIRED_CR0 != REQUIRED_CR0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR0"));
    }
    if sregs.cr3 == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR3"));
    }
    if sregs.cr4 & REQUIRED_CR4 != REQUIRED_CR4 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR4"));
    }
    if sregs.efer & REQUIRED_EFER != REQUIRED_EFER {
        return Err(KvmError::InvalidState("invalid SNP BSP EFER"));
    }
    if sregs.cs.present == 0 || sregs.cs.l == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CS"));
    }
    if regs.rip == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP RIP"));
    }

    tracing::debug!(
        rip = regs.rip,
        rsi = regs.rsi,
        cr0 = sregs.cr0,
        cr3 = sregs.cr3,
        cr4 = sregs.cr4,
        efer = sregs.efer,
        vmsa_efer = sregs.efer | x86defs::X64_EFER_SVME,
        cs_selector = sregs.cs.selector,
        cs_base = sregs.cs.base,
        cs_limit = sregs.cs.limit,
        cs_type = sregs.cs.type_,
        ds_selector = sregs.ds.selector,
        es_selector = sregs.es.selector,
        ss_selector = sregs.ss.selector,
        "validated SNP BSP register state"
    );

    Ok(())
}

const SNP_CPUID_COUNT_MAX: usize = 64;
const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
const SNP_CPUID_FN_SIZE: usize = 48;

/// Serializes the effective BSP CPUID table in the SNP firmware page format.
///
/// Entries that become all-zero after firmware-required sanitization are
/// omitted so the fixed 64-entry firmware limit is applied to the serialized
/// table rather than the unsanitized KVM table.
fn write_snp_cpuid_page(
    page: *mut u8,
    page_len: u64,
    cpuid: &[kvm::kvm_cpuid_entry2],
) -> Result<(), KvmError> {
    if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64 {
        return Err(KvmError::InvalidSnpLaunchRange);
    }

    let cpuid = cpuid
        .iter()
        .copied()
        .filter_map(|mut entry| {
            sanitize_snp_cpuid_entry(&mut entry);
            (entry.eax != 0 || entry.ebx != 0 || entry.ecx != 0 || entry.edx != 0).then_some(entry)
        })
        .collect::<Vec<_>>();
    if cpuid.len() > SNP_CPUID_COUNT_MAX {
        return Err(KvmError::TooManySnpCpuidEntries(cpuid.len()));
    }

    let page = unsafe { std::slice::from_raw_parts_mut(page, page_len as usize) };
    page.fill(0);
    page[..4].copy_from_slice(&(cpuid.len() as u32).to_le_bytes());

    for (index, cpuid) in cpuid.iter().copied().enumerate() {
        let entry = &mut page[SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE..]
            [..SNP_CPUID_FN_SIZE];
        entry[0..4].copy_from_slice(&cpuid.function.to_le_bytes());
        entry[4..8].copy_from_slice(&cpuid.index.to_le_bytes());
        let initial_xsave_leaf = cpuid.function
            == x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
            && (cpuid.index == 0 || cpuid.index == 1);
        let (xcr0, xss) = if initial_xsave_leaf {
            (1_u64, 0_u64)
        } else {
            (0_u64, 0_u64)
        };
        entry[8..16].copy_from_slice(&xcr0.to_le_bytes());
        entry[16..24].copy_from_slice(&xss.to_le_bytes());
        entry[24..28].copy_from_slice(&cpuid.eax.to_le_bytes());
        let ebx = if initial_xsave_leaf { 0x240 } else { cpuid.ebx };
        entry[28..32].copy_from_slice(&ebx.to_le_bytes());
        entry[32..36].copy_from_slice(&cpuid.ecx.to_le_bytes());
        entry[36..40].copy_from_slice(&cpuid.edx.to_le_bytes());
    }

    Ok(())
}

/// Removes KVM-synthetic CPUID bits that SNP firmware validates against hardware.
fn sanitize_snp_cpuid_entry(entry: &mut kvm::kvm_cpuid_entry2) {
    match (entry.function, entry.index) {
        // SNP firmware validates the CPUID page against hardware-supported
        // CPUID values, not KVM's synthetic guest CPUID additions.
        (0x1, _) => entry.ecx &= !0x01000000,
        (0x7, 0) => {
            entry.ebx &= !0x2;
            entry.edx = 0;
        }
        (0x80000008, _) => entry.ebx &= !0x02000000,
        (0x80000021, _) => {
            entry.eax &= !0x200;
            entry.ecx = 0;
        }
        _ => {}
    }
}

/// Converts a generic private-slot lookup failure into the SNP launch error.
fn map_snp_private_range_error(err: KvmError) -> KvmError {
    match err {
        KvmError::InvalidPrivateMemoryRange => KvmError::InvalidSnpLaunchRange,
        err => err,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_snp_cpuid_page_writes_linux_table_and_xsave_inputs() {
        let mut page = vec![0xff; hvdef::HV_PAGE_SIZE as usize];
        let cpuid = [
            kvm::kvm_cpuid_entry2 {
                function: 1,
                index: 0,
                eax: 0x11,
                ebx: 0x12,
                ecx: 0x13,
                edx: 0x14,
                ..Default::default()
            },
            kvm::kvm_cpuid_entry2 {
                function: x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0,
                index: 0,
                eax: 0x21,
                ebx: 0x22,
                ecx: 0x23,
                edx: 0x24,
                ..Default::default()
            },
        ];

        write_snp_cpuid_page(page.as_mut_ptr(), page.len() as u64, &cpuid).unwrap();

        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(page[16..20].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(page[40..44].try_into().unwrap()), 0x11);
        assert_eq!(u32::from_le_bytes(page[44..48].try_into().unwrap()), 0x12);
        let xsave = 16 + 48;
        assert_eq!(
            u32::from_le_bytes(page[xsave..xsave + 4].try_into().unwrap()),
            x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
        );
        assert_eq!(
            u64::from_le_bytes(page[xsave + 8..xsave + 16].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_le_bytes(page[xsave + 28..xsave + 32].try_into().unwrap()),
            0x240
        );
    }

    #[test]
    fn write_snp_cpuid_page_sparsifies_after_sanitizing() {
        let mut page = vec![0xff; hvdef::HV_PAGE_SIZE as usize];
        let cpuid = [
            kvm::kvm_cpuid_entry2 {
                function: 1,
                ecx: 0x0100_0000,
                ..Default::default()
            },
            kvm::kvm_cpuid_entry2 {
                function: 2,
                ..Default::default()
            },
            kvm::kvm_cpuid_entry2 {
                function: 0x4000_0000,
                eax: 0x4000_0001,
                ..Default::default()
            },
        ];

        write_snp_cpuid_page(page.as_mut_ptr(), page.len() as u64, &cpuid).unwrap();

        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(page[16..20].try_into().unwrap()),
            0x4000_0000
        );
    }

    #[test]
    fn write_snp_cpuid_page_enforces_limit_after_sparsifying() {
        let mut page = vec![0xff; hvdef::HV_PAGE_SIZE as usize];
        let mut cpuid = (0..SNP_CPUID_COUNT_MAX as u32)
            .map(|function| kvm::kvm_cpuid_entry2 {
                function,
                eax: 1,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        cpuid.push(kvm::kvm_cpuid_entry2 {
            function: 0xffff,
            ..Default::default()
        });

        write_snp_cpuid_page(page.as_mut_ptr(), page.len() as u64, &cpuid).unwrap();
        assert_eq!(
            u32::from_le_bytes(page[0..4].try_into().unwrap()),
            SNP_CPUID_COUNT_MAX as u32
        );

        cpuid.last_mut().unwrap().eax = 1;
        assert!(matches!(
            write_snp_cpuid_page(page.as_mut_ptr(), page.len() as u64, &cpuid),
            Err(KvmError::TooManySnpCpuidEntries(count)) if count == SNP_CPUID_COUNT_MAX + 1
        ));
    }
}
