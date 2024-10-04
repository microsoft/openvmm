// Copyright (C) Microsoft Corporation. All rights reserved.

//! This module implements init routines to setup the Underhill environment to
//! run VTL0.

use hcl::ioctl::Hcl;
use vm_topology::memory::MemoryLayout;

/// Determines the VTL0 alias map bit.
pub fn vtl0_alias_map_bit(hcl: &mut Hcl, memory_layout: &MemoryLayout) -> Option<u64> {
    let vsm_capabilities = hcl.get_vsm_capabilities();
    tracing::trace!(?vsm_capabilities);
    // Disable the alias map on ARM because physical address size is not
    // reliably reported. Since the position of the alias map bit is inferred
    // from address size, the alias map is broken when the PA size is wrong.
    //
    // TODO: Once the alias map is enabled on ARM, update
    // underhill_mem::init::init to panic if the alias map is not available.
    #[cfg(guest_arch = "aarch64")]
    let vtl0_alias_map_enabled = false;
    #[cfg(guest_arch = "x86_64")]
    let vtl0_alias_map_enabled = vsm_capabilities.vtl0_alias_map_available();
    let vtl0_alias_map_bit: Option<u64> = if vtl0_alias_map_enabled {
        let alias_map_bit = memory_layout.physical_address_size() - 1;

        // TODO: Kernel won't support bits greater than 48. Need 5 level paging
        //       or some other kernel changes. If possible, would be good to not
        //       require 5 level paging and just further extend valid bits.
        if alias_map_bit > 48 {
            // BUGBUG: This needs to be fixed, but allow it with just an error
            // log for now.
            tracing::error!(alias_map_bit, "alias map bit larger than supported");
            None
        } else {
            tracing::info!(alias_map_bit, "enabling alias map");
            Some(1 << alias_map_bit)
        }
    } else {
        tracing::info!("alias map not supported");
        None
    };
    tracing::trace!(
        pas = memory_layout.physical_address_size(),
        ?vtl0_alias_map_bit
    );
    vtl0_alias_map_bit
}
