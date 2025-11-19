// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Helper for loading an ELF kernel image.

use crate::importer::GuestArch;
use crate::importer::GuestArchKind;
use crate::importer::ImageLoad;
use hvdef::HV_PAGE_SIZE;
use object::ReadCache;
use object::ReadRef;
use object::elf;
use object::read::elf::FileHeader;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;

type LE = object::LittleEndian;
const LE: LE = LE {};

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read file header")]
    ReadFileHeader,
    #[error("invalid file header")]
    InvalidFileHeader,
    #[error("target machine mismatch")]
    TargetMachineMismatch,
    #[error("unsupported ELF file byte order")]
    BigEndianElfOnLittle,
    #[error(
        "invalid entry address found in ELF header: {e_entry:#x}, start address: {start_address:#x}, load offset: {load_offset:#x}"
    )]
    InvalidEntryAddress {
        e_entry: u64,
        start_address: u64,
        load_offset: u64,
    },
    #[error("failed to parse ELF program header")]
    InvalidProgramHeader(#[source] object::read::Error),
    #[error("adding load offset {load_offset} to paddr {p_paddr} overflowed")]
    LoadOffsetOverflow { load_offset: u64, p_paddr: u64 },
    #[error("invalid ELF program header memory offset {mem_offset}, below start {start_address}")]
    InvalidProgramHeaderMemoryOffset { mem_offset: u64, start_address: u64 },
    #[error(
        "adding reloc bias {reloc_bias} and load offset {load_offset} to paddr {p_paddr} overflowed"
    )]
    RelocBiasOverflow {
        load_offset: u64,
        reloc_bias: u64,
        p_paddr: u64,
    },
    #[error("failed to read kernel image")]
    ReadKernelImage,
    #[error("failed during import pages call")]
    ImportPages(#[source] anyhow::Error),
    #[error("failed to seek to offset of kernel image")]
    SeekKernelImage,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Information about the loaded ELF image.
#[derive(Debug)]
pub struct LoadInfo {
    /// The minimum physical address used when loading the ELF image. This may be different from the start_address
    /// provided, as the ELF image controls where it should be loaded.
    pub minimum_address_used: u64,
    /// The next available physical address after the kernel was loaded.
    pub next_available_address: u64,
    /// The entrypoint of the image.
    pub entrypoint: u64,
}

/// Loads a kernel from a vmlinux elf image to a slice
///
/// # Arguments
///
/// * `guest_mem` - The guest memory region the kernel is written to.
/// * `kernel_image` - Input vmlinux image.
/// * `start_address` - For x86_64, this is the start of the high memory. Kernel should reside above it.
/// * `load_offset` - The offset to add to each loaded address.
/// * `assume_pic` - Assume that the image contains Position-Independent Code.
/// * `acceptance` - The page acceptance type for pages in the kernel.
/// * `tag` - The tag used to report igvm imports.
///
/// Returns (minimum offset written, maximum offset written, entry address of the kernel).
pub fn load_static_elf<F, R: GuestArch>(
    importer: &mut dyn ImageLoad<R>,
    kernel_image: &mut F,
    start_address: u64,
    load_offset: u64,
    assume_pic: bool,
    acceptance: crate::importer::BootPageAcceptance,
    tag: &str,
) -> Result<LoadInfo>
where
    F: Read + Seek,
{
    let reader = ReadCache::new(kernel_image);
    let ehdr: &elf::FileHeader64<LE> = reader.read_at(0).map_err(|_| Error::ReadFileHeader)?;

    // Sanity checks
    if !ehdr.is_supported() {
        return Err(Error::InvalidFileHeader);
    }
    if ehdr.is_big_endian() {
        return Err(Error::BigEndianElfOnLittle);
    }

    match R::arch() {
        GuestArchKind::Aarch64 => {
            if ehdr.e_machine != object::U16::new(object::LittleEndian, elf::EM_AARCH64) {
                tracing::error!(
                    "ELF file target machine mismatch, was the file built for aarch64?"
                );
                return Err(Error::TargetMachineMismatch);
            }
        }
        GuestArchKind::X86_64 => {
            if ehdr.e_machine != object::U16::new(object::LittleEndian, elf::EM_X86_64) {
                tracing::error!("ELF file target machine mismatch, was the file built for X86_64?");
                return Err(Error::TargetMachineMismatch);
            }
        }
    }

    let e_entry = ehdr.e_entry.get(LE);
    let phdrs = ehdr
        .program_headers(LE, &reader)
        .map_err(Error::InvalidProgramHeader)?;

    // For PIC kernels, calculate load offset by checking lowest paddr in program headers.
    // If it is below start_address, relocate kernel upward. Handles both:
    // - Old kernels (< v6.17): startup code is in .head.text at start of .text, low entry point,
    //   and matches physical load address
    // - New kernels (≥ v6.17): startup code is in .init.text (commit: "x86/boot: Move startup code out of __head section"),
    //   high entry point but low physical load address
    let load_offset = if assume_pic {
        let mut lowest_paddr = u64::MAX;
        for phdr in phdrs {
            if phdr.p_type.get(LE) == elf::PT_LOAD {
                let p_paddr = phdr.p_paddr.get(LE);
                lowest_paddr = lowest_paddr.min(p_paddr);
            }
        }
        if lowest_paddr < start_address {
            start_address - lowest_paddr + load_offset
        } else {
            load_offset
        }
    } else {
        load_offset
    };

    let entry = e_entry
        .checked_add(load_offset)
        .ok_or(Error::InvalidEntryAddress {
            e_entry,
            start_address,
            load_offset,
        })?;
    if entry < start_address {
        return Err(Error::InvalidEntryAddress {
            e_entry,
            start_address,
            load_offset,
        });
    }

    // The first pass on the sections provides the layout data
    let (lowest_addr, last_offset, reloc_bias) = {
        let mut lowest_addr = u64::MAX;
        let mut last_offset = load_offset;

        // Read in each section pointed to by the program headers.
        for phdr in phdrs {
            if phdr.p_type.get(LE) != elf::PT_LOAD {
                continue;
            }

            let p_paddr = phdr.p_paddr.get(LE);
            let mem_offset = p_paddr
                .checked_add(load_offset)
                .ok_or(Error::LoadOffsetOverflow {
                    load_offset,
                    p_paddr,
                })?;

            if mem_offset < start_address {
                return Err(Error::InvalidProgramHeaderMemoryOffset {
                    mem_offset,
                    start_address,
                });
            }

            let page_mask = HV_PAGE_SIZE - 1;
            let page_base = mem_offset / HV_PAGE_SIZE;
            let page_count: u64 =
                ((mem_offset & page_mask) + phdr.p_memsz.get(LE) + page_mask) / HV_PAGE_SIZE;

            lowest_addr = lowest_addr.min(page_base * HV_PAGE_SIZE);
            last_offset = last_offset.max((page_base + page_count) * HV_PAGE_SIZE);
        }

        (
            lowest_addr,
            last_offset,
            if assume_pic {
                lowest_addr - start_address
            } else {
                0
            },
        )
    };

    // During the second pass, read in each section pointed to by the program headers,
    // and import into the guest memory.
    for phdr in phdrs {
        if phdr.p_type.get(LE) != elf::PT_LOAD {
            continue;
        }

        let p_paddr = phdr.p_paddr.get(LE);
        let mem_offset = p_paddr
            .checked_add(load_offset)
            .ok_or(Error::LoadOffsetOverflow {
                load_offset,
                p_paddr,
            })?
            .checked_sub(reloc_bias)
            .ok_or(Error::RelocBiasOverflow {
                load_offset,
                reloc_bias,
                p_paddr,
            })?;

        if mem_offset < start_address {
            return Err(Error::InvalidProgramHeaderMemoryOffset {
                mem_offset,
                start_address,
            });
        }

        let page_mask = HV_PAGE_SIZE - 1;

        let filesz = phdr.p_filesz.get(LE);
        let mut v = vec![0; ((mem_offset & page_mask) + filesz) as usize];
        if filesz != 0 {
            let v_start_offset = (mem_offset & page_mask) as usize;
            let read_length = (v.len() - v_start_offset) as u64;
            v[v_start_offset..].copy_from_slice(
                reader
                    .read_bytes_at(phdr.p_offset.get(LE), read_length)
                    .map_err(|_| Error::ReadKernelImage)?,
            );
        }

        let page_base = mem_offset / HV_PAGE_SIZE;
        let page_count =
            ((mem_offset & page_mask) + phdr.p_memsz.get(LE) + page_mask) / HV_PAGE_SIZE;
        if page_count > 0 {
            importer
                .import_pages(page_base, page_count, tag, acceptance, &v)
                .map_err(Error::ImportPages)?;
        }
    }

    Ok(LoadInfo {
        minimum_address_used: lowest_addr - reloc_bias,
        next_available_address: last_offset - reloc_bias,
        entrypoint: entry - reloc_bias,
    })
}
