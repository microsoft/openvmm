// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common helper routines for all loaders.

use crate::importer::BootPageAcceptance;
use crate::importer::GuestArch;
use crate::importer::ImageLoad;
use crate::importer::SegmentRegister;
use crate::importer::TableRegister;
use crate::importer::X86Register;
use hvdef::HV_PAGE_SIZE;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use x86defs::GdtEntry;
use x86defs::X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES;
use x86defs::X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Trait alias for `Read + Seek`.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

const DEFAULT_GDT_COUNT: usize = 4;
/// The size of the default GDT table, in bytes.
pub const DEFAULT_GDT_SIZE: u64 = HV_PAGE_SIZE;

/// Import a default GDT at the given address, with one page imported.
/// The GDT is used with cs as entry 1, and data segments (ds, es, fs, gs, ss) as entry 2.
/// Registers using the GDT are imported with vtl 0 only.
pub fn import_default_gdt(
    importer: &mut dyn ImageLoad<X86Register>,
    gdt_page_base: u64,
) -> anyhow::Result<()> {
    // Create a default GDT consisting of two entries.
    // ds, es, fs, gs, ss are entry 2 (linear_selector)
    // cs is entry 1 (linear_code64_selector)
    let default_data_attributes: u16 = X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES.into();
    let default_code_attributes: u16 = X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES.into();
    let gdt: [GdtEntry; DEFAULT_GDT_COUNT] = [
        GdtEntry::new_zeroed(),
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_code_attributes as u8,
            attr_high: (default_code_attributes >> 8) as u8,
            ..GdtEntry::new_zeroed()
        },
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_data_attributes as u8,
            attr_high: (default_data_attributes >> 8) as u8,
            ..GdtEntry::new_zeroed()
        },
        GdtEntry::new_zeroed(),
    ];
    let gdt_entry_size = size_of::<GdtEntry>();
    let linear_selector_offset = 2 * gdt_entry_size;
    let linear_code64_selector_offset = gdt_entry_size;

    // Import the GDT into the specified base page.
    importer.import_pages(
        gdt_page_base,
        DEFAULT_GDT_SIZE / HV_PAGE_SIZE,
        "default-gdt",
        BootPageAcceptance::Exclusive,
        gdt.as_bytes(),
    )?;

    // Import GDTR and selectors.
    let mut import_reg = |register| importer.import_vp_register(register);
    import_reg(X86Register::Gdtr(TableRegister {
        base: gdt_page_base * HV_PAGE_SIZE,
        limit: (size_of::<GdtEntry>() * DEFAULT_GDT_COUNT - 1) as u16,
    }))?;

    let ds = SegmentRegister {
        selector: linear_selector_offset as u16,
        base: 0,
        limit: 0xffffffff,
        attributes: default_data_attributes,
    };
    import_reg(X86Register::Ds(ds))?;
    import_reg(X86Register::Es(ds))?;
    import_reg(X86Register::Fs(ds))?;
    import_reg(X86Register::Gs(ds))?;
    import_reg(X86Register::Ss(ds))?;

    let cs = SegmentRegister {
        selector: linear_code64_selector_offset as u16,
        base: 0,
        limit: 0xffffffff,
        attributes: default_code_attributes,
    };
    import_reg(X86Register::Cs(cs))?;

    Ok(())
}

/// Returned when the MMIO layout is not supported.
#[derive(Debug, Error)]
#[error("exactly two MMIO gaps are required")]
pub struct UnsupportedMmio;

