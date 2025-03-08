// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
pub mod aarch64;
mod cpuid;
mod generic;
pub mod io;
pub mod irqcon;
pub mod state;
pub mod x86;

pub use arch::*;
pub use cpuid::*;
pub use generic::*;
pub use vm_topology::processor::VpInfo;

mod arch {
    #[cfg(guest_arch = "x86_64")]
    mod x86 {
        #![cfg_attr(not(guest_arch = "x86_64"), allow(unused_imports))]
        pub use crate::x86::vm;
        pub use crate::x86::vp;
        pub use crate::x86::X86InitialRegs as InitialRegs;
        pub use crate::x86::X86PartitionCapabilities as PartitionCapabilities;
    }
    #[cfg(guest_arch = "aarch64")]
    mod aarch64 {
        #![cfg_attr(not(guest_arch = "aarch64"), allow(unused_imports))]
        pub use crate::aarch64::vm;
        pub use crate::aarch64::vp;
        pub use crate::aarch64::Aarch64InitialRegs as InitialRegs;
        pub use crate::aarch64::Aarch64PartitionCapabilities as PartitionCapabilities;
    }

    #[cfg(guest_arch = "aarch64")]
    pub use aarch64::*;
    #[cfg(guest_arch = "x86_64")]
    pub use x86::*;
}
