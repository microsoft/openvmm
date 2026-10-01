// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM-specific Linux boot configuration.

use super::linux::Error as LinuxError;
use super::linux::KernelConfig;
use super::linux::KernelIsolationConfig;
use guestmem::GuestMemory;
use loader::importer::X86Register;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use memory_range::MemoryRange;
use std::ffi::CString;
use std::io::Seek;
use vm_loader::InitialLoad;
use vm_loader::Loader;

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_linux_x86_mptable(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    apic_ids: &[u32],
    level_triggered_irqs: &[u32],
    reserved_memory_ranges: &[MemoryRange],
) -> Result<InitialLoad<X86Register>, LinuxError> {
    if !matches!(cfg.isolation, KernelIsolationConfig::None) {
        return Err(LinuxError::MpTableIsolation);
    }

    let mut kernel_file = cfg.kernel;
    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(LinuxError::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(LinuxError::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };
    let initrd_config = initrd_reader.as_mut().map(|reader| InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd: reader,
        size: initrd_size,
    });
    let cmdline = CString::new(cfg.cmdline).map_err(LinuxError::CommandLineNul)?;
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    loader::linux::microvm::load_x86_mptable(
        &mut loader,
        &mut kernel_file,
        initrd_config,
        &cmdline,
        cfg.mem_layout,
        &loader::mptable::MpTableConfig {
            apic_ids,
            level_triggered_irqs,
        },
        reserved_memory_ranges,
    )
    .map_err(LinuxError::Loader)?;
    Ok(loader.initial_regs_and_page_imports())
}
