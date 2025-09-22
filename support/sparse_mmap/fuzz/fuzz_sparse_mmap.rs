// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use sparse_mmap::SparseMapping;
use sparse_mmap::alloc_shared_memory;
use std::fmt;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

/// Size constants
pub const SIZE_1KB: usize = 0x400;
pub const SIZE_1MB: usize = 1024 * SIZE_1KB;
pub const SIZE_1GB: usize = 1024 * SIZE_1MB;

/// WARNING: Tweak sizes at your own risk
/// Any size that is too big could trigger an OOM
/// error with libFuzzer.
const MAX_MAP_SIZE: usize = 2 * SIZE_1GB;
const MAX_BUFFER_SIZE: usize = SIZE_1KB;
const MAX_SLICE_SIZE: usize = SIZE_1KB;
const MAX_FILE_PAGES: usize = 256;
const MAX_RAND_OFFSETS: usize = 10;
const MAX_CUSTOM_USIZE: usize = 999;

#[derive(Debug, Arbitrary)]
enum DataType {
    U8,
    U16,
    U32,
    U64,
    U128,
}

#[derive(Debug, Arbitrary)]
enum SparseMappingAction {
    WriteAt { data: Vec<u8> },
    ReadAt { buffer_len: usize },
    ReadPlain { data_type: DataType },
    FillAt { len: usize, value: u8 },
    AtomicSlice { slice_len: usize },
    MapZero { len: usize },
    MapFile { file_offset: u64, writable: bool },
    Unmap { len: usize },
}

#[derive(Debug, Arbitrary, Eq, PartialEq, Hash, Copy, Clone)]
struct Block {
    offset: usize,
    len: usize,
}

impl Block {
    pub fn new(offset: usize, len: usize) -> Result<Block, arbitrary::Error> {
        if len == 0 {
            return Err(arbitrary::Error::IncorrectFormat);
        }
        offset
            .checked_add(len)
            .ok_or(arbitrary::Error::IncorrectFormat)?;
        Ok(Block { offset, len })
    }

    pub fn end(&self) -> usize {
        self.offset + self.len - 1
    }

    pub fn middle(&self) -> usize {
        self.offset + (self.len / 2)
    }
}

impl fmt::Display for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Block(offset: {:x}, len: {:x})", self.offset, self.len)
    }
}

#[derive(Arbitrary, Debug, Eq, PartialEq, Copy, Clone)]
pub enum MemoryLayout {
    Empty,
    Single,
    SingleFull,
    SingleExceeds,
    MultiFull,
    Middle,
    Ends,
    OutOfBounds,
    RandomPages,
    RandomEverything,
}

/// Using fisher-yates' algorithm, deterministically shuffles a slice on "u".
fn shuffle<T>(u: &mut Unstructured<'_>, slice: &mut [T]) -> Result<(), arbitrary::Error> {
    let len = slice.len();
    for i in 0..len {
        let j: usize = u.int_in_range(i..=len - 1)?;
        slice.swap(i, j);
    }
    Ok(())
}

