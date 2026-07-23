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
use std::fs::File;
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;
use vmotherboard::LegacyPciChipsetDeviceHandle;
use vmotherboard::options::BaseChipsetManifest;
use vmotherboard::options::VmChipsetCapabilities;

#[derive(MeshPayload, Debug)]
pub struct Config {
    pub load_mode: LoadMode,
    pub floppy_disks: Vec<floppy_resources::FloppyDiskConfig>,
    pub ide_disks: Vec<ide_resources::IdeDeviceConfig>,
    pub pcie_root_complexes: Vec<PcieRootComplexConfig>,
    pub pcie_devices: Vec<PcieDeviceConfig>,
    pub pcie_switches: Vec<PcieSwitchConfig>,
    pub pcie_generic_initiators: Vec<PcieGenericInitiatorConfig>,
    pub vpci_devices: Vec<VpciDeviceConfig>,
    pub numa: NumaTopology,
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
    // TODO: move FirmwareEvent somewhere not GED-specific.
    pub firmware_event_send: Option<mesh::Sender<get_resources::ged::FirmwareEvent>>,
    pub debugger_rpc: Option<mesh::Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
    pub vmbus_devices: Vec<(DeviceVtl, Resource<VmbusDeviceHandleKind>)>,
    pub chipset_devices: Vec<ChipsetDeviceHandle>,
    pub pci_chipset_devices: Vec<LegacyPciChipsetDeviceHandle>,
    pub isa_dma_controller: Option<Resource<vm_resource::kind::IsaDmaControllerHandleKind>>,
    pub chipset_capabilities: VmChipsetCapabilities,
    /// Memory layout sizing for the layout engine. Determines chipset MMIO
    /// range sizes; addresses are allocated dynamically by the resolver.
    pub layout: vmm_core_defs::LayoutConfig,
    // This is used for testing. TODO: resourcify, and also store this in VMGS.
    pub rtc_delta_milliseconds: i64,
    /// allow the guest to reset without notifying the client
    pub automatic_guest_reset: bool,
}

pub const DEFAULT_GIC_DISTRIBUTOR_BASE: u64 = 0xFFFF_0000;
// The KVM in-kernel vGICv3 requires the distributor and redistributor bases be 64KiB aligned.
pub const DEFAULT_GIC_REDISTRIBUTORS_BASE: u64 = if cfg!(target_os = "linux") {
    0xEFFF_0000
} else {
    0xEFFE_E000
};

/// Base address of the guest-visible GIC v2m MSI frame (exposed via the MADT
/// and used by the software v2m SETSPI decoder for emulated devices). This is
/// OpenVMM-emulated MMIO (one 4 KiB page), not shadowed by the hypervisor, so
/// it stays at the conventional address.
pub const DEFAULT_GIC_V2M_MSI_FRAME_BASE: u64 = 0xEFFE_8000;
/// Size of the v2m MSI frame (one 4KB page is the architectural minimum).
pub const GIC_V2M_MSI_FRAME_SIZE: u64 = 0x1000;

/// Base address of the GIC v2m MSI doorbell used for passthrough on the
/// MSHV root/arm64 backend. Registered with the hypervisor as
/// GITS_TRANSLATER_BASE_ADDRESS.
/// The hypervisor shadows a ~64 KiB region at this base,
/// so it uses the Hyper-V convention address 0xEFF6_8000.
pub const DEFAULT_GIC_V2M_DOORBELL_BASE: u64 = 0xEFF6_8000;

/// Base address of the GICv3 ITS MMIO region. Must be 64 KiB aligned,
/// below the v2m frame address, and not overlap other devices.
/// The region extends from this base to base + GIC_ITS_SIZE (128 KiB).
pub const DEFAULT_GIC_ITS_BASE: u64 = 0xEFFC_0000;
/// Size of the ITS MMIO region (control frame + translation frame, 2×64 KiB).
pub const GIC_ITS_SIZE: u64 = 0x2_0000;

