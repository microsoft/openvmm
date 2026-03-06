// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for the VM worker.

use guid::Guid;
use input_core::InputData;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use net_backend_resources::mac_address::MacAddress;
use openvmm_pcat_locator::RomFileLocation;
use std::fmt;
use std::fs::File;
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;
use vmotherboard::options::BaseChipsetManifest;

#[derive(MeshPayload, Debug)]
pub struct Config {
    pub load_mode: LoadMode,
    pub floppy_disks: Vec<floppy_resources::FloppyDiskConfig>,
    pub ide_disks: Vec<ide_resources::IdeDeviceConfig>,
    pub pcie_root_complexes: Vec<PcieRootComplexConfig>,
    pub pcie_devices: Vec<PcieDeviceConfig>,
    pub pcie_switches: Vec<PcieSwitchConfig>,
    pub vpci_devices: Vec<VpciDeviceConfig>,
    pub memory: MemoryConfig,
    pub processor_topology: ProcessorTopologyConfig,
    pub hypervisor: HypervisorConfig,
    pub chipset: BaseChipsetManifest,
    pub vmbus: Option<VmbusConfig>,
    pub vtl2_vmbus: Option<VmbusConfig>,
    #[cfg(windows)]
    pub kernel_vmnics: Vec<KernelVmNicConfig>,
    pub input: mesh::Receiver<InputData>,
    pub framebuffer: Option<framebuffer::Framebuffer>,
    pub vga_firmware: Option<RomFileLocation>,
    pub vtl2_gfx: bool,
    pub virtio_devices: Vec<(VirtioBus, Resource<VirtioDeviceHandle>)>,
    #[cfg(windows)]
    pub vpci_resources: Vec<virt_whp::device::DeviceHandle>,
    pub vmgs: Option<VmgsResource>,
    pub secure_boot_enabled: bool,
    pub custom_uefi_vars: firmware_uefi_custom_vars::CustomVars,
    // TODO: move FirmwareEvent somewhere not GED-specific.
    pub firmware_event_send: Option<mesh::Sender<get_resources::ged::FirmwareEvent>>,
    pub debugger_rpc: Option<mesh::Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
    pub vmbus_devices: Vec<(DeviceVtl, Resource<VmbusDeviceHandleKind>)>,
    pub chipset_devices: Vec<ChipsetDeviceHandle>,
    pub generation_id_recv: Option<mesh::Receiver<[u8; 16]>>,
    // This is used for testing. TODO: resourcify, and also store this in VMGS.
    pub rtc_delta_milliseconds: i64,
    /// allow the guest to reset without notifying the client
    pub automatic_guest_reset: bool,
    pub efi_diagnostics_log_level: EfiDiagnosticsLogLevelType,
}

// ARM64 needs a larger low gap.
const DEFAULT_LOW_MMAP_GAP_SIZE_X86: u64 = 1024 * 1024 * 128;
const DEFAULT_LOW_MMAP_GAP_SIZE_AARCH64: u64 = 1024 * 1024 * 512;

/// Default mmio gaps for an x86 partition.
pub const DEFAULT_MMIO_GAPS_X86: [MemoryRange; 2] = [
    MemoryRange::new(0x1_0000_0000 - DEFAULT_LOW_MMAP_GAP_SIZE_X86..0x1_0000_0000), // nMB just below 4GB
    MemoryRange::new(0xF_E000_0000..0x10_0000_0000), // 512MB just below 64GB, then up to 64GB
];

/// Default mmio gaps for x86 if VTL2 is enabled.
pub const DEFAULT_MMIO_GAPS_X86_WITH_VTL2: [MemoryRange; 3] = [
    MemoryRange::new(0x1_0000_0000 - DEFAULT_LOW_MMAP_GAP_SIZE_X86..0x1_0000_0000), // nMB just below 4GB
    MemoryRange::new(0xF_E000_0000..0x20_0000_0000), // 512MB just below 64GB, then up to 128GB
    MemoryRange::new(0x20_0000_0000..0x20_4000_0000), // 128GB to 129 GB
];