/// Generate blocks based on a layout
fn generate_blocks(
    u: &mut Unstructured<'_>,
    layout: MemoryLayout,
    map_len: usize,
) -> Result<Vec<Block>, arbitrary::Error> {
    // Helper macro to safely multiply two numbers
    macro_rules! safe_mul {
        ($a:expr, $b:expr) => {
            $a.checked_mul($b)
                .ok_or(arbitrary::Error::IncorrectFormat)?
        };
    }

    let page_size = SparseMapping::page_size();
    let max_pages = map_len / page_size;

    let blocks = match layout {
        MemoryLayout::Empty => vec![],
        MemoryLayout::Single => {
            let offset_pages = u.int_in_range(0..=max_pages)?;
            let length_pages = u.int_in_range(0..=max_pages - offset_pages)?;
            let block = Block::new(
                safe_mul!(offset_pages, page_size),
                safe_mul!(length_pages, page_size),
            )?;
            vec![block]
        }
        MemoryLayout::SingleFull => {
            let block = Block::new(0, safe_mul!(max_pages, page_size))?;
            vec![block]
        }
        MemoryLayout::SingleExceeds => {
            let len_pages = max_pages + u.int_in_range(1..=MAX_CUSTOM_USIZE)?;
            let block = Block::new(0, safe_mul!(len_pages, page_size))?;
            vec![block]
        }
        MemoryLayout::MultiFull => {
            let num_blocks = u.int_in_range(1..=max_pages)?;
            let pages_per_block = max_pages / num_blocks;
            let mut blocks = Vec::with_capacity(num_blocks);

            for i in 0..num_blocks {
                let offset = i * safe_mul!(pages_per_block, page_size);
                let len = safe_mul!(pages_per_block, page_size);
                let block = Block::new(offset, len)?;
                blocks.push(block);
            }

            if !max_pages.is_multiple_of(num_blocks) {
                let offset = num_blocks * safe_mul!(pages_per_block, page_size);
                let len = (max_pages % num_blocks) * page_size;
                let block = Block::new(offset, len)?;
                blocks.push(block);
            }

            blocks
        }
        MemoryLayout::Middle => {
            let len_pages = u.int_in_range(1..=max_pages)?;
            let offset_pages = (max_pages - len_pages) / 2;
            let block = Block::new(
                safe_mul!(offset_pages, page_size),
                safe_mul!(len_pages, page_size),
            )?;
            vec![block]
        }
        MemoryLayout::Ends => {
            let length_pages = u.int_in_range(1..=max_pages.div_ceil(2))?;
            let block1 = Block::new(0, safe_mul!(length_pages, page_size))?;
            let block2 = Block::new(
                safe_mul!((max_pages - length_pages), page_size),
                safe_mul!(length_pages, page_size),
            )?;
            vec![block1, block2]
        }
        MemoryLayout::OutOfBounds => {
            let offset_pages = max_pages + u.int_in_range(1..=MAX_CUSTOM_USIZE)?;
            let length_pages = u.int_in_range(1..=max_pages)?;
            let block = Block::new(
                safe_mul!(offset_pages, page_size),
                safe_mul!(length_pages, page_size),
            )?;
            vec![block]
        }
        MemoryLayout::RandomPages => {
            let num_blocks = u.int_in_range(1..=max_pages)?;
            let mut blocks = Vec::with_capacity(num_blocks);
            for _ in 0..num_blocks {
                let offset_pages = u.int_in_range(0..=max_pages)?;
                let length_pages = u.int_in_range(0..=max_pages - offset_pages)?;
                let block = Block::new(
                    safe_mul!(offset_pages, page_size),
                    safe_mul!(length_pages, page_size),
                )?;
                blocks.push(block);
            }
            blocks
        }
        MemoryLayout::RandomEverything => {
            let num_blocks = u.int_in_range(1..=max_pages)?;
            let mut blocks = Vec::with_capacity(num_blocks);
            for _ in 0..num_blocks {
                let offset = u.int_in_range(0..=map_len)?;
                let length = u.int_in_range(0..=map_len - offset)?;
                let block = Block::new(offset, length)?;
                blocks.push(block);
            }
            blocks
        }
    };

    Ok(blocks)
}

/// Generates some test offsets to use with actions
fn generate_test_offsets(
    u: &mut Unstructured<'_>,
    blocks: &Vec<Block>,
    map_len: usize,
) -> Result<Vec<usize>, arbitrary::Error> {
    // Prepare some offsets to test with
    let mut offsets = Vec::new();

    // Add offsets at block's start, middle, end and random
    for block in blocks {
        offsets.push(block.offset);
        offsets.push(block.middle());
        offsets.push(block.end());
        offsets.push(u.int_in_range(block.offset..=block.end())?);
    }

    // Add 1 random OOB offset
    offsets.push(u.int_in_range(map_len + 1..=usize::MAX)?);

    // Add 1 random offsets for each gap between blocks
    let mut last_end = 0;
    let mut gaps: Vec<usize> = Vec::new();
    for block in blocks {
        if block.offset > last_end {
            gaps.push(u.int_in_range(last_end..=block.offset)?);
        }
        last_end = block.end();
    }
    offsets.extend(gaps);

    // Add random offsets
    let num_rand_offsets = u.int_in_range(1..=MAX_RAND_OFFSETS)?;
    for _ in 0..num_rand_offsets {
        let offset: usize = u.arbitrary()?;
        offsets.push(offset);
    }

    // Shuffle offsets
    shuffle(u, &mut offsets)?;

    Ok(offsets)
}

