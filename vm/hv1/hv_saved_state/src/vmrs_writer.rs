// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMRS file writer.
//!
//! Assembles a complete `.vmrs` file from a partition state blob and guest
//! memory ranges, using [`hvs_file::writer::HvsFileWriter`] for the
//! underlying HyperV Storage file format.

use hvs_file::writer::HvsFileWriter;
use std::io::{self, Seek, Write};

/// VM version used for dump files (v10.0 / Iron).
const VM_VERSION_IRON: i64 = 0x0A00;

/// Size of one guest memory block in bytes (1 MiB = 256 × 4K pages).
const GMO_BLOCK_SIZE_BYTES: usize = 1_048_576;

/// Size of one guest memory block in 4K pages.
const GMO_BLOCK_SIZE_PAGES: u64 = 256;

/// Memory block metadata (MEMORY_BLOCK_OBJECT_SAVE_STRUCT_CURRENT).
///
/// 48 bytes with padding for alignment.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryBlockMeta {
    saved_state_version: u32, // WPMM_MB_SAVE_STATE_VERSION_3 = 3
    flags: u32,
    page_count_total: u64,
    mbp_index_start: u64,
    gpa_index_start: u64,
    virtual_node: u32,
    _padding: u32,
    ksr_block_id: u64,
}

impl MemoryBlockMeta {
    fn as_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 48];
        buf[0..4].copy_from_slice(&self.saved_state_version.to_le_bytes());
        buf[4..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..16].copy_from_slice(&self.page_count_total.to_le_bytes());
        buf[16..24].copy_from_slice(&self.mbp_index_start.to_le_bytes());
        buf[24..32].copy_from_slice(&self.gpa_index_start.to_le_bytes());
        buf[32..36].copy_from_slice(&self.virtual_node.to_le_bytes());
        buf[36..40].copy_from_slice(&self._padding.to_le_bytes());
        buf[40..48].copy_from_slice(&self.ksr_block_id.to_le_bytes());
        buf
    }
}

/// A contiguous GPA range to include in the dump.
struct MemoryRange {
    gpa_start: u64,
    data: Vec<u8>,
}

/// Writes a complete `.vmrs` file.
///
/// Usage:
/// 1. Create with [`VmrsWriter::new`]
/// 2. Set the partition state blob with [`set_partition_state`]
/// 3. Add memory ranges with [`add_memory_range`]
/// 4. Call [`finish`] to write the file
pub struct VmrsWriter<W: Write + Seek> {
    hvs: HvsFileWriter<W>,
    partition_state: Option<Vec<u8>>,
    memory_ranges: Vec<MemoryRange>,
}

impl<W: Write + Seek> VmrsWriter<W> {
    /// Creates a new VMRS writer.
    pub fn new(writer: W) -> io::Result<Self> {
        Ok(Self {
            hvs: HvsFileWriter::new(writer)?,
            partition_state: None,
            memory_ranges: Vec::new(),
        })
    }

    /// Sets the partition state blob (from [`PartitionStateBuilder::finish`]).
    pub fn set_partition_state(&mut self, blob: Vec<u8>) {
        self.partition_state = Some(blob);
    }

    /// Adds a contiguous guest physical memory range to the dump.
    ///
    /// `gpa_start` is the byte address of the start of the range (must be
    /// page-aligned). `data` contains the raw memory contents. The data
    /// length must be a multiple of 4096 (page size).
    pub fn add_memory_range(&mut self, gpa_start: u64, data: Vec<u8>) {
        assert!(gpa_start % 4096 == 0, "GPA must be page-aligned");
        assert!(data.len() % 4096 == 0, "Data must be page-aligned");
        self.memory_ranges.push(MemoryRange { gpa_start, data });
    }

    /// Writes the complete `.vmrs` file.
    pub fn finish(mut self) -> io::Result<W> {
        // VM version
        self.hvs.add_int("/savedstate/VmVersion", VM_VERSION_IRON);
        self.hvs
            .add_int("/configuration/properties/version", VM_VERSION_IRON);

        // Partition state
        let partition_state = self
            .partition_state
            .take()
            .unwrap_or_default();
        self.hvs
            .add_array("/savedstate/savedVM/partition_state", partition_state)?;

        // Memory layout: split ranges into 1 MiB blocks
        let mut meta_block_idx = 0u32;
        let mut data_block_idx = 0u64;

        for range in &self.memory_ranges {
            let gpa_start = range.gpa_start;
            let total_pages = range.data.len() as u64 / 4096;
            let gpa_page_start = gpa_start / 4096;

            // Write metadata for this contiguous range
            let meta = MemoryBlockMeta {
                saved_state_version: 3,
                flags: 0,
                page_count_total: total_pages,
                mbp_index_start: data_block_idx * GMO_BLOCK_SIZE_PAGES,
                gpa_index_start: gpa_page_start,
                virtual_node: 0,
                _padding: 0,
                ksr_block_id: 0,
            };

            let meta_key = format!("/savedstate/RamMemoryBlock{meta_block_idx}");
            self.hvs.add_array(&meta_key, meta.as_bytes())?;
            meta_block_idx += 1;

            // Write data blocks (1 MiB each)
            let mut offset = 0usize;
            while offset < range.data.len() {
                let end = (offset + GMO_BLOCK_SIZE_BYTES).min(range.data.len());
                let block_data = range.data[offset..end].to_vec();
                let data_key = format!("/savedstate/RamBlock{data_block_idx}");
                self.hvs.add_array(&data_key, block_data)?;
                data_block_idx += 1;
                offset = end;
            }
        }

        self.hvs.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PartitionStateBuilder, ProcessorArch, X64VpRegisters};
    use hvdef::HvX64SegmentRegister;
    use hvdef::HvX64TableRegister;
    use hvs_file::reader::HvsFileReader;
    use std::io::Cursor;

