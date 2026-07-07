// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::CxlComponentRegister;
use crate::spec;
use crate::spec::CxlComponentRegisterType;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use inspect::Inspect;
use std::collections::BTreeMap;

/// Returns the absolute byte offset where the primary cache/mem section begins.
fn cachemem_primary_section_start() -> usize {
    spec::CXL_COMPONENT_REG_RANGE_CACHEMEM_PRIMARY_OFFSET as usize
}

/// Returns the total byte length of the primary cache/mem section.
fn cachemem_primary_section_len() -> usize {
    spec::CXL_COMPONENT_REG_RANGE_CACHEMEM_PRIMARY_SIZE_BYTES as usize
}

/// Sparse representation of a 64-KiB CXL component register aperture.
pub struct CxlComponentRegisters {
    registers: Vec<MappedComponentRegister>,
    cachemem_primary_entries: BTreeMap<u16, Vec<CacheMemDirectoryEntry>>,
}

struct MappedComponentRegister {
    start_offset: u16,
    register: Box<dyn CxlComponentRegister>,
}

struct CacheMemRegionUsage {
    entry_count: usize,
    data_start: usize,
    data_end: usize,
}

#[derive(Copy, Clone)]
struct CacheMemDirectoryEntry {
    capability_id: u16,
    capability_version: u8,
    start_offset: u16,
    len: u16,
}

/// Size of one cache/mem region page used by the capability directory model.
fn cachemem_region_size() -> usize {
    spec::CXL_CACHEMEM_REGION_SIZE_BYTES as usize
}

/// End (exclusive) of a 4-KiB cache/mem region page.
fn cachemem_region_end(region_base: usize) -> usize {
    region_base + cachemem_region_size()
}

/// End (exclusive) of the capability-directory area inside one cache/mem region.
fn cachemem_directory_end(region_base: usize, entry_count: usize) -> usize {
    let header_size = spec::CXL_CACHEMEM_CAPABILITY_ARRAY_ENTRY_SIZE_BYTES as usize;
    region_base + header_size * (entry_count + 1)
}

/// Iterates each 4-KiB cache/mem region base within the primary section.
fn cachemem_region_bases() -> impl Iterator<Item = usize> {
    let start = cachemem_primary_section_start();
    let end = start + cachemem_primary_section_len();
    (start..end).step_by(cachemem_region_size())
}

/// Returns the 4-KiB primary cachemem page base containing `offset`.
fn cachemem_region_base_for_offset(offset: usize) -> Option<usize> {
    let section_start = cachemem_primary_section_start();
    let section_end = section_start + cachemem_primary_section_len();
    let end = offset.checked_add(4)?;
    if offset < section_start || end > section_end {
        return None;
    }

    let page_size = cachemem_region_size();
    let within = offset - section_start;
    Some(section_start + (within / page_size) * page_size)
}

impl CxlComponentRegisters {
    /// Creates an empty CXL component register space.
    pub fn new() -> Self {
        Self {
            registers: Vec::new(),
            cachemem_primary_entries: BTreeMap::new(),
        }
    }

    fn cachemem_entries_for_region(
        &self,
        region_base: u16,
    ) -> Option<&Vec<CacheMemDirectoryEntry>> {
        self.cachemem_primary_entries.get(&region_base)
    }

    fn cachemem_entries_for_region_mut(
        &mut self,
        region_base: u16,
    ) -> &mut Vec<CacheMemDirectoryEntry> {
        self.cachemem_primary_entries
            .entry(region_base)
            .or_default()
    }