/// Default virtual timer PPI (GIC INTID). PPI 4 = INTID 16 + 4 = 20.
/// This is the EL1 virtual timer interrupt used across Hyper-V, KVM, and HVF.
pub const DEFAULT_VIRT_TIMER_PPI: u32 = 20;

/// Default total number of GIC interrupts (SGIs + PPIs + SPIs).
/// Must satisfy KVM constraints: 64 <= n <= 1023, multiple of 32.
/// 992 = 31 × 32 is the largest valid value.
pub const DEFAULT_GIC_NR_IRQS: u32 = 992;

/// Default VMBus PPI (GIC INTID). PPI 2 = INTID 16 + 2 = 18.
pub const DEFAULT_VMBUS_PPI: u32 = 18;

/// How firmware tables are presented to the guest in Linux direct boot.
///
/// On x86, `DeviceTree` is not supported and will be rejected. On aarch64,
/// this selects between a full device tree or an ACPI boot path.
#[derive(MeshPayload, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDirectBootMode {
    /// Full device tree with all devices described in DT nodes (aarch64 only).
    DeviceTree,
    /// ACPI tables for device discovery. On aarch64, this also synthesizes
    /// an EFI system table so the kernel enters its ACPI code path. On x86,
    /// ACPI tables are always provided via the zero page.
    Acpi,
}