    #[test]
    fn write_and_read_vmrs() {
        // Build partition state
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.set_os_id(0);
        let mut regs = X64VpRegisters::default();
        regs.rip = 0xFFFFF800_12345678;
        regs.cr3 = 0x1AD000;
        regs.cr0 = 0x80050033;
        regs.efer = 0xD01;
        regs.cs = HvX64SegmentRegister {
            base: 0,
            limit: 0xFFFFFFFF,
            selector: 0x10,
            attributes: 0x209B,
        };
        regs.idtr = HvX64TableRegister {
            pad: [0; 3],
            limit: 0xFFF,
            base: 0xFFFFF800_00000000,
        };
        builder.add_x64_vp(0, &regs);
        let blob = builder.finish();

        // Build VMRS
        let buf = Cursor::new(Vec::new());
        let mut vmrs = VmrsWriter::new(buf).unwrap();
        vmrs.set_partition_state(blob);

        // Add 2 MiB of memory at GPA 0
        let mem = vec![0xABu8; 2 * GMO_BLOCK_SIZE_BYTES];
        vmrs.add_memory_range(0, mem);

        let buf = vmrs.finish().unwrap();
        let data = buf.into_inner();

        // Read back and verify
        let mut reader = HvsFileReader::open(Cursor::new(&data)).unwrap();

        // Check version
        let version = reader.read_int("/savedstate/VmVersion").unwrap();
        assert_eq!(version, VM_VERSION_IRON);

        // Check partition state exists
        assert!(reader.contains_key("/savedstate/savedVM/partition_state"));
        let ps = reader.read_array("/savedstate/savedVM/partition_state").unwrap();
        assert!(!ps.is_empty());

        // Check memory metadata
        assert!(reader.contains_key("/savedstate/RamMemoryBlock0"));
        let meta_bytes = reader.read_array("/savedstate/RamMemoryBlock0").unwrap();
        assert_eq!(meta_bytes.len(), 48);
        let page_count = u64::from_le_bytes(meta_bytes[8..16].try_into().unwrap());
        assert_eq!(page_count, 512); // 2 MiB = 512 pages

        // Check RAM data blocks
        assert!(reader.contains_key("/savedstate/RamBlock0"));
        assert!(reader.contains_key("/savedstate/RamBlock1"));
        let block0 = reader.read_array("/savedstate/RamBlock0").unwrap();
        assert_eq!(block0.len(), GMO_BLOCK_SIZE_BYTES);
        assert!(block0.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn multiple_memory_ranges() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.add_x64_vp(0, &X64VpRegisters::default());
        let blob = builder.finish();

        let buf = Cursor::new(Vec::new());
        let mut vmrs = VmrsWriter::new(buf).unwrap();
        vmrs.set_partition_state(blob);

        // Range 1: 1 MiB at GPA 0
        vmrs.add_memory_range(0, vec![0x11u8; GMO_BLOCK_SIZE_BYTES]);
        // Range 2: 1 MiB at GPA 0x1_0000_0000 (4 GiB, after MMIO hole)
        vmrs.add_memory_range(0x1_0000_0000, vec![0x22u8; GMO_BLOCK_SIZE_BYTES]);

        let buf = vmrs.finish().unwrap();
        let mut reader = HvsFileReader::open(Cursor::new(buf.into_inner())).unwrap();

        // Two metadata blocks
        assert!(reader.contains_key("/savedstate/RamMemoryBlock0"));
        assert!(reader.contains_key("/savedstate/RamMemoryBlock1"));

        // Verify GPA mapping in second metadata block
        let meta1 = reader.read_array("/savedstate/RamMemoryBlock1").unwrap();
        let gpa_page_start = u64::from_le_bytes(meta1[24..32].try_into().unwrap());
        assert_eq!(gpa_page_start, 0x1_0000_0000 / 4096);

        // Two data blocks
        let block0 = reader.read_array("/savedstate/RamBlock0").unwrap();
        assert!(block0.iter().all(|&b| b == 0x11));
        let block1 = reader.read_array("/savedstate/RamBlock1").unwrap();
        assert!(block1.iter().all(|&b| b == 0x22));
    }

    #[test]
    fn empty_memory_produces_valid_file() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.add_x64_vp(0, &X64VpRegisters::default());
        let blob = builder.finish();

        let buf = Cursor::new(Vec::new());
        let mut vmrs = VmrsWriter::new(buf).unwrap();
        vmrs.set_partition_state(blob);
        // No memory added

        let buf = vmrs.finish().unwrap();
        let reader = HvsFileReader::open(Cursor::new(buf.into_inner())).unwrap();
        assert!(reader.contains_key("/savedstate/VmVersion"));
        assert!(reader.contains_key("/savedstate/savedVM/partition_state"));
    }
}
