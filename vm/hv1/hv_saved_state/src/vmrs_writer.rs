// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMRS file writer.
//!
//! Assembles a complete `.vmrs` file from a partition state blob and guest
//! memory ranges, using [`hvs_file::writer::HvsFileWriter`] for the
//! underlying HyperV Storage file format.
//!
//! Guest memory is read on demand via a caller-provided
//! [`GuestMemoryReader`] trait — memory is never buffered in full.

use crate::defs::GMO_BLOCK_SIZE_BYTES;
use crate::defs::GMO_BLOCK_SIZE_PAGES;
use crate::defs::MemoryBlockSaveStruct;
use crate::defs::VM_VERSION_IRON;
use crate::defs::WPMM_MB_SAVE_STATE_VERSION_3;
use hvs_file::writer::HvsFileWriter;
use std::io::{self, Seek, Write};
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// A contiguous guest physical address range.
#[derive(Clone, Debug)]
pub struct GpaRange {
    /// Starting GPA (byte address, must be page-aligned).
    pub gpa_start: u64,
    /// Length in bytes (must be a multiple of 4096).
    pub length: u64,
}

/// Trait for reading guest physical memory on demand.
///
/// Implementors provide access to guest RAM without requiring the entire
/// contents to be materialized in memory at once.
pub trait GuestMemoryReader {
    /// Reads guest physical memory starting at `gpa` into `buf`.
    ///
    /// Returns an error if the read fails. The caller guarantees that
    /// `gpa..gpa+buf.len()` falls within a previously declared
    /// [`GpaRange`].
    fn read_gpa(&mut self, gpa: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// Writes a complete `.vmrs` file.
///
/// Usage:
/// 1. Create with [`VmrsWriter::new`]
/// 2. Set the partition state blob with [`set_partition_state`]
/// 3. Declare memory ranges with [`add_memory_range`]
/// 4. Call [`finish`] with a [`GuestMemoryReader`] to stream memory to disk
pub struct VmrsWriter<W: Write + Seek> {
    hvs: HvsFileWriter<W>,
    partition_state: Option<Vec<u8>>,
    ranges: Vec<GpaRange>,
}

impl<W: Write + Seek> VmrsWriter<W> {
    /// Creates a new VMRS writer.
    pub fn new(writer: W) -> io::Result<Self> {
        Ok(Self {
            hvs: HvsFileWriter::new(writer)?,
            partition_state: None,
            ranges: Vec::new(),
        })
    }

    /// Sets the partition state blob (from [`PartitionStateBuilder::finish`]).
    pub fn set_partition_state(&mut self, blob: Vec<u8>) {
        self.partition_state = Some(blob);
    }

    /// Declares a contiguous guest physical memory range to include.
    ///
    /// `gpa_start` must be page-aligned and `length` must be a multiple
    /// of 4096. The actual memory content is read later during [`finish`].
    pub fn add_memory_range(&mut self, gpa_start: u64, length: u64) {
        assert!(gpa_start % 4096 == 0, "GPA must be page-aligned");
        assert!(length % 4096 == 0, "length must be page-aligned");
        self.ranges.push(GpaRange { gpa_start, length });
    }

    /// Writes the complete `.vmrs` file, reading guest memory on demand.
    ///
    /// Memory is streamed through a reusable 1 MiB buffer — at no point
    /// is the entire guest address space materialized in memory.
    pub fn finish(mut self, reader: &mut dyn GuestMemoryReader) -> io::Result<W> {
        // VM version
        self.hvs.add_int("/savedstate/VmVersion", VM_VERSION_IRON);
        self.hvs
            .add_int("/configuration/properties/version", VM_VERSION_IRON);

        // Partition state
        let partition_state = self.partition_state.take().unwrap_or_default();
        self.hvs
            .add_array("/savedstate/savedVM/partition_state", &partition_state)?;

        // Memory layout: split ranges into 1 MiB blocks, streaming each
        // block through a reusable buffer.
        let mut meta_block_idx = 0u32;
        let mut data_block_idx = 0u64;
        let mut block_buf = vec![0u8; GMO_BLOCK_SIZE_BYTES];

        for range in &self.ranges {
            let total_pages = range.length / 4096;
            let gpa_page_start = range.gpa_start / 4096;

            // Write metadata for this contiguous range
            let mut meta = MemoryBlockSaveStruct::new_zeroed();
            meta.saved_state_version = WPMM_MB_SAVE_STATE_VERSION_3;
            meta.page_count_total = total_pages;
            meta.mbp_index_start = data_block_idx * GMO_BLOCK_SIZE_PAGES;
            meta.gpa_index_start = gpa_page_start;

            let meta_key = format!("/savedstate/RamMemoryBlock{meta_block_idx}");
            self.hvs.add_array(&meta_key, meta.as_bytes())?;
            meta_block_idx += 1;

            // Stream data blocks (1 MiB each)
            let mut gpa = range.gpa_start;
            let gpa_end = range.gpa_start + range.length;
            while gpa < gpa_end {
                let block_len = GMO_BLOCK_SIZE_BYTES.min((gpa_end - gpa) as usize);
                let buf = &mut block_buf[..block_len];
                reader.read_gpa(gpa, buf)?;

                let data_key = format!("/savedstate/RamBlock{data_block_idx}");
                self.hvs.add_array(&data_key, buf)?;
                data_block_idx += 1;
                gpa += block_len as u64;
            }
        }

        self.hvs.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PartitionStateBuilder, ProcessorArch, VpState, X64VpState};
    use hvdef::Vtl;
    use hvs_file::reader::HvsFileReader;
    use std::io::Cursor;

    /// Test reader that fills all reads with a single byte value.
    struct FillReader(u8);

    impl GuestMemoryReader for FillReader {
        fn read_gpa(&mut self, _gpa: u64, buf: &mut [u8]) -> io::Result<()> {
            buf.fill(self.0);
            Ok(())
        }
    }

    /// Test reader backed by a map of GPA ranges to fill bytes.
    struct MultiRangeReader(Vec<(u64, u64, u8)>); // (start, end, fill)

    impl GuestMemoryReader for MultiRangeReader {
        fn read_gpa(&mut self, gpa: u64, buf: &mut [u8]) -> io::Result<()> {
            for &(start, end, fill) in &self.0 {
                if gpa >= start && gpa < end {
                    buf.fill(fill);
                    return Ok(());
                }
            }
            Err(io::Error::new(io::ErrorKind::Other, "unmapped GPA"))
        }
    }

    fn make_test_blob() -> Vec<u8> {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.set_os_id(0);
        let mut regs = virt::x86::vp::Registers::default();
        regs.rip = 0xFFFFF800_12345678;
        regs.cr3 = 0x1AD000;
        regs.cr0 = 0x80050033;
        regs.efer = 0xD01;
        regs.cs = virt::x86::SegmentRegister {
            base: 0,
            limit: 0xFFFFFFFF,
            selector: 0x10,
            attributes: 0x209B,
        };
        regs.idtr = virt::x86::TableRegister {
            base: 0xFFFFF800_00000000,
            limit: 0xFFF,
        };
        builder.add_vp(
            0,
            vec![(
                Vtl::Vtl0,
                VpState::X64(X64VpState {
                    registers: regs,
                    debug_registers: None,
                    xsave: None,
                }),
            )],
            Vtl::Vtl0,
        );
        builder.finish()
    }

    #[test]
    fn write_and_read_vmrs() {
        let blob = make_test_blob();

        let buf = Cursor::new(Vec::new());
        let mut vmrs = VmrsWriter::new(buf).unwrap();
        vmrs.set_partition_state(blob);
        vmrs.add_memory_range(0, 2 * GMO_BLOCK_SIZE_BYTES as u64);

        let mut mem = FillReader(0xAB);
        let buf = vmrs.finish(&mut mem).unwrap();
        let data = buf.into_inner();

        let mut hvs_reader = HvsFileReader::open(Cursor::new(&data)).unwrap();

        assert_eq!(
            hvs_reader.read_int("/savedstate/VmVersion").unwrap(),
            VM_VERSION_IRON
        );
        assert!(hvs_reader.contains_key("/savedstate/savedVM/partition_state"));

        // Check memory metadata
        let meta_bytes = hvs_reader
            .read_array("/savedstate/RamMemoryBlock0")
            .unwrap();
        assert_eq!(meta_bytes.len(), 48);
        let page_count = u64::from_le_bytes(meta_bytes[8..16].try_into().unwrap());
        assert_eq!(page_count, 512); // 2 MiB = 512 pages

        // Check RAM data blocks were streamed correctly
        let block0 = hvs_reader.read_array("/savedstate/RamBlock0").unwrap();
        assert_eq!(block0.len(), GMO_BLOCK_SIZE_BYTES);
        assert!(block0.iter().all(|&b| b == 0xAB));
        let block1 = hvs_reader.read_array("/savedstate/RamBlock1").unwrap();
        assert!(block1.iter().all(|&b| b == 0xAB));
    }

    fn make_default_blob() -> Vec<u8> {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.add_vp(
            0,
            vec![(
                Vtl::Vtl0,
                VpState::X64(X64VpState {
                    registers: Default::default(),
                    debug_registers: None,
                    xsave: None,
                }),
            )],
            Vtl::Vtl0,
        );
        builder.finish()
    }

    #[test]
    fn multiple_memory_ranges() {
        let blob = make_default_blob();

        let buf = Cursor::new(Vec::new());
        let mut vmrs = VmrsWriter::new(buf).unwrap();
        vmrs.set_partition_state(blob);
        vmrs.add_memory_range(0, GMO_BLOCK_SIZE_BYTES as u64);
        vmrs.add_memory_range(0x1_0000_0000, GMO_BLOCK_SIZE_BYTES as u64);

        let mut mem = MultiRangeReader(vec![
            (0, GMO_BLOCK_SIZE_BYTES as u64, 0x11),
            (
                0x1_0000_0000,
                0x1_0000_0000 + GMO_BLOCK_SIZE_BYTES as u64,
                0x22,
            ),
        ]);
        let buf = vmrs.finish(&mut mem).unwrap();
        let mut hvs_reader = HvsFileReader::open(Cursor::new(buf.into_inner())).unwrap();

        // Two metadata blocks
        assert!(hvs_reader.contains_key("/savedstate/RamMemoryBlock0"));
        assert!(hvs_reader.contains_key("/savedstate/RamMemoryBlock1"));

        // Verify GPA mapping in second metadata block
        let meta1 = hvs_reader.read_array("/savedstate/RamMemoryBlock1").unwrap();
        let gpa_page_start = u64::from_le_bytes(meta1[24..32].try_into().unwrap());
        assert_eq!(gpa_page_start, 0x1_0000_0000 / 4096);

        // Data read on demand with correct fill bytes
        let block0 = hvs_reader.read_array("/savedstate/RamBlock0").unwrap();
        assert!(block0.iter().all(|&b| b == 0x11));
        let block1 = hvs_reader.read_array("/savedstate/RamBlock1").unwrap();
        assert!(block1.iter().all(|&b| b == 0x22));
    }

    #[test]
    fn empty_memory_produces_valid_file() {
        let blob = make_default_blob();

        let buf = Cursor::new(Vec::new());
        let mut vmrs = VmrsWriter::new(buf).unwrap();
        vmrs.set_partition_state(blob);

        let mut mem = FillReader(0);
        let buf = vmrs.finish(&mut mem).unwrap();
        let hvs_reader = HvsFileReader::open(Cursor::new(buf.into_inner())).unwrap();
        assert!(hvs_reader.contains_key("/savedstate/VmVersion"));
        assert!(hvs_reader.contains_key("/savedstate/savedVM/partition_state"));
    }
}