/// Computes the x86 variable MTRRs that describe the given memory layout. This
/// is intended to be used to setup MTRRs for booting a guest with two mmio
/// gaps, such as booting Linux, UEFI, or PCAT.
///
/// N.B. Currently this panics if there are not exactly two MMIO ranges.
pub fn compute_variable_mtrrs(
    memory: &MemoryLayout,
    physical_address_width: u8,
) -> Result<Vec<X86Register>, UnsupportedMmio> {
    const WRITEBACK: u64 = 0x6;

    let &[mmio_gap_low, mmio_gap_high] = memory.mmio().try_into().map_err(|_| UnsupportedMmio)?;

    // Clamp the width to something reasonable.
    let gpa_space_size = physical_address_width.clamp(36, 52);

    // The MMIO limits will be the basis of the MTRR calculations
    // as page count doesn't work when there may be gaps between memory blocks.

    let mut result = Vec::with_capacity(8);

    // Our PCAT firmware sets MTRR 200 and MTRR Mask 201 to 128 MB during boot, so we
    // mimic that here.
    let pcat_mtrr_size = 128 * 1024 * 1024;

    result.push(X86Register::MtrrPhysBase0(WRITEBACK));
    result.push(X86Register::MtrrPhysMask0(mtrr_mask(
        gpa_space_size,
        pcat_mtrr_size - 1,
    )));

    // If there is more than 128 MB, use MTRR 202 and MTRR Mask 203 to cover the
    // amount of memory below the 3.8GB memory gap.
    if memory.end_of_ram() > pcat_mtrr_size {
        result.push(X86Register::MtrrPhysBase1(pcat_mtrr_size | WRITEBACK));
        result.push(X86Register::MtrrPhysMask1(mtrr_mask(
            gpa_space_size,
            mmio_gap_low.start() - 1,
        )));
    }

    // If there is more than ~3.8GB of memory, use MTRR 204 and MTRR Mask 205 to cover
    // the amount of memory above 4GB.
    if memory.end_of_ram() > mmio_gap_low.end() {
        result.push(X86Register::MtrrPhysBase2(mmio_gap_low.end() | WRITEBACK));
        result.push(X86Register::MtrrPhysMask2(mtrr_mask(
            gpa_space_size,
            mmio_gap_high.start() - 1,
        )));
    }

    // If there is more memory than 64GB then use MTRR 206 and MTRR Mask 207 and possibly
    // MTRR 208 and MTRR Mask 209 depending on maximum address width. Both MTRR pairs are
    // used with the magic 8TB boundary to work around a bug in older Linux kernels
    // (e.g. RHEL 6.x, etc.)
    if memory.end_of_ram() > mmio_gap_high.end() {
        result.push(X86Register::MtrrPhysBase3(mmio_gap_high.end() | WRITEBACK));
        result.push(X86Register::MtrrPhysMask3(mtrr_mask(
            gpa_space_size,
            (1 << std::cmp::min(gpa_space_size, 43)) - 1,
        )));
        if gpa_space_size > 43 {
            result.push(X86Register::MtrrPhysBase4((1 << 43) | WRITEBACK));
            result.push(X86Register::MtrrPhysMask4(mtrr_mask(
                gpa_space_size,
                (1 << gpa_space_size) - 1,
            )));
        }
    }

    Ok(result)
}

fn mtrr_mask(gpa_space_size: u8, maximum_address: u64) -> u64 {
    const ENABLED: u64 = 1 << 11;

    let mut result = ENABLED;

    // Set all the bits above bit 11 to 1's to cover the gpa_space_size
    for index in 12..gpa_space_size {
        result |= 1 << index;
    }

    // Clear the span of bits above bit 11 to cover the maximum address
    for index in 12..gpa_space_size {
        let test_maximum_address = 1 << index;

        if maximum_address >= test_maximum_address {
            // Turn the correct bit off
            result &= !(1 << index);
        } else {
            // Done clearing the span of bits
            break;
        }
    }

    result
}