    /// Adds one emulated register block to the component aperture.
    ///
    /// Placement is computed from the block type and current section usage.
    /// Returns `false` if the block is unaligned, zero-sized, out of section
    /// capacity, or would overlap an existing block.
    pub fn add_register(&mut self, register: Box<dyn CxlComponentRegister>) -> bool {
        let register_type = register.register_type();
        let capability_id = register.capability_id();
        let capability_version = register.capability_version();
        let len = usize::from(register.len());

        // The register type can be either cache/mem primary or cache/mem extended.
        // TODO: Add support for other register types (e.g. ARB/MUX).
        // TODO: Support placement into the extended cache/mem section.
        // For now, both register types are placed only in the 4-KiB primary section.
        if !matches!(
            register_type,
            CxlComponentRegisterType::CacheMemRegister
                | CxlComponentRegisterType::CacheMemExtendedRegister
        ) {
            return false;
        }

        if len == 0 || !len.is_multiple_of(4) {
            return false;
        }

        let section_start = cachemem_primary_section_start();
        let section_len = cachemem_primary_section_len();
        let Some(section_end) = section_start.checked_add(section_len) else {
            return false;
        };

        // Find a 4-KiB cache/mem page where we can fit both:
        // 1) the grown directory (existing entries + this new one), and
        // 2) the actual register block payload.
        let mut selected: Option<(usize, usize)> = None;
        for base in cachemem_region_bases() {
            let usage = self.cachemem_region_usage(base as u16);
            let directory_end = cachemem_directory_end(base, usage.entry_count + 1);

            // Directory growth must not consume already-placed payload bytes.
            if directory_end > usage.data_start {
                continue;
            }

            // Data begins after both current payload usage and directory growth.
            let candidate_start = usage.data_end.max(directory_end);
            let Some(candidate_end) = candidate_start.checked_add(len) else {
                continue;
            };

            if candidate_end <= cachemem_region_end(base) {
                selected = Some((base, candidate_start));
                break;
            }
        }

        let Some((base, start)) = selected else {
            return false;
        };
        let region_base = base as u16;

        if !start.is_multiple_of(4) {
            return false;
        }

        let Some(end) = start.checked_add(len) else {
            return false;
        };

        if end > spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES as usize {
            return false;
        }

        if start < section_start || end > section_end {
            return false;
        }

        // Final safety check: no overlap with any previously mapped block.
        for reg in &self.registers {
            let reg_start = usize::from(reg.start_offset);
            let reg_end = reg_start + usize::from(reg.register.len());

            if start < reg_end && reg_start < end {
                return false;
            }
        }

        self.registers.push(MappedComponentRegister {
            start_offset: start as u16,
            register,
        });

        // Keep a per-page cachemem directory view sorted by payload start offset.
        let entries = self.cachemem_entries_for_region_mut(region_base);
        let entry = CacheMemDirectoryEntry {
            capability_id,
            capability_version,
            start_offset: start as u16,
            len: len as u16,
        };

        let insert_at = entries.partition_point(|e| e.start_offset <= entry.start_offset);
        entries.insert(insert_at, entry);
        true
    }

    /// Scans one cache/mem 4-KiB page and reports directory entry count + data end.
    fn cachemem_region_usage(&self, region_base: u16) -> CacheMemRegionUsage {
        let Some(entries) = self.cachemem_entries_for_region(region_base) else {
            return CacheMemRegionUsage {
                entry_count: 0,
                data_start: cachemem_region_end(usize::from(region_base)),
                data_end: usize::from(region_base),
            };
        };

        let entry_count = entries.len();
        let data_start = entries
            .iter()
            .map(|e| usize::from(e.start_offset))
            .min()
            .unwrap_or(cachemem_region_end(usize::from(region_base)));
        let data_end = entries
            .iter()
            .map(|e| usize::from(e.start_offset) + usize::from(e.len))
            .max()
            .unwrap_or(usize::from(region_base));

        CacheMemRegionUsage {
            entry_count,
            data_start,
            data_end,
        }
    }

