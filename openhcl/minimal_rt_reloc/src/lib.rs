// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to apply relocations in an environment without a runtime.
//!
//! Do not reach out to global variables or function pointers (and the Rust
//! formatting facilities in particular or panic processing) from this code as
//! they generate relocation records.
//!
//! This is a separate crate from `minimal_rt` so that we can scope build
//! overrides, needed to prevent relocations from being generated in this code,
//! to just this module.

#![no_std]
// UNSAFETY: Manipulating instructions in memory.
#![expect(unsafe_code)]

/// Stores error code, line number, and the pointer to the file name in the registers.
/// Cannot call into the panic facilities before relocation, that won't be debuggable at all.
macro_rules! panic_no_relocs {
    ($code:expr) => {{
        let _code = $code;
        unsafe {
            #[cfg(target_arch = "x86_64")]
            core::arch::asm!("ud2", options(noreturn));
            #[cfg(target_arch = "aarch64")]
            core::arch::asm!("brk #0", options(noreturn));
        }
    }};
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
#[expect(dead_code)]
enum ElfDynTag {
    Null = 0,
    RelA = 7,
    RelASz = 8,
    RelAEnt = 9,
    Rel = 17,
    RelSz = 18,
    RelEnt = 19,
    RelACount = 0x6ffffff9,
    RelCount = 0x6ffffffa,
}

const R_ERROR_RELA: u64 = 1;
const R_ERROR_RELASZ: u64 = 2;
const R_ERROR_REL: u64 = 3;
const R_ERROR_RELSZ: u64 = 4;

#[cfg(target_arch = "x86_64")]
const R_RELATIVE: u32 = 8;
#[cfg(target_arch = "aarch64")]
const R_RELATIVE: u32 = 0x403;

#[derive(Clone, Copy)]
#[repr(C)]
struct Elf64Dyn {
    tag: ElfDynTag,
    val: usize,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Elf64Rela {
    offset: u64,
    info: u64,
    addend: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Elf64Rel {
    offset: u64,
    info: u64,
}

fn rela_type(rela: &Elf64Rela) -> u32 {
    rela.info as u32
}

fn rel_type(rel: &Elf64Rel) -> u32 {
    rel.info as u32
}

fn apply_rel(mapped_addr: u64, vaddr: u64, begin: usize, end: usize) {
    // SAFETY: constructing a slice of relocation records from
    // the pointer and the size coming from the `.dynamic` ELF section.
    let rel = unsafe {
        core::slice::from_raw_parts_mut(
            begin as *mut Elf64Rel,
            (end - begin) / size_of::<Elf64Rel>(),
        )
    };
    for rel in rel {
        if rel_type(rel) != R_RELATIVE {
            panic_no_relocs!(R_ERROR_REL)
        }

        let rel_addr = rel.offset.wrapping_add(mapped_addr) as *mut u64;

        // SAFETY: updating the address as prescribed by the ELF
        // ABI.
        unsafe {
            let rel = core::ptr::read_unaligned(rel_addr);
            core::ptr::write_unaligned(rel_addr, rel.wrapping_add(vaddr));
        }
    }
}

fn apply_rela(mapped_addr: u64, vaddr: u64, begin: usize, end: usize) {
    // SAFETY: constructing a slice of relocation records from
    // the pointer and the size coming from the `.dynamic` ELF section.
    let rela = unsafe {
        core::slice::from_raw_parts_mut(
            begin as *mut Elf64Rela,
            (end - begin) / size_of::<Elf64Rela>(),
        )
    };
    for rel in rela {
        if rela_type(rel) != R_RELATIVE {
            panic_no_relocs!(R_ERROR_RELA);
        }

        // SAFETY: updating the address as prescribed by the ELF
        // ABI.
        unsafe {
            core::ptr::write_unaligned(
                rel.offset.wrapping_add(mapped_addr) as *mut u64,
                rel.addend.wrapping_add(vaddr),
            );
        }
    }
}

/// Apply relocations to the image mapped at `mapped_addr` so that it can be run
/// at `vaddr`, using the _DYNAMIC section at `dynamic_addr`.
///
/// # Safety
/// The caller must ensure that this is called only during startup, with the
/// appropriate arguments, since this updates code and data across the binary.
pub unsafe extern "C" fn relocate(mapped_addr: usize, vaddr: usize, dynamic_addr: usize) {
    if mapped_addr == dynamic_addr {
        // Empty dynamic section or wrong linker flags (no PIE?),
        // exit
        return;
    }

    let mut rela_offset = None;
    let mut rela_entry_size = 0;
    let mut rela_count = 0;

    let mut rel_offset = None;
    let mut rel_entry_size = 0;
    let mut rel_count = 0;

    let mut dynamic = dynamic_addr as *mut Elf64Dyn;
    // SAFETY: Following the ELF specification. Not creating data races,
    // invalid values, dangling references, or modifying immutables.
    while unsafe { dynamic.read_unaligned().tag } != ElfDynTag::Null {
        // SAFETY: Following the ELF specification. Not creating data races,
        // invalid values, dangling references, or modifying immutables.
        let Elf64Dyn { tag, val } = unsafe { *dynamic };
        match tag {
            ElfDynTag::RelA => {
                rela_offset = Some(val);
            }
            ElfDynTag::RelAEnt => {
                rela_entry_size = val;
            }
            ElfDynTag::Rel => {
                rel_offset = Some(val);
            }
            ElfDynTag::RelEnt => {
                rel_entry_size = val;
            }
            ElfDynTag::RelACount => {
                rela_count = val;
            }
            ElfDynTag::RelCount => {
                rel_count = val;
            }
            _ => {}
        }

        dynamic = dynamic.wrapping_add(1);
    }

    if let Some(rela_offset) = rela_offset {
        const RELA_ENTRY_SIZE: usize = size_of::<Elf64Rela>();
        if rela_entry_size != RELA_ENTRY_SIZE {
            panic_no_relocs!(R_ERROR_RELASZ);
        }

        let begin = mapped_addr + rela_offset;
        let end = begin + rela_count * RELA_ENTRY_SIZE;
        apply_rela(mapped_addr as u64, vaddr as u64, begin, end);
    }

    if let Some(rel_offset) = rel_offset {
        const REL_ENTRY_SIZE: usize = size_of::<Elf64Rel>();
        if rel_entry_size != REL_ENTRY_SIZE {
            panic_no_relocs!(R_ERROR_RELSZ);
        }

        let begin = mapped_addr + rel_offset;
        let end = begin + rel_count * REL_ENTRY_SIZE;
        apply_rel(mapped_addr as u64, vaddr as u64, begin, end);
    }
}