/// Default mmio gaps for an aarch64 partition.
pub const DEFAULT_MMIO_GAPS_AARCH64: [MemoryRange; 2] = [
    MemoryRange::new(0x1_0000_0000 - DEFAULT_LOW_MMAP_GAP_SIZE_AARCH64..0x1_0000_0000), // nMB just below 4GB
    MemoryRange::new(0xF_E000_0000..0x10_0000_0000), // 512MB just below 64GB, then up to 64GB
];

/// Default mmio gaps for aarch64 if VTL2 is enabled.
pub const DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2: [MemoryRange; 3] = [
    MemoryRange::new(0x1_0000_0000 - DEFAULT_LOW_MMAP_GAP_SIZE_AARCH64..0x1_0000_0000), // nMB just below 4GB
    MemoryRange::new(0xF_E000_0000..0x20_0000_0000), // 512MB just below 64GB, then up to 128GB
    MemoryRange::new(0x20_0000_0000..0x20_4000_0000), // 128GB to 129 GB
];

pub const DEFAULT_GIC_DISTRIBUTOR_BASE: u64 = 0xFFFF_0000;
// The KVM in-kernel vGICv3 requires the distributor and redistributor bases be 64KiB aligned.
pub const DEFAULT_GIC_REDISTRIBUTORS_BASE: u64 = if cfg!(target_os = "linux") {
    0xEFFF_0000
} else {
    0xEFFE_E000
};

pub const DEFAULT_PCIE_ECAM_BASE: u64 = 0x8_0000_0000; // 32GB, size depends on configuration

#[derive(MeshPayload, Debug)]
pub enum LoadMode {
    Linux {
        kernel: File,
        initrd: Option<File>,
        cmdline: String,
        enable_serial: bool,
        custom_dsdt: Option<Vec<u8>>,
    },
    Uefi {
        firmware: File,
        enable_debugging: bool,
        enable_memory_protections: bool,
        disable_frontpage: bool,
        enable_tpm: bool,
        enable_battery: bool,
        enable_serial: bool,
        enable_vpci_boot: bool,
        uefi_console_mode: Option<UefiConsoleMode>,
        default_boot_always_attempt: bool,
        bios_guid: Guid,
    },
    Pcat {
        firmware: RomFileLocation,
        boot_order: [PcatBootDevice; 4],
    },
    Igvm {
        file: File,
        cmdline: String,
        vtl2_base_address: Vtl2BaseAddressType,
        com_serial: Option<SerialInformation>,
    },
    None,
}

#[derive(Debug, Clone, Copy, MeshPayload)]
pub struct SerialInformation {
    pub io_port: u16,
    pub irq: u32,
}

/// Different types to specify the base address for the VTL2 region of the IGVM
/// file.
#[derive(Debug, Clone, Copy, MeshPayload)]
pub enum Vtl2BaseAddressType {
    /// Use the addresses specified in the file. The IGVM file does not need to
    /// support relocations.
    File,
    /// Put VTL2 at the specified address. The IGVM file must support
    /// relocations.
    Absolute(u64),
    /// Use the specified range in the supplied MemoryLayout, as the caller has
    /// created a specific range for VTL2. The IGVM file must support
    /// relocations.
    ///
    /// An optional size may be specified to override the size describing VTL2
    /// provided in the IGVM file. It must be larger than the IGVM file provided
    /// size.
    MemoryLayout { size: Option<u64> },
    /// Tell VTL2 to allocate out it's own memory. This will load the file at
    /// the base address specified in the file, and the host will tell VTL2 the
    /// size of memory to allocate for itself.
    ///
    /// An optional size may be specified to override the size describing VTL2
    /// provided in the IGVM file. It must be larger than the IGVM file provided
    /// size.
    Vtl2Allocate { size: Option<u64> },
}

