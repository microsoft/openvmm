// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use loader::importer::X86Register;
use thiserror::Error;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;

#[derive(Debug, Error)]
pub enum Error {
    #[error("pcat loader error")]
    Loader(#[source] loader::pcat::Error),
}

/// Load the PCAT BIOS.
///
/// Since the BIOS is in ROM, this actually just returns the PCAT initial
/// registers.
#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_pcat(gm: &GuestMemory, mem_layout: &MemoryLayout) -> Result<Vec<X86Register>, Error> {
    let mut loader = Loader::new(gm.clone(), mem_layout, hvdef::Vtl::Vtl0);
    loader::pcat::load(&mut loader, None, mem_layout.max_ram_below_4gb()).map_err(Error::Loader)?;
    Ok(loader.initial_regs())
}