#[derive(MeshPayload, Debug)]
pub enum LoadMode {
    Linux {
        kernel: File,
        initrd: Option<File>,
        cmdline: String,
        enable_serial: bool,
        boot_mode: LinuxDirectBootMode,
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
        enable_vmbus: bool,
        force_dma_bounce: bool,
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

/// Specifies a PCIe MMIO BAR window, either by size (the resolver allocates) or
/// by a fixed location. Fixed locations exist for assigned-device, IOMMU, and
/// physical-topology compatibility.
#[derive(Debug, MeshPayload)]
pub enum PcieMmioRangeConfig {
    /// Dynamically allocate a range of the given size.
    Dynamic {
        /// Size of the range in bytes.
        size: u64,
    },
    /// Use the specified fixed memory range.
    Fixed(MemoryRange),
}

#[derive(Debug, MeshPayload)]
pub struct RootComplexCxlConfig {
    /// HDM window size in bytes for this CXL root complex.
    pub hdm_size: u64,
    /// CFMWS HDM window restrictions bitmask.
    pub hdm_window_restrictions: u16,
}

#[derive(Debug, MeshPayload)]
pub struct PcieRootComplexConfig {
    pub index: u32,
    pub name: String,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
    pub low_mmio: PcieMmioRangeConfig,
    pub high_mmio: PcieMmioRangeConfig,
    pub ports: Vec<PciePortConfig>,
    /// Optional CXL configuration for root-complex CXL mode.
    pub cxl: Option<RootComplexCxlConfig>,
    /// Optional IOMMU for this root complex.
    pub iommu: Option<PcieIommuConfig>,
    /// NUMA node affinity for this root complex. Used to generate `_PXM` in
    /// the ACPI SSDT so the guest OS sees correct NUMA locality for devices
    /// under this root complex.
    pub vnode: Option<u32>,
    /// When true, treat non-zero BAR values found during probing as pinned
    /// addresses. Used for P2P DMA with GPA = HPA.
    pub preserve_bars: bool,
}

/// Configuration for a single PCIe port — either a root-complex root port or a
/// switch downstream port.
#[derive(Debug, MeshPayload)]
pub struct PciePortConfig {
    /// Port name used for topology wiring and lookup.
    pub name: String,
    /// The device/function (`device << 3 | function`) to place this port at on
    /// its bus.
    ///
    /// When `None`, the port is assigned the lowest available devfn. Ports are
    /// assigned in order, so an explicit devfn that collides with a
    /// previously-assigned port (including one assigned automatically) is an
    /// error. Honored for both root-complex root ports and switch downstream
    /// ports.
    pub devfn: Option<u8>,
    /// Enables PCIe hotplug capabilities for this port.
    pub hotplug: bool,
    /// Optional ACS capability bitmask to expose on this port.
    pub acs_capabilities_supported: Option<u16>,
    /// Marks this port as CXL-capable.
    ///
    /// Runtime port construction derives required BAR/subregion layout from
    /// this flag (currently CXL component registers for BAR0).
    pub cxl: bool,
    /// Enables PASID support for functions downstream of this port.
    pub pasid: bool,
}

#[derive(Debug, MeshPayload)]
pub struct PcieSwitchConfig {
    pub name: String,
    pub parent_port: String,
    /// The downstream ports of this switch.
    pub ports: Vec<PciePortConfig>,
}

/// Declares that the device directly behind a named PCIe port (a root port or
/// a switch downstream port) is a generic initiator (GI) for the given NUMA
/// node. Used to generate an SRAT Generic Initiator Affinity structure so the
/// guest attaches the device's memory to that (typically CPU-less) proximity
/// domain.
///
/// The port is resolved against the live topology by port name after switch
/// downstream ports have been enumerated, so it can target devices that sit
/// behind a switch.
#[derive(Debug, MeshPayload)]
pub struct PcieGenericInitiatorConfig {
    /// Name of the PCIe port (root port or switch downstream port) behind
    /// which the generic-initiator device resides.
    pub port_name: String,
    /// NUMA node the device is a generic initiator for.
    pub node: u32,
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
    /// NUMA node affinity for this VPCI device.
    pub vnode: Option<u32>,
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
    /// Disable the PMU.
    Disabled,
}

/// MSI controller selection for aarch64 PCIe interrupt delivery.
#[derive(Debug, Protobuf, Default, Clone)]
pub enum GicMsiConfig {
    /// Automatically select the best available MSI controller:
    /// ITS when the hypervisor supports it, otherwise GICv2m.
    #[default]
    Auto,
    /// Force GICv3 ITS for MSI delivery via LPIs.
    Its,
    /// Force GICv2m for MSI delivery via SPIs.
    V2m {
        /// Number of SPIs to reserve for PCIe MSIs. Defaults to a
        /// platform-specific value when `None`.
        spi_count: Option<u32>,
    },
}

/// IOMMU configuration for a single PCIe root complex.
#[derive(Debug, MeshPayload, Clone)]
pub enum PcieIommuConfig {
    /// AMD IOMMU (AMD-Vi) for x86_64 guests.
    AmdVi,
    /// Arm SMMUv3 for aarch64 guests.
    Smmu {
        /// Enable HW-accelerated nested translation (iommufd). Requires VFIO
        /// devices with `iommu=` behind this SMMU.
        accel: bool,
        /// Output address size (OAS) resolution policy.
        oas: SmmuOas,
    },
    /// Intel VT-d for x86_64 guests.
    IntelVtd,
}

/// Output address size (OAS) policy for an emulated SMMUv3.
#[derive(Debug, MeshPayload, Clone, Copy)]
pub enum SmmuOas {
    /// Advertise a fixed default OAS. See `DEFAULT_AUTO_OAS_BITS` for the
    /// sizing policy and its limits.
    Auto,
    /// Use a fixed OAS in bits (one of 32, 36, 40, 42, 44, 48, 52).
    Fixed(u8),
}

#[derive(Debug, Protobuf, Default, Clone)]
pub struct Aarch64TopologyConfig {
    pub gic_config: Option<GicConfig>,
    pub pmu_gsiv: PmuGsivConfig,
    pub gic_msi: GicMsiConfig,
}

/// GIC configuration for the virtual machine.
///
/// The variant selects the GIC version. `None` inner config means use
/// defaults for that version's addresses.
#[derive(Debug, Protobuf, Clone)]
pub enum GicConfig {
    /// GICv2 with optional address overrides.
    V2(Option<GicV2Config>),
    /// GICv3 with optional address overrides.
    V3(Option<GicV3Config>),
}

/// GICv2-specific address configuration.
#[derive(Debug, Protobuf, Clone)]
pub struct GicV2Config {
    pub gic_distributor_base: u64,
    pub cpu_interface_base: u64,
}

/// GICv3-specific address configuration.
#[derive(Debug, Protobuf, Clone)]
pub struct GicV3Config {
    pub gic_distributor_base: u64,
    pub gic_redistributors_base: u64,
}

#[derive(Debug, Protobuf, Clone)]
pub enum ArchTopologyConfig {
    X86(X86TopologyConfig),
    Aarch64(Aarch64TopologyConfig),
}

/// Per-node memory allocation configuration.
#[derive(Debug, Clone, Copy, MeshPayload)]
pub struct MemoryConfig {
    pub mem_size: u64,
    pub prefetch_memory: bool,
    pub private_memory: bool,
    pub transparent_hugepages: bool,
    pub hugepages: bool,
    pub hugepage_size: Option<u64>,
    /// Host physical NUMA node to bind this allocation to (Linux:
    /// `mbind(MPOL_BIND)`). `None` means OS default placement.
    pub host_numa_node: Option<u32>,
}

/// Virtual NUMA topology for the VM.
#[derive(Debug, MeshPayload)]
pub struct NumaTopology {
    /// NUMA nodes. The vnode ID is the index into this vector.
    pub nodes: Vec<NumaNode>,
    /// Inter-node distances for the SLIT. If empty, defaults are used
    /// (10 for self, 20 for cross-node).
    pub distances: Vec<NumaDistance>,
}

/// A single virtual NUMA node.
#[derive(Debug, MeshPayload)]
pub struct NumaNode {
    /// Memory allocation for this node. `None` means a CPU-only or
    /// device-only node.
    pub mem: Option<MemoryConfig>,
    /// VP assignment for this node.
    pub vps: VpAssignment,
}

/// How VPs are assigned to a NUMA node.
#[derive(Debug, MeshPayload)]
pub enum VpAssignment {
    /// Assign VPs to nodes by round-robining sockets over the CPU-bearing
    /// nodes only: a VP with socket ID `vp_index / vps_per_socket` belongs to
    /// the `(vp_index / vps_per_socket) % num_cpu_nodes`-th `FromTopology`
    /// node. `vps_per_socket` comes from `ProcessorTopologyConfig`;
    /// `num_cpu_nodes` is the number of `FromTopology` nodes, so `Empty`
    /// (CPU-less) nodes are skipped and do not affect the distribution.
    FromTopology,
    /// Explicit VP indices assigned to this node.
    Explicit(Vec<u32>),
    /// A CPU-less node: no VPs are assigned to it. Unlike `Explicit`, this
    /// may be combined with `FromTopology` nodes, so a memory- or
    /// device-only node can be declared without forcing every other node to
    /// spell out its VP set.
    Empty,
}

/// An inter-node distance entry for the ACPI SLIT.
#[derive(Debug, MeshPayload)]
pub struct NumaDistance {
    /// Source node index.
    pub src: u32,
    /// Destination node index.
    pub dst: u32,
    /// Distance value (10 = local, 20 = default cross-node, 255 = unreachable).
    pub distance: u8,
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
    pub with_vtl2: Option<Vtl2Config>,
    pub with_isolation: Option<IsolationType>,
    /// Expose hardware virtualization (VMX/SVM) to the guest so that it can run
    /// its own hypervisor. A backend that does not recognize this request
    /// rejects it rather than silently ignoring it (see
    /// `virt::Hypervisor::recognizes_nested_virt`).
    pub nested_virt: bool,
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
    Snp,
    Cca,
}

impl From<IsolationType> for virt::IsolationType {
    fn from(value: IsolationType) -> Self {
        match value {
            IsolationType::Vbs => Self::Vbs,
            IsolationType::Snp => Self::Snp,
            IsolationType::Cca => Self::Cca,
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