#[derive(Debug, MeshPayload)]
pub struct PcieRootComplexConfig {
    pub index: u32,
    pub name: String,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
    pub low_mmio_size: u32,
    pub high_mmio_size: u64,
    pub ports: Vec<PcieRootPortConfig>,
}

#[derive(Debug, MeshPayload)]
pub struct PcieRootPortConfig {
    pub name: String,
    pub hotplug: bool,
}

#[derive(Debug, MeshPayload)]
pub struct PcieSwitchConfig {
    pub name: String,
    pub num_downstream_ports: u8,
    pub parent_port: String,
    pub hotplug: bool,
}

#[derive(Debug, MeshPayload)]
pub struct PcieDeviceConfig {
    pub port_name: String,
    pub resource: Resource<PciDeviceHandleKind>,
}

#[derive(Debug, MeshPayload)]
pub struct VpciDeviceConfig {
    pub vtl: DeviceVtl,
    /// The ID of the device. Vpci devices are identified by a portion of `data2` and `data3` of the
    /// instance ID, which is used to generate the guest-visible device ID.
    pub instance_id: Guid,
    pub resource: Resource<PciDeviceHandleKind>,
}

#[derive(Debug, Protobuf)]
pub struct ProcessorTopologyConfig {
    pub proc_count: u32,
    pub vps_per_socket: Option<u32>,
    pub enable_smt: Option<bool>,
    pub arch: Option<ArchTopologyConfig>,
}

#[derive(Debug, Protobuf, Default, Clone)]
pub struct X86TopologyConfig {
    pub apic_id_offset: u32,
    pub x2apic: X2ApicConfig,
}

#[derive(Debug, Default, Copy, Clone, Protobuf)]
pub enum X2ApicConfig {
    #[default]
    /// Support the X2APIC if recommended by the hypervisor or if needed by the
    /// topology configuration.
    Auto,
    /// Support the X2APIC, and automatically enable it if needed to address all
    /// processors.
    Supported,
    /// Do not support the X2APIC.
    Unsupported,
    /// Support and enable the X2APIC.
    Enabled,
}

#[derive(Debug, Protobuf, Default, Clone)]
pub enum PmuGsivConfig {
    #[default]
    /// Use the hypervisor's platform GSIV value for the PMU.
    Platform,
    /// Use the specified GSIV value for the PMU.
    Gsiv(u32),
}

#[derive(Debug, Protobuf, Default, Clone)]
pub struct Aarch64TopologyConfig {
    pub gic_config: Option<GicConfig>,
    pub pmu_gsiv: PmuGsivConfig,
}

#[derive(Debug, Protobuf, Clone)]
pub struct GicConfig {
    pub gic_distributor_base: u64,
    pub gic_redistributors_base: u64,
}

#[derive(Debug, Protobuf, Clone)]
pub enum ArchTopologyConfig {
    X86(X86TopologyConfig),
    Aarch64(Aarch64TopologyConfig),
}

#[derive(Debug, MeshPayload)]
pub struct MemoryConfig {
    pub mem_size: u64,
    pub mmio_gaps: Vec<MemoryRange>,
    pub prefetch_memory: bool,
    pub pcie_ecam_base: u64,
}

#[derive(Debug, MeshPayload, Default)]
pub struct VmbusConfig {
    pub vsock_listener: Option<unix_socket::UnixListener>,
    pub vsock_path: Option<String>,
    pub vmbus_max_version: Option<u32>,
    #[cfg(windows)]
    pub vmbusproxy_handle: Option<vmbus_proxy::ProxyHandle>,
    pub vtl2_redirect: bool,
}

#[derive(Debug, MeshPayload, Default)]
pub struct HypervisorConfig {
    pub with_hv: bool,
    pub user_mode_hv_enlightenments: bool,
    pub user_mode_apic: bool,
    pub with_vtl2: Option<Vtl2Config>,
    pub with_isolation: Option<IsolationType>,
}