    /// Serves synthetic cache/mem directory dwords when `offset` targets that area.
    fn read_cachemem_directory_u32(&self, offset: usize) -> Option<u32> {
        // TODO: Add synthetic directory support for extended cache/mem section.
        // For now, only primary cache/mem section reads are synthesized.
        // Compute the page directly from offset to avoid scanning all pages.
        let region_base = cachemem_region_base_for_offset(offset)?;

        // Convert absolute aperture address -> dword index within this page.
        let index = (offset - region_base)
            / (spec::CXL_CACHEMEM_CAPABILITY_ARRAY_ENTRY_SIZE_BYTES as usize);

        let entry_count = self
            .cachemem_entries_for_region(region_base as u16)
            .map(Vec::len)
            .unwrap_or(0);

        // Index 0 is always the required CXL Capability Header.
        if index == 0 {
            return spec::CxlCacheMemCapabilityHeader::encode(entry_count);
        }

        // Use precomputed per-page sorted entries instead of filtering all registers.
        let entries = self.cachemem_entries_for_region(region_base as u16)?;

        if entries.len() > spec::CXL_CACHEMEM_CAPABILITY_ARRAY_MAX_ENTRIES {
            return None;
        }

        // Only offsets that fall in the directory window are synthetic.
        let directory_end = cachemem_directory_end(region_base, entries.len());
        if offset >= directory_end {
            return None;
        }

        // Capability-array entries are 1-based in address space but 0-based in Vec.
        let entry = entries[index - 1];

        // Pointer is encoded as an offset inside this 4-KiB page.
        let pointer = entry.start_offset - region_base as u16;
        spec::CxlCacheMemCapabilityArrayEntry::encode(
            entry.capability_id,
            entry.capability_version,
            pointer,
        )
    }

    /// Validates that an aperture access is either 4 or 8 bytes,
    /// is naturally aligned to its length, and is in-bounds.
    fn checked_access_offset(offset: u16, len: usize) -> Option<usize> {
        let offset = usize::from(offset);
        let end = offset.checked_add(len)?;
        if !matches!(len, 4 | 8)
            || !offset.is_multiple_of(len)
            || end > spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES as usize
        {
            return None;
        }
        Some(offset)
    }

    fn read_u32(&self, offset: usize, val: &mut u32) -> IoResult {
        // Directory space is synthesized; it is not backed by a register object.
        if let Some(value) = self.read_cachemem_directory_u32(offset) {
            *val = value;
            return IoResult::Ok;
        }

        let Some((index, rel)) = self.find_block_index_for_access(offset) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        let Some(value) = self.registers[index].register.read_u32(rel) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        *val = value;
        IoResult::Ok
    }

    fn write_u32(&mut self, offset: usize, val: u32) -> IoResult {
        // Directory spaces are read-only by definition.
        if self.read_cachemem_directory_u32(offset).is_some() {
            return IoResult::Err(IoError::InvalidRegister);
        }

        let Some((index, rel)) = self.find_block_index_for_access(offset) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        if self.registers[index].register.write_u32(rel, val) {
            IoResult::Ok
        } else {
            IoResult::Err(IoError::InvalidRegister)
        }
    }

    /// Locates the mapped register block that owns an absolute component offset.
    fn find_block_index_for_access(&self, offset: usize) -> Option<(usize, u16)> {
        for (index, reg) in self.registers.iter().enumerate() {
            let start = usize::from(reg.start_offset);
            let end = start + usize::from(reg.register.len());
            if offset >= start && offset + 4 <= end {
                let rel = offset - start;
                return Some((index, rel as u16));
            }
        }

        None
    }

    /// Returns the component register aperture length in bytes.
    pub fn len(&self) -> usize {
        spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES as usize
    }

    /// Returns the absolute component offset for a capability id.
    pub fn capability_offset(&self, capability_id: u16) -> Option<u16> {
        self.registers
            .iter()
            .find(|reg| reg.register.capability_id() == capability_id)
            .map(|reg| reg.start_offset)
    }

