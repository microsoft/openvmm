// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The effective x86 CPU compatibility contract that a snapshot restore
//! destination must reproduce exactly.

use super::X86PartitionCapabilities;

/// Canonical CPUID leaf in a saved CPU compatibility contract.
#[derive(Debug, Clone, PartialEq, Eq, mesh_protobuf::Protobuf)]
#[mesh(package = "virt.x86")]
pub struct CpuContractCpuidLeaf {
    #[mesh(1)]
    pub function: u32,
    #[mesh(2)]
    pub index: Option<u32>,
    #[mesh(3)]
    pub result: [u32; 4],
    #[mesh(4)]
    pub mask: [u32; 4],
}

/// XSAVE component layout in a saved CPU compatibility contract.
#[derive(Debug, Clone, PartialEq, Eq, mesh_protobuf::Protobuf)]
#[mesh(package = "virt.x86")]
pub struct CpuContractXsaveComponent {
    #[mesh(1)]
    pub index: u32,
    #[mesh(2)]
    pub offset: u32,
    #[mesh(3)]
    pub length: u32,
    #[mesh(4)]
    pub align: bool,
}

/// Effective CPU contract that a destination must reproduce exactly.
#[derive(Debug, Clone, PartialEq, Eq, mesh_protobuf::Protobuf)]
#[mesh(package = "virt.x86")]
pub struct CpuCompatibilityContract {
    #[mesh(1)]
    pub vendor: [u8; 12],
    #[mesh(2)]
    pub cpuid: Vec<CpuContractCpuidLeaf>,
    #[mesh(3)]
    pub xcr0_supported: u64,
    #[mesh(4)]
    pub xss_supported: u64,
    #[mesh(5)]
    pub xsave_standard_len: u32,
    #[mesh(6)]
    pub xsave_compact_len: u32,
    #[mesh(7)]
    pub xsave_components: Vec<CpuContractXsaveComponent>,
    #[mesh(8)]
    pub x2apic: bool,
    #[mesh(9)]
    pub x2apic_enabled: bool,
    #[mesh(10)]
    pub cet: bool,
    #[mesh(11)]
    pub cet_ss: bool,
    #[mesh(12)]
    pub sgx: bool,
    #[mesh(13)]
    pub tsc_aux: bool,
    #[mesh(14)]
    pub physical_address_width: u32,
    #[mesh(15)]
    pub tsc_deadline: bool,
}

impl CpuCompatibilityContract {
    /// Builds a canonical contract from effective partition state.
    pub fn new(caps: &X86PartitionCapabilities, cpuid: &crate::CpuidLeafSet) -> Self {
        Self {
            vendor: caps.vendor.0,
            cpuid: cpuid
                .leaves()
                .iter()
                .map(|leaf| CpuContractCpuidLeaf {
                    function: leaf.function,
                    index: leaf.index,
                    result: leaf.result,
                    mask: leaf.mask,
                })
                .collect(),
            xcr0_supported: caps.xsave.features,
            xss_supported: caps.xsave.supervisor_features,
            xsave_standard_len: caps.xsave.standard_len,
            xsave_compact_len: caps.xsave.compact_len,
            xsave_components: caps
                .xsave
                .feature_info
                .iter()
                .enumerate()
                .filter(|(_, feature)| feature.len != 0)
                .map(|(index, feature)| CpuContractXsaveComponent {
                    index: index as u32,
                    offset: feature.offset,
                    length: feature.len,
                    align: feature.align,
                })
                .collect(),
            x2apic: caps.x2apic,
            x2apic_enabled: caps.x2apic_enabled,
            cet: caps.cet,
            cet_ss: caps.cet_ss,
            sgx: caps.sgx,
            tsc_aux: caps.tsc_aux,
            physical_address_width: u32::from(caps.physical_address_width),
            tsc_deadline: caps.tsc_deadline,
        }
    }
}