/// Error returned by [`ChunkBuf::import_file_region`].
#[derive(Debug, Error)]
pub enum ImportFileRegionError {
    /// The file length exceeds the memory length.
    #[error("file length {file_length} exceeds memory length {memory_length}")]
    FileLengthExceedsMemoryLength {
        /// The file length.
        file_length: u64,
        /// The memory length.
        memory_length: u64,
    },
    /// Failed to seek the file.
    #[error("failed to seek file")]
    Seek(#[source] std::io::Error),
    /// Failed to read the file.
    #[error("failed to read file")]
    Read(#[source] std::io::Error),
    /// Failed to import pages.
    #[error("failed to import pages")]
    ImportPages(#[source] anyhow::Error),
    /// Address computation overflowed.
    #[error("address computation overflowed")]
    Overflow,
}

/// Parameters for [`ChunkBuf::import_file_region`].
pub struct ImportFileRegion<'a, F: ?Sized> {
    /// The file to read from.
    pub file: &'a mut F,
    /// The offset within the file to start reading.
    pub file_offset: u64,
    /// The number of bytes to read from the file.
    pub file_length: u64,
    /// The guest physical address to import into.
    pub gpa: u64,
    /// The total memory region length (file data + zero fill).
    pub memory_length: u64,
    /// The page acceptance type.
    pub acceptance: BootPageAcceptance,
    /// A debug tag for tracing.
    pub tag: &'a str,
}

/// A page-aligned chunk buffer for streaming file data into guest memory.
///
/// The buffer is guaranteed to hold at least one page (`HV_PAGE_SIZE`) and its
/// length is always a whole number of pages. Reuse the same `ChunkBuf` across
/// multiple imports to avoid repeated allocations.
pub struct ChunkBuf(Vec<u8>);

impl ChunkBuf {
    /// Default chunk size (64 KiB).
    const DEFAULT_SIZE: usize = 64 * 1024;

    /// Create a new chunk buffer with the default size.
    pub fn new() -> Self {
        Self::with_size(Self::DEFAULT_SIZE)
    }

    /// Create a new chunk buffer with the given byte size, rounded down to a
    /// whole number of pages.
    ///
    /// Panics if `size` is less than `HV_PAGE_SIZE`.
    pub fn with_size(size: usize) -> Self {
        let page_count = size as u64 / HV_PAGE_SIZE;
        assert!(page_count > 0, "ChunkBuf must be at least one page");
        Self(vec![0u8; (page_count * HV_PAGE_SIZE) as usize])
    }

    /// Import a region from a file into guest memory.
    ///
    /// Reads `file_length` bytes from `file` at `file_offset`, importing them
    /// at guest physical address `gpa`. If `gpa` is not page-aligned, the
    /// leading bytes of that page are zeroed. If `memory_length` exceeds
    /// `file_length`, the remaining bytes are zeroed. Zeroing extends to the
    /// end of the last target page.
    pub fn import_file_region<F, R: GuestArch>(
        &mut self,
        importer: &mut dyn ImageLoad<R>,
        params: ImportFileRegion<'_, F>,
    ) -> Result<(), ImportFileRegionError>
    where
        F: ReadSeek + ?Sized,
    {
        let ImportFileRegion {
            file,
            file_offset,
            file_length,
            gpa,
            memory_length,
            acceptance,
            tag,
        } = params;

        if file_length > memory_length {
            return Err(ImportFileRegionError::FileLengthExceedsMemoryLength {
                file_length,
                memory_length,
            });
        }

        if memory_length == 0 {
            return Ok(());
        }

        let buf = &mut self.0[..];
        let buf_pages = buf.len() as u64 / HV_PAGE_SIZE;

        let page_mask = HV_PAGE_SIZE - 1;
        let leading_zero = gpa & page_mask;
        let page_base = gpa / HV_PAGE_SIZE;
        let total_page_count = leading_zero
            .checked_add(memory_length)
            .and_then(|v| v.checked_add(page_mask))
            .ok_or(ImportFileRegionError::Overflow)?
            / HV_PAGE_SIZE;

        file.seek(std::io::SeekFrom::Start(file_offset))
            .map_err(ImportFileRegionError::Seek)?;

        let mut pages_done: u64 = 0;
        let mut file_remaining = file_length;

        while file_remaining > 0 {
            let chunk_pages = (total_page_count - pages_done).min(buf_pages);
            let chunk_bytes = (chunk_pages * HV_PAGE_SIZE) as usize;
            let chunk_buf = &mut buf[..chunk_bytes];

            let data_start = if pages_done == 0 {
                leading_zero as usize
            } else {
                0
            };
            let data_len = file_remaining.min((chunk_bytes - data_start) as u64) as usize;

            // Zero leading padding on the first chunk.
            chunk_buf[..data_start].fill(0);

            // Read file data.
            file.read_exact(&mut chunk_buf[data_start..data_start + data_len])
                .map_err(ImportFileRegionError::Read)?;

            file_remaining -= data_len as u64;

            // On the last chunk with file data, extend page_count to cover all
            // remaining pages. import_pages will zero beyond the data.
            let import_page_count = if file_remaining == 0 {
                total_page_count - pages_done
            } else {
                chunk_pages
            };

            importer
                .import_pages(
                    page_base + pages_done,
                    import_page_count,
                    tag,
                    acceptance,
                    &chunk_buf[..data_start + data_len],
                )
                .map_err(ImportFileRegionError::ImportPages)?;

            pages_done += import_page_count;
        }

        // No file data at all — just import zero pages.
        if file_length == 0 {
            importer
                .import_pages(page_base, total_page_count, tag, acceptance, &[])
                .map_err(ImportFileRegionError::ImportPages)?;
        }

        Ok(())
    }

    /// Read a file in chunks and compute its CRC32, rewinding it afterward.
    pub fn crc32(&mut self, file: &mut dyn ReadSeek, len: u64) -> Result<u32, std::io::Error> {
        file.seek(std::io::SeekFrom::Start(0))?;
        let mut hasher = crc32fast::Hasher::new();
        let mut remaining = len;
        while remaining > 0 {
            let to_read = remaining.min(self.0.len() as u64) as usize;
            file.read_exact(&mut self.0[..to_read])?;
            hasher.update(&self.0[..to_read]);
            remaining -= to_read as u64;
        }
        file.rewind()?;
        Ok(hasher.finalize())
    }
}