    /// Reads bytes at a component-register-relative offset.
    pub fn read(&self, offset: u16, data: &mut [u8]) -> IoResult {
        let Some(offset) = Self::checked_access_offset(offset, data.len()) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        // checked_access_offset constrains accesses to 4 or 8 bytes, so splitting
        // into 4-byte chunks is always exact and safe here.
        for (i, chunk) in data.chunks_exact_mut(4).enumerate() {
            let mut value = 0;
            match self.read_u32(offset + i * 4, &mut value) {
                IoResult::Ok => {}
                err => return err,
            }
            chunk.copy_from_slice(&value.to_le_bytes());
        }

        IoResult::Ok
    }

    /// Writes bytes at a component-register-relative offset.
    pub fn write(&mut self, offset: u16, data: &[u8]) -> IoResult {
        let Some(offset) = Self::checked_access_offset(offset, data.len()) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        // checked_access_offset constrains accesses to 4 or 8 bytes, so splitting
        // into 4-byte chunks is always exact and safe here.
        for (i, chunk) in data.chunks_exact(4).enumerate() {
            let value = u32::from_le_bytes(chunk.try_into().expect("4-byte dword chunk"));
            match self.write_u32(offset + i * 4, value) {
                IoResult::Ok => {}
                err => return err,
            }
        }

        IoResult::Ok
    }

    /// Resets all registered component register blocks.
    pub fn reset(&mut self) {
        for reg in &mut self.registers {
            reg.register.reset();
        }
    }
}

impl Default for CxlComponentRegisters {
    fn default() -> Self {
        Self::new()
    }
}

impl Inspect for CxlComponentRegisters {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.field("aperture_len", self.len());
        resp.field("mapped_register_count", self.registers.len());
        resp.field(
            "cachemem_primary_page_count",
            self.cachemem_primary_entries.len(),
        );
    }
}