/// Executes an action on a sparse mapping across all offsets
fn exec_action(
    u: &mut Unstructured<'_>,
    action: SparseMappingAction,
    mapping: &SparseMapping,
    offsets: &Vec<usize>,
) -> Result<(), arbitrary::Error> {
    fuzz_eprintln!("Executing action: {:?}", action);
    match action {
        SparseMappingAction::WriteAt { data } => {
            for offset in offsets {
                let _ = mapping.write_at(*offset, &(data));
            }
        }
        SparseMappingAction::ReadAt { buffer_len } => {
            let mut buffer = vec![0; buffer_len.min(MAX_BUFFER_SIZE)];
            for offset in offsets {
                let _ = mapping.read_at(*offset, &mut buffer);
            }
        }
        SparseMappingAction::ReadPlain { data_type } => {
            match data_type {
                DataType::U8 => {
                    for offset in offsets {
                        let _ = mapping.read_plain::<u8>(*offset);
                    }
                }
                DataType::U16 => {
                    for offset in offsets {
                        let _ = mapping.read_plain::<u16>(*offset);
                    }
                }
                DataType::U32 => {
                    for offset in offsets {
                        let _ = mapping.read_plain::<u32>(*offset);
                    }
                }
                DataType::U64 => {
                    for offset in offsets {
                        let _ = mapping.read_plain::<u64>(*offset);
                    }
                }
                DataType::U128 => {
                    for offset in offsets {
                        let _ = mapping.read_plain::<u128>(*offset);
                    }
                }
            };
        }
        SparseMappingAction::FillAt { len, value } => {
            for offset in offsets {
                let _ = mapping.fill_at(*offset, value, len);
            }
        }
        SparseMappingAction::AtomicSlice { slice_len } => {
            let adjusted_slice_len = slice_len.min(MAX_SLICE_SIZE);
            let map_len = mapping.len();
            for start in offsets {
                if map_len >= *start && map_len - start >= adjusted_slice_len {
                    let _ = mapping.atomic_slice(*start, adjusted_slice_len);
                }
            }
        }
        SparseMappingAction::MapZero { len } => {
            for offset in offsets {
                let _ = mapping.map_zero(*offset, len);
            }
        }
        SparseMappingAction::MapFile {
            file_offset,
            writable,
        } => {
            let page_size = SparseMapping::page_size();
            let len_pages = u.int_in_range(1..=MAX_FILE_PAGES)?;
            let len = len_pages
                .checked_mul(page_size)
                .ok_or(arbitrary::Error::IncorrectFormat)?;
            match alloc_shared_memory(len) {
                Ok(fd) => {
                    for offset in offsets {
                        match mapping.map_file(*offset, len, &fd, file_offset, writable) {
                            Ok(_) => {
                                fuzz_eprintln!("MapFile'd file successfully");
                            }
                            Err(e) => {
                                fuzz_eprintln!("Error mapping file: {:?}", e)
                            }
                        }
                    }
                }
                Err(e) => {
                    fuzz_eprintln!("Error creating fd: {:?}", e)
                }
            }
        }
        SparseMappingAction::Unmap { len } => {
            for offset in offsets {
                match mapping.unmap(*offset, len) {
                    Ok(_) => {
                        fuzz_eprintln!("Unmapped file successfully");
                    }
                    Err(e) => {
                        fuzz_eprintln!("Error unmapping file: {:?}", e)
                    }
                }
            }
        }
    }

    Ok(())
}

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    // Generate mapping. Expected failures should only be caused
    // by a size of 0 or overflow.
    let mut map_len = u.int_in_range(0..=MAX_MAP_SIZE)?;
    let mapping = match SparseMapping::new(map_len) {
        Ok(mapping) => mapping,
        Err(e) => match e.kind() {
            std::io::ErrorKind::InvalidInput => match e.to_string().as_str() {
                "length must be greater than 0" => {
                    fuzz_eprintln!("Generated a mapping of length 0, which isn't allowed.");
                    return Err(arbitrary::Error::IncorrectFormat);
                }
                "length and alignment combination causes overflow" => {
                    fuzz_eprintln!("Generated a mapping with a length that causes overflow.");
                    return Err(arbitrary::Error::IncorrectFormat);
                }
                _ => panic!("Unexpected error: {:?}", e),
            },
            _ => panic!("Unexpected error: {:?}", e),
        },
    };
    map_len = mapping.len();
    fuzz_eprintln!("Generated mapping of length: {:x}", map_len);

    // Get layout
    let layout: MemoryLayout = u.arbitrary()?;
    fuzz_eprintln!("Using layout: {:?}", layout);

    // Get blocks
    let blocks: Vec<Block> = generate_blocks(u, layout, map_len)?;
    fuzz_eprintln!("Generated blocks: {:?}", blocks);

    // Try to allocate blocks. Expected failures should only be caused by blocks
    // with invalid length and offset requirements from SparseMapping::validate_offset_len
    // which gives an InvalidInput error.
    let mut no_failures = true;
    for block in &blocks {
        if let Err(e) = mapping.alloc(block.offset, block.len) {
            match e.kind() {
                std::io::ErrorKind::InvalidInput => {
                    fuzz_eprintln!("Block had invalid offset and length: {:?}", block);
                }
                _ => {
                    panic!(
                        "Allocation failure led to unexpected error:\n\
                        Block: {:?}\n\
                        Layout: {:?}\n\
                        Map length: {:x}\n\
                        Error: {:?}",
                        block, layout, map_len, e
                    );
                }
            }
            no_failures = false;
        }
    }
    if no_failures && layout != MemoryLayout::Empty {
        fuzz_eprintln!("All blocks were successfully allocated!");
    }
    fuzz_eprintln!("Done allocating blocks...");

    // Get test offsets
    let offsets = generate_test_offsets(u, &blocks, map_len)?;
    fuzz_eprintln!(
        "Acquired test offsets: {:?}",
        offsets
            .iter()
            .map(|offset| format!("{:x}", offset))
            .collect::<Vec<_>>()
    );

    // Perform various actions for all offsets
    while !u.is_empty() {
        let action = u.arbitrary()?;
        let _ = exec_action(u, action, &mapping, &offsets);
    }

    Ok(())
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