#[derive(Debug, Copy, Clone, MeshPayload)]
pub enum Hypervisor {
    Kvm,
    MsHv,
    Whp,
    Hvf,
}

impl fmt::Display for Hypervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Kvm => "kvm",
            Self::MsHv => "mshv",
            Self::Whp => "whp",
            Self::Hvf => "hvf",
        })
    }
}

#[derive(Debug, MeshPayload)]
pub struct KernelVmNicConfig {
    pub instance_id: Guid,
    pub mac_address: MacAddress,
    pub switch_port_id: SwitchPortId,
}

#[derive(Clone, Debug, MeshPayload)]
pub struct SwitchPortId {
    pub switch: Guid,
    pub port: Guid,
}

pub const DEFAULT_PCAT_BOOT_ORDER: [PcatBootDevice; 4] = [
    PcatBootDevice::Optical,
    PcatBootDevice::HardDrive,
    PcatBootDevice::Network,
    PcatBootDevice::Floppy,
];

#[derive(MeshPayload, Debug, Clone, Copy, PartialEq)]
pub enum PcatBootDevice {
    Floppy,
    HardDrive,
    Optical,
    Network,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone, MeshPayload)]
pub enum VirtioBus {
    Mmio,
    Pci,
}

/// Policy for the partition when mapping VTL0 memory late.
#[derive(Eq, PartialEq, Debug, Copy, Clone, MeshPayload)]
pub enum LateMapVtl0MemoryPolicy {
    /// Halt execution of the VP if VTL0 memory is accessed.
    Halt,
    /// Log the error but emulate the access with the instruction emulator.
    Log,
    /// Inject an exception into the guest.
    InjectException,
}

impl From<LateMapVtl0MemoryPolicy> for virt::LateMapVtl0MemoryPolicy {
    fn from(value: LateMapVtl0MemoryPolicy) -> Self {
        match value {
            LateMapVtl0MemoryPolicy::Halt => virt::LateMapVtl0MemoryPolicy::Halt,
            LateMapVtl0MemoryPolicy::Log => virt::LateMapVtl0MemoryPolicy::Log,
            LateMapVtl0MemoryPolicy::InjectException => {
                virt::LateMapVtl0MemoryPolicy::InjectException
            }
        }
    }
}

/// Configuration for VTL2.
///
/// NOTE: This is distinct from `virt::Vtl2Config` to keep an abstraction
/// between the virt crate and this crate. Users should not be specifying
/// virt crate configuration directly.
#[derive(Debug, Clone, MeshPayload)]
pub struct Vtl2Config {
    /// Enable the VTL0 alias map. This maps VTL0's view of memory in VTL2 at
    /// the highest legal physical address bit.
    pub vtl0_alias_map: bool,
    /// If set, map VTL0 memory late after VTL2 has started. The current
    /// heuristic is to defer mapping VTL0 memory until the first
    /// `HvModifyVtlProtectionMask` hypercall is made.
    pub late_map_vtl0_memory: Option<LateMapVtl0MemoryPolicy>,
}

// Isolation type for a partition.
#[derive(Eq, PartialEq, Debug, Copy, Clone, MeshPayload)]
pub enum IsolationType {
    Vbs,
}

impl From<IsolationType> for virt::IsolationType {
    fn from(value: IsolationType) -> Self {
        match value {
            IsolationType::Vbs => Self::Vbs,
        }
    }
}

/// Which VTL to assign a particular device to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, MeshPayload)]
pub enum DeviceVtl {
    Vtl0,
    Vtl1,
    Vtl2,
}

#[derive(Copy, Clone, Debug, MeshPayload)]
pub enum UefiConsoleMode {
    Default,
    Com1,
    Com2,
    None,
}

#[derive(Copy, Clone, Debug, MeshPayload, Default)]
pub enum EfiDiagnosticsLogLevelType {
    /// Default log level
    #[default]
    Default,
    /// Include INFO logs
    Info,
    /// All logs
    Full,
}