mod save_restore {
    use super::*;
    use thiserror::Error;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateBlob;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "cxl.component_registers")]
        pub struct SavedState {
            #[mesh(1)]
            pub registers: Vec<(String, SavedStateBlob)>,
        }
    }

    #[derive(Debug, Error)]
    enum CxlComponentRegistersRestoreError {
        #[error("found unexpected component register {0}")]
        InvalidRegister(String),
    }

    impl SaveRestore for CxlComponentRegisters {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                registers: self
                    .registers
                    .iter_mut()
                    .map(|mapped| {
                        let id = mapped.register.label().to_owned();
                        Ok((id, mapped.register.save()?))
                    })
                    .collect::<Result<_, _>>()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { registers } = state;

            for (id, entry) in registers {
                tracing::debug!(save_id = id.as_str(), "restoring cxl component register");

                let mut restored = false;
                for mapped in &mut self.registers {
                    if mapped.register.label() == id {
                        mapped.register.restore(entry)?;
                        restored = true;
                        break;
                    }
                }

                if !restored {
                    return Err(RestoreError::InvalidSavedState(
                        CxlComponentRegistersRestoreError::InvalidRegister(id).into(),
                    ));
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::component_registers::test_helper::TestCxlComponentRegisterBlock;
    use crate::spec;
    use chipset_device::io::IoResult;
    use vmcore::save_restore::SaveRestore;

    use super::CxlComponentRegisterType;
    use super::CxlComponentRegisters;

    #[test]
    fn component_register_space_metadata() {
        // The aperture is always the fixed 64-KiB CXL component-register window.
        let regs = CxlComponentRegisters::new();
        assert_eq!(regs.len(), 64 * 1024);
    }

    #[test]
    fn component_register_space_read_write_and_reset() {
        // Data reads/writes target payload space after the directory dwords.
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );
        assert!(matches!(
            regs.write(0x1008, &0x1122_3344u32.to_le_bytes()),
            IoResult::Ok
        ));

        let mut read_bytes = [0u8; 4];
        assert!(matches!(regs.read(0x1008, &mut read_bytes), IoResult::Ok));
        let read = u32::from_le_bytes(read_bytes);
        assert_eq!(read, 0x1122_3344);

        regs.reset();
        assert!(matches!(regs.read(0x1008, &mut read_bytes), IoResult::Ok));
        let read = u32::from_le_bytes(read_bytes);
        assert_eq!(read, 0);
    }

    #[test]
    fn cachemem_directory_header_exists_even_with_zero_entries() {
        // Index 0 must always expose the CXL Capability Header.
        let regs = CxlComponentRegisters::new();
        let mut header_bytes = [0u8; 4];
        assert!(matches!(regs.read(0x1000, &mut header_bytes), IoResult::Ok));
        let header = u32::from_le_bytes(header_bytes);
        assert_eq!(header, 0x0011_0001);
    }

    #[test]
    fn component_register_space_rejects_invalid_offsets() {
        // Directory dwords are writable=false, and unmapped/out-of-bounds reads fail.
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );
        assert!(matches!(
            regs.write(0x1000, &1u32.to_le_bytes()),
            IoResult::Err(_)
        ));

        assert!(matches!(
            regs.write(u16::MAX, &1u32.to_le_bytes()),
            IoResult::Err(_)
        ));
        let mut value_bytes = [0u8; 4];
        assert!(matches!(
            regs.read(u16::MAX, &mut value_bytes),
            IoResult::Err(_)
        ));

        // Address is valid in aperture but unmapped by any register block.
        assert!(matches!(
            regs.write(0x2000, &1u32.to_le_bytes()),
            IoResult::Err(_)
        ));
        assert!(matches!(
            regs.read(0x2000, &mut value_bytes),
            IoResult::Err(_)
        ));
    }

    #[test]
    fn component_register_space_rejects_partial_and_unaligned_accesses() {
        // Spec requires full-width accesses only (4-byte for 32-bit, 8-byte for 64-bit).
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        let mut one = [0u8; 1];
        assert!(matches!(regs.read(0x1008, &mut one), IoResult::Err(_)));
        assert!(matches!(regs.write(0x1008, &one), IoResult::Err(_)));

        let mut two = [0u8; 2];
        assert!(matches!(regs.read(0x1008, &mut two), IoResult::Err(_)));
        assert!(matches!(regs.write(0x1008, &two), IoResult::Err(_)));

        let mut four = [0u8; 4];
        assert!(matches!(regs.read(0x100a, &mut four), IoResult::Err(_)));
        assert!(matches!(regs.write(0x100a, &four), IoResult::Err(_)));

        let mut eight = [0u8; 8];
        assert!(matches!(regs.read(0x100c, &mut eight), IoResult::Err(_)));
        assert!(matches!(regs.write(0x100c, &eight), IoResult::Err(_)));
    }

    #[test]
    fn component_register_space_rejects_second_block_when_directory_growth_would_overlap() {
        // TODO: Teach allocator to repack/relocate payloads so additional capabilities
        // can be added in the same 4-KiB primary page without directory overlap.
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );
        assert!(
            !regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        // Existing register remains unchanged at its original offset.
        let mut value_bytes = [0u8; 4];
        assert!(matches!(regs.read(0x1008, &mut value_bytes), IoResult::Ok));
        let value = u32::from_le_bytes(value_bytes);
        assert_eq!(value, 0);
    }

    #[test]
    fn component_register_space_rejects_bad_block_params() {
        // Zero-sized and unaligned-size payloads are rejected.
        let mut regs = CxlComponentRegisters::new();
        assert!(
            !regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                0,
            )))
        );
        assert!(
            !regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                10,
            )))
        );
    }

    #[test]
    fn component_register_space_rejects_when_section_capacity_is_exhausted() {
        // Fill every 4-KiB primary cachemem page with one max-sized payload block.
        let mut regs = CxlComponentRegisters::new();
        let page_count = (spec::CXL_COMPONENT_REG_RANGE_CACHEMEM_PRIMARY_SIZE_BYTES as usize)
            / (spec::CXL_CACHEMEM_REGION_SIZE_BYTES as usize);
        let max_payload_len = (spec::CXL_CACHEMEM_REGION_SIZE_BYTES as usize)
            - (2 * spec::CXL_CACHEMEM_CAPABILITY_ARRAY_ENTRY_SIZE_BYTES as usize);

        for _ in 0..page_count {
            assert!(
                regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                    CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                    max_payload_len,
                )))
            );
        }

        assert!(
            !regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );
    }

    #[test]
    fn component_register_space_supports_both_cachemem_register_types_in_primary_section() {
        // Extended register type is supported and placed in the primary section.
        // TODO: Add support for placement/synthesis in the extended section itself.
        let mut primary_regs = CxlComponentRegisters::new();
        assert!(
            primary_regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        let mut extended_type_regs = CxlComponentRegisters::new();
        assert!(
            extended_type_regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_EXTENDED_REGISTER,
                16,
            )))
        );

        // CXL RAB/MUX placement is intentionally unsupported for now.
        assert!(
            !primary_regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_ARB_MUX_REGISTER,
                16,
            )))
        );
    }

    #[test]
    fn cachemem_directory_exposes_header_and_capability_pointer_entries() {
        // Directory layout must expose header first, then capability pointer entries.
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        // First dword: CXL Capability Header with Array_Size=1.
        let mut header_bytes = [0u8; 4];
        assert!(matches!(regs.read(0x1000, &mut header_bytes), IoResult::Ok));
        let header = u32::from_le_bytes(header_bytes);
        assert_eq!(header, 0x0111_0001);

        // Second dword: capability header, pointer should resolve to 0x8 in this region.
        let mut entry_bytes = [0u8; 4];
        assert!(matches!(regs.read(0x1004, &mut entry_bytes), IoResult::Ok));
        let entry = u32::from_le_bytes(entry_bytes);
        assert_eq!(entry & 0xFFFF, 0x20);
        assert_eq!((entry >> 16) & 0xF, 1);
        assert_eq!((entry >> 20) & 0xFFF, 0x2);
    }

    #[test]
    fn component_register_space_accepts_extended_cachemem_register_type() {
        // Extended register type is accepted, but still mapped into primary section.
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_EXTENDED_REGISTER,
                16,
            )))
        );

        // First payload still starts in primary page after synthetic directory words.
        let mut value_bytes = [0u8; 4];
        assert!(matches!(regs.read(0x1008, &mut value_bytes), IoResult::Ok));
        let value = u32::from_le_bytes(value_bytes);
        assert_eq!(value, 0);
    }

    #[test]
    fn component_register_space_save_restore_preserves_payload_state() {
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        let original = 0xdead_beefu32;
        assert!(matches!(
            regs.write(0x1008, &original.to_le_bytes()),
            IoResult::Ok
        ));

        let saved = regs.save().expect("save should succeed");

        let mut restored = CxlComponentRegisters::new();
        assert!(
            restored.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        restored.restore(saved).expect("restore should succeed");

        let mut value_bytes = [0u8; 4];
        assert!(matches!(
            restored.read(0x1008, &mut value_bytes),
            IoResult::Ok
        ));
        let value = u32::from_le_bytes(value_bytes);
        assert_eq!(value, original);
    }

    #[test]
    fn component_register_space_restore_rejects_layout_mismatch() {
        let mut regs = CxlComponentRegisters::new();
        assert!(
            regs.add_register(Box::new(TestCxlComponentRegisterBlock::new(
                CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                16,
            )))
        );

        let saved = regs.save().expect("save should succeed");

        let mut restored = CxlComponentRegisters::new();
        let err = restored.restore(saved).expect_err("restore must fail");
        match err {
            vmcore::save_restore::RestoreError::InvalidSavedState(_) => {}
            _ => panic!("unexpected restore error variant"),
        }
    }
}
