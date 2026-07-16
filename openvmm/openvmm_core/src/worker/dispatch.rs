// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod amd_iommu_wiring;
mod dump;
mod ecam_config_access;
mod intel_vtd_wiring;
mod ioapic_iommu_wiring;
mod pcie_topology;
mod pcie_wiring;
mod smmu_wiring;

use crate::emuplat;
use crate::partition::BindHvliteVp;
use crate::partition::HvlitePartition;
use crate::vmgs_non_volatile_store::HvLiteVmgsNonVolatileStore;
use crate::worker::memory_layout::ChipsetMmioRanges;
use crate::worker::memory_layout::MemoryLayoutInput;
use crate::worker::memory_layout::ResolvedIommuRanges;
use crate::worker::memory_layout::ResolvedPcieRootComplexRanges;
use crate::worker::memory_layout::resolve_memory_layout;
use crate::worker::rom::RomBuilder;
use acpi::dsdt;
use anyhow::Context;
use cfg_if::cfg_if;
use chipset_device::io::IoResult;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAccessType;
use chipset_device::pci::PciConfigAddress;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_resources::LEGACY_CHIPSET_PCI_BUS_NAME;
use chipset_resources::cmos_rtc_time_source::SystemTimeClockHandle;
use cxl_spec::spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES;
use debug_ptr::DebugPtr;
use disk_backend::Disk;
use disk_backend::resolve::ResolveDiskParameters;
use firmware_uefi_resources::LogLevel;
use floppy_resources::FloppyDiskConfig;
use futures::FutureExt;
use futures::StreamExt;
use futures::executor::block_on;
use futures::future::try_join_all;
use futures_concurrency::prelude::*;
use guestmem::GuestMemory;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use hypervisor_resources::HypervisorKind;
use ide_resources::GuestMedia;
use ide_resources::IdeDeviceConfig;
use igvm::IgvmFile;
use input_core::InputData;
use input_core::MultiplexedInputHandle;
use membacking::GuestMemoryBuilder;
use membacking::GuestMemoryManager;
use membacking::SharedMemoryBacking;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::payload::Protobuf;
use mesh::payload::message::ProtobufMessage;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use missing_dev::MissingDevManifest;
use openvmm_defs::config::Aarch64TopologyConfig;
use openvmm_defs::config::ArchTopologyConfig;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::EfiDiagnosticsLogLevelType;
use openvmm_defs::config::GicConfig;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieIommuConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::PmuGsivConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::config::Vtl2BaseAddressType;
use openvmm_defs::config::Vtl2Config;
use openvmm_defs::config::X2ApicConfig;
use openvmm_defs::config::X86TopologyConfig;
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_pcat_locator::RomFileLocation;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::local::block_with_io;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pci_core::PciInterruptPin;
use pcie::root::GenericPcieRootComplex;
use pcie::switch::GenericPcieSwitch;
use scsi_core::ResolveScsiDeviceHandleParams;
use scsidisk::atapi_scsi::AtapiScsiDisk;
use serial_16550_resources::ComPort;
use state_unit::SavedStateUnit;
use state_unit::SpawnedUnit;
use state_unit::StateUnits;
use std::fs::File;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use virt::ProtoPartition;
use virt::VpIndex;
use virtio::PciInterruptModel;
use virtio::VirtioMmioDevice;
use virtio::VirtioPciDevice;
use virtio::resolve::VirtioResolveInput;
use vm_loader::InitialLoad;
use vm_loader::initial_regs::initial_regs;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::KeyboardInputHandleKind;
use vm_resource::kind::MouseInputHandleKind;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_topology::cxl::CfmwsWindowRestrictions;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::pcie::PcieHostBridgeCxlInfo;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::aarch64::GicVersion;
use vm_topology::processor::x86::X86Topology;
use vmbus_channel::channel::VmbusDevice;
use vmbus_server::HvsockRelayChannel;
use vmbus_server::VmbusServer;
use vmbus_server::hvsock::HvsockRelay;
use vmcore::save_restore::SavedStateRoot;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vm_task::thread::ThreadDriverBackend;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use vmcore::vmtime::VmTimeSource;
use vmgs_resources::GuestStateEncryptionPolicy;
use vmgs_resources::VmgsResource;
use vmm_core::acpi_builder::AcpiTablesBuilder;
use vmm_core::acpi_builder::GenericInitiator;
use vmm_core::acpi_builder::SlitInfo;
use vmm_core::device_builder::VpciBusConfig;
use vmm_core::input_distributor::InputDistributor;
use vmm_core::partition_unit::Halt;
use vmm_core::partition_unit::PartitionUnit;
use vmm_core::partition_unit::PartitionUnitParams;
use vmm_core::partition_unit::block_on_vp;
use vmm_core::vmbus_unit::ChannelUnit;
use vmm_core::vmbus_unit::VmbusServerHandle;
use vmm_core::vmbus_unit::offer_vmbus_device_handle_unit;
use vmm_core_defs::HaltReason;
use vmotherboard::BaseChipsetBuilder;
use vmotherboard::BaseChipsetBuilderOutput;
use vmotherboard::ChipsetDeviceHandle;
use vmotherboard::ChipsetDevices;
use vmotherboard::LegacyPciChipsetDeviceHandle;
use vmotherboard::options::BaseChipsetDevices;
use vmotherboard::options::BaseChipsetFoundation;
use vmotherboard::options::BaseChipsetManifest;
use vmotherboard::options::VmChipsetCapabilities;
#[cfg(all(windows, feature = "virt_whp"))]
use vpci::bus::VpciBus;
use watchdog_core::platform::BaseWatchdogPlatform;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_core::resources::StaticWatchdogPlatformResolver;

#[cfg(guest_arch = "x86_64")]
const PM_BASE: u16 = 0x400;
#[cfg(guest_arch = "x86_64")]
const SYSTEM_IRQ_ACPI: u32 = 9;

/// Creates a thread to run low-performance devices on.
pub fn new_device_thread() -> (JoinHandle<()>, DefaultDriver) {
    DefaultPool::spawn_on_thread("basic_device_thread")
}

impl Manifest {
    fn from_config(config: Config) -> Self {
        Self {
            load_mode: config.load_mode,
            floppy_disks: config.floppy_disks,
            ide_disks: config.ide_disks,
            pcie_root_complexes: config.pcie_root_complexes,
            pcie_devices: config.pcie_devices,
            pcie_switches: config.pcie_switches,
            pcie_generic_initiators: config.pcie_generic_initiators,
            vpci_devices: config.vpci_devices,
            hypervisor: config.hypervisor,
            numa: config.numa,
            processor_topology: config.processor_topology,
            chipset: config.chipset,
            #[cfg(windows)]
            kernel_vmnics: config.kernel_vmnics,
            input: config.input,
            framebuffer: config.framebuffer,
            vga_firmware: config.vga_firmware,
            vtl2_gfx: config.vtl2_gfx,
            virtio_devices: config.virtio_devices,
            vmbus: config.vmbus,
            vtl2_vmbus: config.vtl2_vmbus,
            #[cfg(all(windows, feature = "virt_whp"))]
            vpci_resources: config.vpci_resources,
            vmgs: config.vmgs,
            secure_boot_enabled: config.secure_boot_enabled,
            custom_uefi_vars: config.custom_uefi_vars,
            firmware_event_send: config.firmware_event_send,
            debugger_rpc: config.debugger_rpc,
            vmbus_devices: config.vmbus_devices,
            chipset_devices: config.chipset_devices,
            pci_chipset_devices: config.pci_chipset_devices,
            isa_dma_controller: config.isa_dma_controller,
            chipset_capabilities: config.chipset_capabilities,
            layout: config.layout,
            rtc_delta_milliseconds: config.rtc_delta_milliseconds,
            automatic_guest_reset: config.automatic_guest_reset,
            efi_diagnostics_log_level: match config.efi_diagnostics_log_level {
                EfiDiagnosticsLogLevelType::Default => LogLevel::make_default(),
                EfiDiagnosticsLogLevelType::Info => LogLevel::make_info(),
                EfiDiagnosticsLogLevelType::Full => LogLevel::make_full(),
            },
        }
    }
}

/// This is the manifest of devices with resolved resources (handles, channels).
///
/// Currently this is identical to `Config`, but that will change in future
/// updates.
#[derive(MeshPayload)]
pub struct Manifest {
    load_mode: LoadMode,
    floppy_disks: Vec<FloppyDiskConfig>,
    ide_disks: Vec<IdeDeviceConfig>,
    pcie_root_complexes: Vec<PcieRootComplexConfig>,
    pcie_devices: Vec<PcieDeviceConfig>,
    pcie_switches: Vec<PcieSwitchConfig>,
    pcie_generic_initiators: Vec<openvmm_defs::config::PcieGenericInitiatorConfig>,
    vpci_devices: Vec<VpciDeviceConfig>,
    numa: NumaTopology,
    processor_topology: ProcessorTopologyConfig,
    hypervisor: HypervisorConfig,
    chipset: BaseChipsetManifest,
    #[cfg(windows)]
    kernel_vmnics: Vec<openvmm_defs::config::KernelVmNicConfig>,
    input: mesh::Receiver<InputData>,
    framebuffer: Option<framebuffer::Framebuffer>,
    vga_firmware: Option<RomFileLocation>,
    vtl2_gfx: bool,
    virtio_devices: Vec<(VirtioBus, Resource<VirtioDeviceHandle>)>,
    vmbus: Option<VmbusConfig>,
    vtl2_vmbus: Option<VmbusConfig>,
    #[cfg(all(windows, feature = "virt_whp"))]
    vpci_resources: Vec<virt_whp::device::DeviceHandle>,
    vmgs: Option<VmgsResource>,
    secure_boot_enabled: bool,
    custom_uefi_vars: firmware_uefi_custom_vars::CustomVars,
    firmware_event_send: Option<mesh::Sender<get_resources::ged::FirmwareEvent>>,
    debugger_rpc: Option<mesh::Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
    vmbus_devices: Vec<(DeviceVtl, Resource<VmbusDeviceHandleKind>)>,
    chipset_devices: Vec<ChipsetDeviceHandle>,
    pci_chipset_devices: Vec<LegacyPciChipsetDeviceHandle>,
    isa_dma_controller: Option<Resource<vm_resource::kind::IsaDmaControllerHandleKind>>,
    chipset_capabilities: VmChipsetCapabilities,
    layout: vmm_core_defs::LayoutConfig,
    rtc_delta_milliseconds: i64,
    automatic_guest_reset: bool,
    efi_diagnostics_log_level: LogLevel,
}

#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "openvmm")]
pub struct SavedState {
    #[mesh(1)]
    pub units: Vec<SavedStateUnit>,
}

async fn open_simple_disk(
    resolver: &ResourceResolver,
    disk_type: Resource<DiskHandleKind>,
    read_only: bool,
    driver_source: &VmTaskDriverSource,
) -> anyhow::Result<Disk> {
    let disk = resolver
        .resolve(
            disk_type,
            ResolveDiskParameters {
                read_only,
                driver_source,
            },
        )
        .await?;
    Ok(disk.0)
}

#[derive(MeshPayload)]
pub struct RestartState {
    hypervisor: Resource<HypervisorKind>,
    manifest: Manifest,
    running: bool,
    saved_state: SavedState,
    shared_memory: Option<SharedMemoryBacking>,
    rpc: mesh::Receiver<VmRpc>,
    notify: mesh::Sender<HaltReason>,
}

// Used for locating VM information in a debugger
// Do not use during program execution
static LOADED_VM: DebugPtr<LoadedVm> = DebugPtr::new();

/// The VM worker, used to create and run a VM partition.
pub struct VmWorker {
    vm: LoadedVm,
    rpc: mesh::Receiver<VmRpc>,
    device_thread: JoinHandle<()>,
}

impl Worker for VmWorker {
    type Parameters = VmWorkerParameters;
    type State = RestartState;
    const ID: WorkerId<Self::Parameters> = VM_WORKER;

    fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
        let (device_thread, device_driver) = new_device_thread();

        let manifest = Manifest::from_config(parameters.cfg);

        let hypervisor = block_on(ResourceResolver::new().resolve(parameters.hypervisor, ()))
            .context("failed to resolve hypervisor backend")?;

        let shared_memory = parameters
            .shared_memory
            .map(|fd| SharedMemoryBacking::from_mappable(fd.into()));

        let vm = block_on(InitializedVm::new(
            VmTaskDriverSource::new(ThreadDriverBackend::new(device_driver)),
            hypervisor.0,
            manifest,
            shared_memory,
        ))?;
        let saved_state = parameters
            .saved_state
            .map(|m| m.parse())
            .transpose()
            .context("failed to decode saved state")?;

        let vm = block_with_io(|_| vm.load(saved_state, parameters.notify))?;

        LOADED_VM.store(&vm);

        Ok(Self {
            vm,
            rpc: parameters.rpc,
            device_thread,
        })
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        let RestartState {
            hypervisor,
            manifest,
            running,
            saved_state,
            shared_memory,
            rpc,
            notify,
        } = state;
        let (device_thread, device_driver) = new_device_thread();

        let hypervisor = block_on(ResourceResolver::new().resolve(hypervisor, ()))
            .context("failed to resolve hypervisor backend")?;

        let vm = block_on(InitializedVm::new(
            VmTaskDriverSource::new(ThreadDriverBackend::new(device_driver)),
            hypervisor.0,
            manifest,
            shared_memory,
        ))?;
        pal_async::local::block_on(async {
            let mut vm = vm.load(Some(saved_state), notify).await?;

            LOADED_VM.store(&vm);

            if running {
                vm.resume().await;
            }
            Ok(Self {
                vm,
                rpc,
                device_thread,
            })
        })
    }

    fn run(self, worker_rpc: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        DefaultPool::run_with(async |driver| {
            let driver = driver;
            self.vm.run(&driver, self.rpc, worker_rpc).await
        });
        self.device_thread.join().unwrap();
        Ok(())
    }
}

/// Resolved per-instance IOMMU resources for the VM, keyed by IOMMU type.
///
/// A VM has at most one IOMMU type, so the resolved resources are stored as a
/// single enum rather than three mutually-exclusive fields.
enum ResolvedIommu {
    /// No IOMMU is configured.
    None,
    /// Arm SMMUv3 resources, one per instance.
    #[cfg(guest_arch = "aarch64")]
    Smmu(Vec<smmu_wiring::ResolvedSmmuResources>),
    /// AMD IOMMU resources, one per instance.
    #[cfg(guest_arch = "x86_64")]
    AmdVi(Vec<amd_iommu_wiring::ResolvedIommuResources>),
    /// Intel VT-d resources, one per unit.
    #[cfg(guest_arch = "x86_64")]
    IntelVtd(Vec<intel_vtd_wiring::ResolvedVtdResources>),
}

/// Instantiated IOMMU devices for the VM, keyed by IOMMU type.
///
/// A VM has at most one IOMMU type, so the setup results (ACPI configs plus
/// per-RC shared state) are held as a single enum rather than as separate
/// mutually-exclusive fields.
enum IommuDevices {
    /// No IOMMU is configured.
    None,
    /// Arm SMMUv3 devices.
    #[cfg(guest_arch = "aarch64")]
    Smmu(smmu_wiring::SmmuDevicesResult),
    /// AMD IOMMU devices.
    #[cfg(guest_arch = "x86_64")]
    AmdVi(amd_iommu_wiring::IommuDevicesResult),
    /// Intel VT-d devices.
    #[cfg(guest_arch = "x86_64")]
    IntelVtd(intel_vtd_wiring::VtdDevicesResult),
}

/// A VM that has been initialized but not yet loaded (i.e. the saved state is
/// not yet available).
pub(crate) struct InitializedVm {
    partition: Arc<dyn HvlitePartition>,
    vps: Vec<Box<dyn BindHvliteVp>>,
    vmtime_keeper: VmTimeKeeper,
    vmtime_source: VmTimeSource,
    memory_manager: GuestMemoryManager,
    gm: GuestMemory,
    cfg: Manifest,
    mem_layout: MemoryLayout,
    resolved_pcie_root_complex_ranges: Vec<ResolvedPcieRootComplexRanges>,
    virtio_mmio_region: MemoryRange,
    chipset_mmio: ChipsetMmioRanges,
    vtl2_framebuffer_gpa_base: Option<u64>,
    resolved_iommu: ResolvedIommu,
    processor_topology: ProcessorTopology,
    igvm_file: Option<IgvmFile>,
    driver_source: VmTaskDriverSource,
}

trait ExtractTopologyConfig {
    fn to_config(&self) -> ProcessorTopologyConfig;
}

impl ExtractTopologyConfig for ProcessorTopology<X86Topology> {
    fn to_config(&self) -> ProcessorTopologyConfig {
        ProcessorTopologyConfig {
            proc_count: self.vp_count(),
            vps_per_socket: Some(self.reserved_vps_per_socket()),
            enable_smt: Some(self.smt_enabled()),
            arch: Some(ArchTopologyConfig::X86(X86TopologyConfig {
                apic_id_offset: self.vp_arch(VpIndex::BSP).apic_id,
                x2apic: match self.apic_mode() {
                    vm_topology::processor::x86::ApicMode::XApic => X2ApicConfig::Unsupported,
                    vm_topology::processor::x86::ApicMode::X2ApicSupported => {
                        X2ApicConfig::Supported
                    }
                    vm_topology::processor::x86::ApicMode::X2ApicEnabled => X2ApicConfig::Enabled,
                },
            })),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
struct X86TopologyResult {
    processor_topology: ProcessorTopology<X86Topology>,
}

#[cfg(guest_arch = "x86_64")]
fn build_x86_topology(config: &ProcessorTopologyConfig) -> anyhow::Result<X86TopologyResult> {
    use vm_topology::processor::x86::X2ApicState;

    let arch = match &config.arch {
        None => Default::default(),
        Some(ArchTopologyConfig::X86(arch)) => arch.clone(),
        _ => anyhow::bail!("invalid architecture config"),
    };
    let mut builder = TopologyBuilder::from_host_topology()?;
    builder.apic_id_offset(arch.apic_id_offset);
    if let Some(smt) = config.enable_smt {
        builder.smt_enabled(smt);
    }
    if let Some(count) = config.vps_per_socket {
        builder.vps_per_socket(count);
    }
    let x2apic = match arch.x2apic {
        X2ApicConfig::Auto => {
            // FUTURE: query the hypervisor for a recommendation.
            X2ApicState::Supported
        }
        X2ApicConfig::Supported => X2ApicState::Supported,
        X2ApicConfig::Unsupported => X2ApicState::Unsupported,
        X2ApicConfig::Enabled => X2ApicState::Enabled,
    };
    builder.x2apic(x2apic);
    Ok(X86TopologyResult {
        processor_topology: builder.build(config.proc_count)?,
    })
}

impl ExtractTopologyConfig for ProcessorTopology<Aarch64Topology> {
    fn to_config(&self) -> ProcessorTopologyConfig {
        ProcessorTopologyConfig {
            proc_count: self.vp_count(),
            vps_per_socket: Some(self.reserved_vps_per_socket()),
            enable_smt: Some(self.smt_enabled()),
            arch: Some(ArchTopologyConfig::Aarch64(Aarch64TopologyConfig {
                gic_config: Some(match self.gic_version() {
                    GicVersion::V3 {
                        redistributors_base,
                    } => GicConfig::V3(Some(openvmm_defs::config::GicV3Config {
                        gic_distributor_base: self.gic_distributor_base(),
                        gic_redistributors_base: redistributors_base,
                    })),
                    GicVersion::V2 { cpu_interface_base } => {
                        GicConfig::V2(Some(openvmm_defs::config::GicV2Config {
                            gic_distributor_base: self.gic_distributor_base(),
                            cpu_interface_base,
                        }))
                    }
                }),
                pmu_gsiv: match self.pmu_gsiv() {
                    Some(gsiv) => PmuGsivConfig::Gsiv(gsiv),
                    None => PmuGsivConfig::Disabled,
                },
                gic_msi: Default::default(),
            })),
        }
    }
}

#[cfg(guest_arch = "aarch64")]
struct Aarch64TopologyResult {
    processor_topology: ProcessorTopology<Aarch64Topology>,
    spi_layout: super::spi_layout::ResolvedSpiLayout,
}

#[cfg(guest_arch = "aarch64")]
fn build_aarch64_topology(
    config: &ProcessorTopologyConfig,
    platform_info: &virt::PlatformInfo,
    smmu_count: usize,
) -> anyhow::Result<Aarch64TopologyResult> {
    use openvmm_defs::config::GicMsiConfig;
    use vm_topology::processor::aarch64::Aarch64PlatformConfig;
    use vm_topology::processor::aarch64::GicItsInfo;
    use vm_topology::processor::aarch64::GicMsiController;
    use vm_topology::processor::aarch64::GicV2mInfo;

    const DEFAULT_GIC_V2M_SPI_COUNT: u32 = 64;

    let arch = match &config.arch {
        None => Default::default(),
        Some(ArchTopologyConfig::Aarch64(arch)) => arch.clone(),
        _ => anyhow::bail!("invalid architecture config"),
    };

    let pmu_gsiv = match arch.pmu_gsiv {
        PmuGsivConfig::Disabled => None,
        PmuGsivConfig::Gsiv(gsiv) => Some(gsiv),
        PmuGsivConfig::Platform => platform_info.platform_gsiv,
    };

    // TODO: When this value is supported on all platforms, we should change
    // the arch config to not be an option. For now, warn since the ARM VBSA
    // expects this to be available.
    if pmu_gsiv.is_none() {
        tracing::warn!("PMU GSIV is not set");
    }

    let (gic_distributor_base, gic_version) = match &arch.gic_config {
        Some(GicConfig::V3(config)) => {
            let dist = config
                .as_ref()
                .map(|c| c.gic_distributor_base)
                .unwrap_or(openvmm_defs::config::DEFAULT_GIC_DISTRIBUTOR_BASE);
            let redist = config
                .as_ref()
                .map(|c| c.gic_redistributors_base)
                .unwrap_or(openvmm_defs::config::DEFAULT_GIC_REDISTRIBUTORS_BASE);
            (
                dist,
                GicVersion::V3 {
                    redistributors_base: redist,
                },
            )
        }
        Some(GicConfig::V2(config)) => {
            let dist = config
                .as_ref()
                .map(|c| c.gic_distributor_base)
                .unwrap_or(openvmm_defs::config::DEFAULT_GIC_DISTRIBUTOR_BASE);
            let cpu_if = config
                .as_ref()
                .map(|c| c.cpu_interface_base)
                .unwrap_or(openvmm_defs::config::DEFAULT_GIC_REDISTRIBUTORS_BASE);
            (
                dist,
                GicVersion::V2 {
                    cpu_interface_base: cpu_if,
                },
            )
        }
        None => {
            // No explicit GIC config — use the hypervisor's detected version
            // with default addresses.
            let dist = openvmm_defs::config::DEFAULT_GIC_DISTRIBUTOR_BASE;
            let second = openvmm_defs::config::DEFAULT_GIC_REDISTRIBUTORS_BASE;
            if platform_info.supports_gic_v3 {
                (
                    dist,
                    GicVersion::V3 {
                        redistributors_base: second,
                    },
                )
            } else {
                (
                    dist,
                    GicVersion::V2 {
                        cpu_interface_base: second,
                    },
                )
            }
        }
    };

    // Resolve ITS vs v2m and determine v2m SPI count.
    let is_gicv2 = matches!(gic_version, GicVersion::V2 { .. });
    let v2m_spi_count = match &arch.gic_msi {
        GicMsiConfig::Auto if platform_info.supports_its && !is_gicv2 => None,
        GicMsiConfig::Auto => Some(DEFAULT_GIC_V2M_SPI_COUNT),
        GicMsiConfig::Its => {
            if is_gicv2 {
                anyhow::bail!("ITS is incompatible with GICv2");
            }
            if !platform_info.supports_its {
                anyhow::bail!("ITS requested but the hypervisor does not support it");
            }
            None
        }
        GicMsiConfig::V2m { spi_count } => Some(spi_count.unwrap_or(DEFAULT_GIC_V2M_SPI_COUNT)),
    };

    // Resolve SPI layout — all SPI allocations in one deterministic pass.
    let gic_nr_irqs = openvmm_defs::config::DEFAULT_GIC_NR_IRQS;
    let spi_layout = super::spi_layout::resolve_spi_layout(&super::spi_layout::SpiLayoutInput {
        gic_nr_irqs,
        v2m_spi_count,
        smmu_count,
    })?;

    // Build the GIC MSI controller from resolved SPIs.
    let gic_msi = if let Some(count) = v2m_spi_count {
        GicMsiController::V2m(GicV2mInfo {
            frame_base: openvmm_defs::config::DEFAULT_GIC_V2M_MSI_FRAME_BASE,
            doorbell_base: openvmm_defs::config::DEFAULT_GIC_V2M_DOORBELL_BASE,
            spi_base: spi_layout
                .v2m_spi_base
                .expect("v2m base must be allocated when v2m_spi_count is Some"),
            spi_count: count,
        })
    } else {
        GicMsiController::Its(GicItsInfo {
            its_base: openvmm_defs::config::DEFAULT_GIC_ITS_BASE,
        })
    };

    let platform = Aarch64PlatformConfig {
        gic_distributor_base,
        gic_version,
        gic_msi,
        pmu_gsiv,
        virt_timer_ppi: openvmm_defs::config::DEFAULT_VIRT_TIMER_PPI,
        gic_nr_irqs,
    };

    let mut builder = TopologyBuilder::new_aarch64(platform);
    if let Some(smt) = config.enable_smt {
        builder.smt_enabled(smt);
    }
    if let Some(count) = config.vps_per_socket {
        builder.vps_per_socket(count);
    } else {
        builder.vps_per_socket(config.proc_count);
    }
    Ok(Aarch64TopologyResult {
        processor_topology: builder.build(config.proc_count)?,
        spi_layout,
    })
}

/// A VM that has been loaded and can be run.
///
/// Most new state should be added to [`LoadedVmInner`].
pub(crate) struct LoadedVm {
    state_units: StateUnits,
    inner: LoadedVmInner,
    running: bool,
}

/// Most of the VM state for [`LoadedVm`], excluding things that are necessary
/// for state machine transitions.
struct LoadedVmInner {
    driver_source: VmTaskDriverSource,
    resolver: ResourceResolver,
    partition_unit: PartitionUnit,
    partition: Arc<dyn HvlitePartition>,
    chipset_devices: ChipsetDevices,
    _vmtime: SpawnedUnit<VmTimeKeeper>,
    memory_manager: GuestMemoryManager,
    gm: GuestMemory,
    vtl0_hvsock_relay: Option<HvsockRelay>,
    vtl2_hvsock_relay: Option<HvsockRelay>,
    vmbus_server: Option<VmbusServerHandle>,
    vtl2_vmbus_server: Option<VmbusServerHandle>,
    #[cfg(windows)]
    _vmbus_proxy: Option<vmbus_server::ProxyIntegration>,
    #[cfg(windows)]
    _kernel_vmnics: Vec<vmswitch::kernel::KernelVmNic>,
    numa_cfg: NumaTopology,
    mem_layout: MemoryLayout,
    processor_topology: ProcessorTopology,
    hypervisor_cfg: HypervisorConfig,
    vmbus_redirect: bool,
    vmbus_devices: Vec<SpawnedUnit<ChannelUnit<dyn VmbusDevice>>>,

    input_distributor: SpawnedUnit<InputDistributor>,
    vtl2_framebuffer_gpa_base: Option<u64>,

    chipset_cfg: BaseChipsetManifest,
    chipset_capabilities: VmChipsetCapabilities,
    #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
    virtio_mmio_region: MemoryRange,
    #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
    virtio_mmio_irq: u32,
    /// Resolved chipset MMIO ranges.
    chipset_mmio: ChipsetMmioRanges,
    /// ((device, function), interrupt)
    #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
    pci_legacy_interrupts: Vec<((u8, Option<u8>), u32)>,
    firmware_event_send: Option<mesh::Sender<get_resources::ged::FirmwareEvent>>,

    load_mode: LoadMode,
    igvm_file: Option<IgvmFile>,
    next_igvm_file: Option<IgvmFile>,
    _vmgs_task: Option<Task<()>>,
    vmgs_client_inspect_handle: Option<vmgs_broker::VmgsClient>,

    /// VFIO container manager inspect handle (Linux only).
    #[cfg(target_os = "linux")]
    vfio_inspect: Option<vfio_assigned_device::manager::VfioManagerClient>,
    /// VFIO cdev + iommufd manager inspect handle (Linux only).
    #[cfg(target_os = "linux")]
    vfio_cdev_inspect: Option<vfio_assigned_device::manager::VfioCdevManagerClient>,

    // relay halt messages, intercepting reset if configured.
    halt_recv: mesh::Receiver<HaltReason>,
    client_notify_send: mesh::Sender<HaltReason>,
    /// allow the guest to reset without notifying the client
    automatic_guest_reset: bool,
    chipset: Arc<vmotherboard::Chipset>,
    /// Instantiated IOMMU devices (ACPI configs + per-RC shared state),
    /// keyed by IOMMU type. `IommuDevices::None` when no IOMMU is configured.
    iommu_devices: IommuDevices,
    /// IOAPIC PCIe Requester ID when x86 IOMMU interrupt remapping is active.
    /// For AMD this is threaded into IVRS at firmware-load time; for Intel
    /// the matching DMAR device scope is carried by the per-unit ACPI config.
    #[cfg(guest_arch = "x86_64")]
    ioapic_iommu_rid: Option<u16>,
    pcie_host_bridges: Vec<PcieHostBridge>,
    pcie_root_complexes: Vec<Arc<closeable_mutex::CloseableMutex<GenericPcieRootComplex>>>,
    /// Sources for SRAT generic-initiator entries, one per
    /// [`PcieGenericInitiatorConfig`](openvmm_defs::config::PcieGenericInitiatorConfig).
    /// Each holds the port's live bus-range handle, read at ACPI-build time
    /// (after PCI resource assignment) to derive the device bus.
    generic_initiator_sources: Vec<GenericInitiatorSource>,
    pcie_hotplug_devices: Vec<(
        String,
        vmotherboard::DynamicDeviceUnit,
        Arc<closeable_mutex::CloseableMutex<chipset_device_resources::ErasedChipsetDevice>>,
    )>,
}

/// Helper to determine the x86 IOMMU shared state for a given root complex.
///
/// At most one of AMD IOMMU and Intel VT-d will be active (they are mutually
/// exclusive). Returns `None` when no IOMMU covers `rc_idx`.
#[cfg(guest_arch = "x86_64")]
fn x86_iommu_for_rc(
    iommu_devices: &IommuDevices,
    rc_idx: usize,
) -> Option<pcie_wiring::X86IommuSharedState<'_>> {
    match iommu_devices {
        IommuDevices::AmdVi(devices) => devices
            .shared_states
            .get(rc_idx)
            .and_then(|s| s.as_ref())
            .map(pcie_wiring::X86IommuSharedState::AmdVi),
        IommuDevices::IntelVtd(devices) => devices
            .shared_states
            .get(rc_idx)
            .and_then(|s| s.as_ref())
            .map(pcie_wiring::X86IommuSharedState::IntelVtd),
        IommuDevices::None => None,
    }
}

/// Helper to determine the SMMU shared state for a given root complex.
///
/// Returns `None` when no SMMU covers `rc_idx`.
#[cfg(guest_arch = "aarch64")]
fn smmu_for_rc(iommu_devices: &IommuDevices, rc_idx: usize) -> Option<&Arc<smmu::SmmuSharedState>> {
    match iommu_devices {
        IommuDevices::Smmu(devices) => devices.shared_states.get(rc_idx).and_then(|s| s.as_ref()),
        IommuDevices::None => None,
    }
}

fn convert_vtl2_config(
    vtl2_cfg: Option<&Vtl2Config>,
    load_mode: &LoadMode,
    igvm_file: Option<&IgvmFile>,
) -> anyhow::Result<Option<virt::Vtl2Config>> {
    let vtl2_cfg = match vtl2_cfg {
        Some(cfg) => cfg,
        None => return Ok(None),
    };

    let late_map_vtl0_memory = match vtl2_cfg.late_map_vtl0_memory {
        Some(policy) => {
            use super::vm_loaders::igvm::vtl2_memory_info;
            use virt::LateMapVtl0AllowedRanges;
            let igvm_file = igvm_file.context("vtl2 configured but not loading from igvm")?;

            let allowed_ranges = if let LoadMode::Igvm {
                vtl2_base_address, ..
            } = load_mode
            {
                let range = vtl2_memory_info(igvm_file).context("invalid igvm file")?;
                match vtl2_base_address {
                    Vtl2BaseAddressType::File => {
                        // Allowed range is the file range as-is.
                        LateMapVtl0AllowedRanges::Ranges(vec![range])
                    }
                    Vtl2BaseAddressType::Absolute(base) => {
                        // This file must support relocations.
                        if !crate::worker::vm_loaders::igvm::supports_relocations(igvm_file) {
                            anyhow::bail!(
                                "vtl2 base address is absolute but igvm file does not support relocations"
                            );
                        }

                        // Use the size, but the base is the requested load
                        // base.
                        let allowed =
                            MemoryRange::try_new(*base..base.wrapping_add(range.len()))
                                .with_context(|| format!("invalid vtl2 absolute base {base:#x}"))?;
                        LateMapVtl0AllowedRanges::Ranges(vec![allowed])
                    }
                    Vtl2BaseAddressType::MemoryLayout { .. } => {
                        LateMapVtl0AllowedRanges::MemoryLayout
                    }
                    Vtl2BaseAddressType::Vtl2Allocate { .. } => {
                        // When VTL2 is doing allocation, we do not know which
                        // ranges we should disallow late map access of.
                        anyhow::bail!(
                            "late map vtl0 memory is not supported when VTL2 is doing self allocation of ram"
                        );
                    }
                }
            } else {
                anyhow::bail!("vtl2 configured but not loading from igvm");
            };

            Some(virt::LateMapVtl0MemoryConfig {
                allowed_ranges,
                policy: policy.into(),
            })
        }
        None => None,
    };

    let config = virt::Vtl2Config {
        late_map_vtl0_memory,
    };

    Ok(Some(config))
}

/// A source for an SRAT generic-initiator entry.
///
/// A generic-initiator entry declares that the device directly behind a named
/// port (device 0, function 0 on the port's secondary bus) is a generic
/// initiator for NUMA node `N`. This attaches passthrough device memory (e.g.
/// an NVIDIA Grace GPU's coherent aperture) to a CPU-less NUMA node so the
/// guest driver can online it.
///
/// Rather than predict the device's bus number, this holds the port's live
/// [`AssignedBusRange`](pci_core::bus_range::AssignedBusRange) handle — the
/// same atomic the config-space emulator updates when bridge bus-number
/// registers are written. The secondary bus is read from it at ACPI-build time,
/// after PCI resource assignment has run.
struct GenericInitiatorSource {
    /// Shared bus-range handle of the port the device sits behind.
    bus_range: pci_core::bus_range::AssignedBusRange,
    /// PCI segment of the root complex.
    segment: u16,
    /// Proximity domain (NUMA node) the device is a generic initiator for.
    vnode: u32,
}

impl InitializedVm {
    /// Creates and initializes a VM using the given backend.
    async fn new(
        driver_source: VmTaskDriverSource,
        create_vm: crate::hypervisor_backend::CreateVmFn,
        cfg: Manifest,
        shared_memory: Option<SharedMemoryBacking>,
    ) -> anyhow::Result<Self> {
        create_vm(driver_source, cfg, shared_memory).await
    }

    /// Creates and initializes a VM with the given hypervisor backend.
    ///
    /// This is the main monomorphization point — callers provide a concrete
    /// `virt::Hypervisor` implementation. Called from the blanket impl of
    /// [`HypervisorBackend`](crate::hypervisor_backend::HypervisorBackend).
    pub(crate) async fn new_with_hypervisor<P, H>(
        driver_source: VmTaskDriverSource,
        hypervisor: &mut H,
        #[cfg_attr(not(guest_arch = "aarch64"), expect(unused_variables))]
        platform_info: virt::PlatformInfo,
        cfg: Manifest,
        shared_memory: Option<SharedMemoryBacking>,
    ) -> anyhow::Result<Self>
    where
        H: virt::Hypervisor<Partition = P>,
        P: 'static + HvlitePartition,
    {
        let node_mem_sizes: Vec<u64> = cfg
            .numa
            .nodes
            .iter()
            .map(|n| n.mem.as_ref().map_or(0, |m| m.mem_size))
            .collect();

        let vmtime_keeper = VmTimeKeeper::new(&driver_source.simple(), VmTime::from_100ns(0));
        let vmtime_source = vmtime_keeper
            .builder()
            .build(&driver_source.simple())
            .await
            .unwrap();

        // Pre-parse the igvm file early.
        let igvm_file = if let LoadMode::Igvm { file, .. } = &cfg.load_mode {
            let igvm_file = super::vm_loaders::igvm::read_igvm_file(file)
                .context("reading igvm file failed")?;
            Some(igvm_file)
        } else {
            None
        };

        let hv_config = if cfg.hypervisor.with_hv {
            cfg_if::cfg_if! {
                if #[cfg(all(windows, feature = "virt_whp"))] {
                    let allow_device_assignment = !cfg.vpci_resources.is_empty();
                } else {
                    let allow_device_assignment = false;
                }
            }

            Some(virt::HvConfig {
                allow_device_assignment,
                vtl2: convert_vtl2_config(
                    cfg.hypervisor.with_vtl2.as_ref(),
                    &cfg.load_mode,
                    igvm_file.as_ref(),
                )?,
            })
        } else {
            None
        };

        #[cfg(guest_arch = "aarch64")]
        let (mut processor_topology, spi_layout) = {
            let smmu_count = cfg
                .pcie_root_complexes
                .iter()
                .filter(|rc| matches!(rc.iommu, Some(PcieIommuConfig::Smmu)))
                .count();
            let result =
                build_aarch64_topology(&cfg.processor_topology, &platform_info, smmu_count)?;
            (result.processor_topology, result.spi_layout)
        };
        #[cfg(not(guest_arch = "aarch64"))]
        let mut processor_topology = {
            let result = build_x86_topology(&cfg.processor_topology)?;
            result.processor_topology
        };

        // Validate NUMA topology and resolve VP-to-vnode assignments.
        let vp_to_vnode = super::numa::resolve_numa_vp_assignment(
            &cfg.numa,
            cfg.processor_topology.proc_count,
            processor_topology.vps_per_socket(),
        )
        .context("invalid NUMA topology")?;
        processor_topology.set_vnodes(&vp_to_vnode);

        // Validate that NUMA node references in the PCIe topology point at
        // nodes that actually exist.
        let num_nodes = cfg.numa.nodes.len() as u32;
        for rc in &cfg.pcie_root_complexes {
            if let Some(vnode) = rc.vnode
                && vnode >= num_nodes
            {
                anyhow::bail!(
                    "PCIe root complex '{}' references NUMA node {vnode} which does not exist (num_nodes={num_nodes})",
                    rc.name
                );
            }
        }

        // A backend must explicitly recognize an optional feature; requesting
        // one it does not recognize fails here rather than being silently
        // ignored during partition creation.
        if cfg.hypervisor.nested_virt && !hypervisor.recognizes_nested_virt() {
            anyhow::bail!("the selected hypervisor does not support nested virtualization");
        }

        let proto = hypervisor
            .new_partition(virt::ProtoPartitionConfig {
                processor_topology: &processor_topology,
                hv_config,
                vmtime: &vmtime_source,
                isolation: cfg
                    .hypervisor
                    .with_isolation
                    .map(|typ| typ.into())
                    .unwrap_or(virt::IsolationType::None),
                nested_virt: cfg.hypervisor.nested_virt,
            })
            .context("failed to create the prototype partition")?;

        let physical_address_size = proto.max_physical_address_size();

        // Determine if a special vtl2 memory allocation should be used.
        let vtl2_layout = if let LoadMode::Igvm {
            vtl2_base_address, ..
        } = &cfg.load_mode
        {
            match vtl2_base_address {
                Vtl2BaseAddressType::File
                | Vtl2BaseAddressType::Absolute(_)
                | Vtl2BaseAddressType::Vtl2Allocate { .. } => None,
                Vtl2BaseAddressType::MemoryLayout { size } => {
                    let vtl2_layout = super::vm_loaders::igvm::vtl2_memory_layout_request(
                        igvm_file
                            .as_ref()
                            .expect("igvm file should be already parsed"),
                        *size,
                    )
                    .context("unable to determine vtl2 memory layout request")?;
                    tracing::info!(?vtl2_layout, "vtl2 memory layout request selected");

                    Some(vtl2_layout)
                }
            }
        } else {
            None
        };

        let virtio_mmio_count = cfg
            .virtio_devices
            .iter()
            .filter(|(bus, _)| matches!(bus, VirtioBus::Mmio))
            .count();

        // On aarch64 Linux direct boot, start RAM at 1 GiB to avoid the low GPA
        // region (128 MiB–129 MiB) that iommufd reserves for the host MSI
        // doorbell in IOVA space. Without this gap, iommufd identity-mapped DMA
        // for passthrough devices fails because it cannot allocate IOVAs in
        // that range.
        //
        // FUTURE: this needs to be present for UEFI as well, but UEFI cannot
        // only boot from low memory. Either:
        //  1. Fix Linux to allow configuring the reserved IOVA range.
        //  2. Fix UEFI to allow booting from >0.
        //  3. Install a little bit of low memory, enough for UEFI to get to DXE
        //     (which can run anywhere.)
        let ram_start_address =
            if cfg!(guest_arch = "aarch64") && matches!(cfg.load_mode, LoadMode::Linux { .. }) {
                1024 * 1024 * 1024 // 1 GiB
            } else {
                0
            };

        let vtl2_framebuffer_size = if cfg.vtl2_gfx {
            cfg.framebuffer
                .as_ref()
                .context("no framebuffer configured")?
                .len() as u64
        } else {
            0
        };
        let resolved_layout = resolve_memory_layout(MemoryLayoutInput {
            node_mem_sizes: &node_mem_sizes,
            layout: cfg.layout.clone(),
            pcie_root_complexes: &cfg.pcie_root_complexes,
            virtio_mmio_count,
            vtl2_layout,
            ram_start_address,
            vtl2_framebuffer_size,
            physical_address_size,
        })
        .context("invalid memory configuration")?;
        let mem_layout = resolved_layout.memory_layout;
        let resolved_pcie_root_complex_ranges = resolved_layout.pcie_root_complex_ranges;
        let virtio_mmio_region = resolved_layout.virtio_mmio_region;
        let chipset_mmio = resolved_layout.chipset_mmio;

        // Combine the IOMMU RC configs with the MMIO ranges from the layout
        // engine into the resolved per-instance resources. A VM has at most one
        // IOMMU type, so this produces a single `ResolvedIommu`.
        #[cfg(guest_arch = "x86_64")]
        let resolved_iommu = match &resolved_layout.iommu_ranges {
            ResolvedIommuRanges::AmdVi(ranges) => ResolvedIommu::AmdVi(
                amd_iommu_wiring::resolve_iommu_resources(&cfg.pcie_root_complexes, ranges),
            ),
            ResolvedIommuRanges::IntelVtd(ranges) => ResolvedIommu::IntelVtd(
                intel_vtd_wiring::resolve_vtd_resources(&cfg.pcie_root_complexes, ranges)?,
            ),
            _ => ResolvedIommu::None,
        };
        #[cfg(guest_arch = "aarch64")]
        let resolved_iommu = match &resolved_layout.iommu_ranges {
            ResolvedIommuRanges::Smmu(ranges) => {
                ResolvedIommu::Smmu(smmu_wiring::resolve_smmu_resources(ranges, &spi_layout))
            }
            _ => ResolvedIommu::None,
        };

        // Place the alias map at the end of the address space. Newer versions
        // of OpenHCL support receiving this offset via devicetree (especially
        // important on ARM64 where the physical address width used here is not
        // reported to the guest), but older ones depend on it being hardcoded.
        let vtl0_alias_map = cfg.hypervisor.with_vtl2.as_ref().and_then(|cfg| {
            cfg.vtl0_alias_map
                .then_some(1 << (physical_address_size - 1))
        });

        // Build per-node RAM backing requests. Each NUMA node with memory
        // gets its own backing (memfd), enabling per-node hugepage settings
        // and host NUMA binding.
        let num_nodes = cfg.numa.nodes.len();

        // Group RAM ranges by vnode.
        let mut ranges_by_node: Vec<Vec<MemoryRange>> = vec![Vec::new(); num_nodes];
        for r in mem_layout.ram() {
            ranges_by_node[r.vnode as usize].push(r.range);
        }

        // VTL2 memory goes on the first node with memory.
        if let Some(vtl2_range) = mem_layout.vtl2_range() {
            let first_mem_node = cfg
                .numa
                .nodes
                .iter()
                .position(|n| n.mem.is_some())
                .unwrap_or(0);
            ranges_by_node[first_mem_node].push(vtl2_range);
        }

        // For restore, an existing mappable can only be applied to
        // single-node configurations.
        let nodes_with_ranges = ranges_by_node.iter().filter(|r| !r.is_empty()).count();
        let mut existing_mappable = if let Some(smb) = shared_memory {
            if nodes_with_ranges > 1 {
                anyhow::bail!(
                    "shared memory restore not supported with {nodes_with_ranges} memory nodes"
                );
            }
            Some(smb.into_mappable())
        } else {
            None
        };

        let mut memory_builder = GuestMemoryBuilder::new();
        memory_builder = memory_builder
            .vtl0_alias_map(vtl0_alias_map)
            .x86_legacy_support(
                matches!(cfg.load_mode, LoadMode::Pcat { .. }) || cfg.chipset.with_hyperv_vga,
            );

        for (vnode, ranges) in ranges_by_node.into_iter().enumerate() {
            if ranges.is_empty() {
                continue;
            }

            let mem = cfg.numa.nodes[vnode]
                .mem
                .as_ref()
                .with_context(|| format!("node {vnode} has RAM ranges but no memory config"))?;

            if let Some(size) = mem.hugepage_size
                && !mem.hugepages
            {
                anyhow::bail!("node {vnode}: hugepage_size={size} requires hugepages=on");
            }

            let mut backing = membacking::RamBackingRequest::new(ranges)
                .prefetch(mem.prefetch_memory)
                .private_memory(mem.private_memory)
                .transparent_hugepages(mem.transparent_hugepages)
                .host_numa_node(mem.host_numa_node);
            if mem.hugepages {
                backing = backing.hugepages(mem.hugepage_size);
            }
            if let Some(mappable) = existing_mappable.take() {
                backing = backing.existing_mappable(mappable);
            }

            memory_builder = memory_builder.add_backing(backing);
        }

        #[cfg(all(windows, feature = "virt_whp"))]
        if !cfg.vpci_resources.is_empty() {
            memory_builder = memory_builder.pin_mappings(true);
        }

        cfg_if! {
            if #[cfg(windows)] {
                let vtl2_memory_process = if cfg.hypervisor.with_vtl2.is_some() {
                    // VTL2 needs a separate memory hosting process.
                    let process = pal::windows::process::empty_process()
                        .context("could not launch a memory process for VTL2")?;
                    Some(Box::new(process) as _)
                } else {
                    None
                };
            } else {
                let vtl2_memory_process = None;
            }
        }

        let max_addr = mem_layout
            .end_of_layout()
            .max(mem_layout.vtl2_range().map_or(0, |r| r.end()));

        let mut memory_manager = memory_builder
            .build(max_addr)
            .await
            .context("failed to build guest memory")?;

        let gm = memory_manager
            .client()
            .guest_memory()
            .await
            .context("failed to get guest memory")?;
        let mut cpuid = Vec::new();

        // Add in Hyper-V VMM CPUID leaves.
        if cfg.hypervisor.with_hv {
            let confidential_vmbus = false;
            // Only advertise extended IOAPIC on non-PCAT systems.
            let extended_ioapic_rte = !matches!(cfg.load_mode, LoadMode::Pcat { .. });
            cpuid.extend(vmm_core::cpuid::hyperv_cpuid_leaves(
                extended_ioapic_rte,
                confidential_vmbus,
            ));
        }

        let (partition, vps) = proto
            .build(virt::PartitionConfig {
                mem_layout: &mem_layout,
                guest_memory: &gm,
                cpuid: &cpuid,
                vtl0_alias_map,
            })
            .context("failed to create the partition")?;

        let vps = vps.into_iter().map(|vp| Box::new(vp) as _).collect();

        let partition = Arc::new(partition);

        memory_manager
            .attach_partition(Vtl::Vtl0, &partition.memory_mapper(Vtl::Vtl0), None)
            .await
            .context("failed to attach memory to the partition")?;

        if cfg.hypervisor.with_vtl2.is_some() {
            memory_manager
                .attach_partition(
                    Vtl::Vtl2,
                    &partition.memory_mapper(Vtl::Vtl2),
                    vtl2_memory_process,
                )
                .await
                .context("failed to attach memory to VTL2")?;
        }

        Ok(Self {
            partition,
            vps,
            vmtime_keeper,
            vmtime_source,
            memory_manager,
            gm,
            cfg,
            mem_layout,
            resolved_pcie_root_complex_ranges,
            virtio_mmio_region,
            chipset_mmio,
            vtl2_framebuffer_gpa_base: resolved_layout.vtl2_framebuffer_gpa_base,
            resolved_iommu,
            processor_topology,
            igvm_file,
            driver_source,
        })
    }

    /// Loads the state for an initialized VM.
    ///
    // FUTURE: move more of this logic into new() so that more can be done
    //         outside the VM-PHU/live migration blackout window.
    async fn load(
        self,
        saved_state: Option<SavedState>,
        client_notify_send: mesh::Sender<HaltReason>,
    ) -> Result<LoadedVm, anyhow::Error> {
        use vmotherboard::options::dev;

        let Self {
            partition,
            vps,
            vmtime_keeper,
            vmtime_source,
            memory_manager,
            gm,
            cfg,
            mem_layout,
            resolved_pcie_root_complex_ranges,
            virtio_mmio_region,
            chipset_mmio,
            vtl2_framebuffer_gpa_base,
            resolved_iommu,
            processor_topology,
            igvm_file,
            driver_source,
        } = self;

        let mut resolver = ResourceResolver::new();

        resolver.add_async_resolver(
            chipset_device_worker::resolver::RemoteChipsetDeviceResolver(
                OpenVmmRemoteDynamicResolvers {},
            ),
        );

        // Expose the partition reference time source, if available.
        if cfg.hypervisor.with_hv {
            if let Some(ref_time) = partition.reference_time_source() {
                resolver.add_resolver(ref_time);
            }
        }

        if cfg
            .vmgs
            .as_ref()
            .is_some_and(|x| !matches!(x.encryption_policy(), GuestStateEncryptionPolicy::None(_)))
        {
            unimplemented!("guest state encryption not supported on openvmm");
        }

        let vmgs = match cfg.vmgs {
            Some(VmgsResource::Disk(disk)) => Some(
                vmgs::Vmgs::try_open(
                    open_simple_disk(&resolver, disk.disk, false, &driver_source).await?,
                    None,
                    true,
                    false,
                )
                .await
                .context("failed to open vmgs file")?,
            ),
            Some(VmgsResource::ReprovisionOnFailure(disk)) => Some(
                vmgs::Vmgs::try_open(
                    open_simple_disk(&resolver, disk.disk, false, &driver_source).await?,
                    None,
                    true,
                    true,
                )
                .await
                .context("failed to open vmgs file")?,
            ),
            Some(VmgsResource::Reprovision(disk)) => Some(
                vmgs::Vmgs::request_format(
                    open_simple_disk(&resolver, disk.disk, false, &driver_source).await?,
                    None,
                )
                .await
                .context("failed to format vmgs file")?,
            ),
            Some(VmgsResource::Ephemeral) => None,
            // TODO: make sure we don't need a VMGS
            None => None,
        };

        let (vmgs_client, vmgs_task) = if let Some(vmgs) = vmgs {
            let (vmgs_client, vmgs_task) =
                vmgs_broker::spawn_vmgs_broker(driver_source.builder().build("vmgs_broker"), vmgs);
            resolver.add_resolver(vmgs_client.clone());
            (Some(vmgs_client), Some(vmgs_task))
        } else {
            (None, None)
        };

        // For sanity: we immediately restrict `vmgs_client` to the
        // `HvLiteVmgsNonVolatileStore` API, since we don't want code past this
        // point to interact with VMGS as anything but an opaque
        // `NonVolatileStore`
        //
        // ...but we keep a reference to the original untyped client, since we need
        // to pass it to LoadedVm so that we can `inspect` VMGS at runtime.
        let vmgs_client_inspect_handle = vmgs_client.clone();
        let vmgs_client: Option<&dyn HvLiteVmgsNonVolatileStore> =
            vmgs_client.as_ref().map(|x| x as _);

        let (halt_vps, halt_request_recv) = Halt::new();
        let halt_vps = Arc::new(halt_vps);

        resolver.add_resolver(vmm_core::platform_resolvers::HaltResolver(halt_vps.clone()));
        #[cfg(guest_arch = "x86_64")]
        let ioapic_routing = {
            let conn = ioapic_iommu_wiring::IoApicRoutingConnection::new(
                partition.clone().ioapic_routing(),
            );
            resolver.add_resolver(vmm_core::platform_resolvers::IoApicRoutingResolver(
                conn.target(),
            ));
            conn
        };
        resolver.add_resolver(emuplat::i440bx_host_pci_bridge::AdjustGpaRangeResolver(
            memory_manager.ram_visibility_control(),
        ));

        let mapper = memory_manager.device_memory_mapper();

        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_mut))]
        let mut deps_hyperv_firmware_pcat = None;
        match &cfg.load_mode {
            LoadMode::Uefi { .. } => {
                use emuplat::uefi::*;
                // Register the platform-specific resolvers used by the UEFI
                // device.
                resolver.add_resolver(emuplat::firmware::MeshLoggerResolver::new(
                    cfg.firmware_event_send.clone(),
                ));
                resolver.add_async_resolver(OpenvmmUefiWatchdogPlatformResolver::new(
                    partition.clone(),
                    halt_vps.clone(),
                ));
            }
            #[cfg(guest_arch = "x86_64")]
            LoadMode::Pcat {
                firmware,
                boot_order,
            } => {
                tracing::debug!(?firmware, "Loading BIOS firmware.");
                let rom_builder = RomBuilder::new("bios".into(), Box::new(mapper.clone()));
                let rom = rom_builder.build_from_file_location(firmware)?;
                // TODO: move mtrr replay to a resource.
                let halt_vps = halt_vps.clone();
                deps_hyperv_firmware_pcat = Some(dev::HyperVFirmwarePcat {
                    logger: Box::new(emuplat::firmware::MeshLogger::new(
                        cfg.firmware_event_send.clone(),
                    )),
                    generation_id_recv: mesh::channel().1,
                    rom: Some(Box::new(rom)),
                    replay_mtrrs: Box::new(move || halt_vps.replay_mtrrs()),
                    config: {
                        let pcat_slit_info =
                            if cfg.numa.nodes.len() > 1 || !cfg.numa.distances.is_empty() {
                                Some(SlitInfo {
                                    num_nodes: cfg.numa.nodes.len(),
                                    distances: cfg
                                        .numa
                                        .distances
                                        .iter()
                                        .map(|d| (d.src, d.dst, d.distance))
                                        .collect(),
                                })
                            } else {
                                None
                            };
                        let acpi_tables_builder = AcpiTablesBuilder {
                            processor_topology: &processor_topology,
                            mem_layout: &mem_layout,
                            cache_topology: None,
                            pcie_host_bridges: &Vec::new(),
                            slit_info: pcat_slit_info.as_ref(),
                            // PCAT BIOS is mutually exclusive with PCIe root
                            // ports (the only source of generic initiators).
                            generic_initiators: &[],
                            arch: vmm_core::acpi_builder::AcpiArchConfig::X86 {
                                with_ioapic: cfg.chipset_capabilities.with_ioapic,
                                with_pic: cfg.chipset_capabilities.with_pic,
                                with_pit: cfg.chipset_capabilities.with_pit,
                                with_psp: cfg.chipset.with_generic_psp,
                                pm_base: PM_BASE,
                                acpi_irq: SYSTEM_IRQ_ACPI,
                                iommu: None,
                            },
                        };
                        let srat = acpi_tables_builder.build_srat();
                        firmware_pcat::config::PcatBiosConfig {
                            processor_topology: processor_topology.clone(),
                            mem_layout: mem_layout.clone(),
                            chipset_low_mmio: chipset_mmio.low,
                            chipset_high_mmio: chipset_mmio.high,
                            srat,

                            hibernation_enabled: false,
                            initial_generation_id: {
                                let mut generation_id = [0; 16];
                                getrandom::fill(&mut generation_id).expect("rng failure");
                                generation_id
                            },
                            boot_order: {
                                use firmware_pcat::config::BootDevice;
                                use firmware_pcat::config::BootDeviceStatus;
                                use openvmm_defs::config::PcatBootDevice;
                                boot_order.map(|dev| BootDeviceStatus {
                                    kind: match dev {
                                        PcatBootDevice::Floppy => BootDevice::Floppy,
                                        PcatBootDevice::HardDrive => BootDevice::HardDrive,
                                        PcatBootDevice::Optical => BootDevice::Optical,
                                        PcatBootDevice::Network => BootDevice::Network,
                                    },
                                    // TODO: accurately model this?
                                    attached: true,
                                })
                            },
                            num_lock_enabled: false,
                            // TODO: these are all very bogus values, and need to be swapped out with something better
                            smbios: firmware_pcat::config::SmbiosConstants {
                                bios_guid: guid::Guid {
                                    data1: 0xC4066C45,
                                    data2: 0x503D,
                                    data3: 0x40E8,
                                    data4: [0xB1, 0x5C, 0x31, 0x26, 0x4E, 0x5F, 0xE1, 0xD9],
                                },
                                system_serial_number: "9583-9572-9874-4843-7295-1653-92".into(),
                                base_board_serial_number: "9583-9572-9874-4843-7295-1653-92".into(),
                                chassis_serial_number: "9583-9572-9874-4843-7295-1653-92".into(),
                                chassis_asset_tag: "9583-9572-9874-4843-7295-1653-92".into(),
                                bios_lock_string: "00000000000000000000000000000000".into(),
                                processor_manufacturer: b"\0".to_vec(),
                                processor_version: b"\0".to_vec(),
                                cpu_info_bundle: None,
                            },
                        }
                    },
                })
            }
            _ => {}
        };

        if let Some(gpa) = vtl2_framebuffer_gpa_base {
            tracing::debug!("Vtl2 framebuffer gpa base: {:#x}", gpa);
        }

        let state_units = StateUnits::new();

        let vmtime = state_units
            .add("vmtime")
            .spawn(driver_source.simple(), {
                |recv| {
                    let mut vmtime = vmtime_keeper;
                    async move {
                        vmm_core::vmtime_unit::run_vmtime(&mut vmtime, recv).await;
                        vmtime
                    }
                }
            })
            .unwrap();

        let mut input_distributor = InputDistributor::new(cfg.input);
        resolver.add_async_resolver::<KeyboardInputHandleKind, _, MultiplexedInputHandle, _>(
            input_distributor.client().clone(),
        );
        resolver.add_async_resolver::<MouseInputHandleKind, _, MultiplexedInputHandle, _>(
            input_distributor.client().clone(),
        );

        let input_distributor = state_units
            .add("input")
            .spawn(driver_source.simple(), async |mut recv| {
                input_distributor.run(&mut recv).await;
                input_distributor
            })
            .unwrap();

        let mut pci_legacy_interrupts = Vec::new();

        let mut ide_drives = [[None, None], [None, None]];
        if cfg.chipset.with_hyperv_ide {
            pci_legacy_interrupts.push(((7, None), 14));
            pci_legacy_interrupts.push(((7, None), 15));

            for disk_cfg in cfg.ide_disks {
                let path = disk_cfg.path;
                let media = match disk_cfg.guest_media {
                    GuestMedia::Dvd(disk_type) => {
                        let dvd = resolver
                            .resolve(
                                disk_type,
                                ResolveScsiDeviceHandleParams {
                                    driver_source: &driver_source,
                                },
                            )
                            .await
                            .context("failed to open IDE DVD")?;

                        let scsi_disk = Arc::new(AtapiScsiDisk::new(dvd.0));
                        ide::DriveMedia::optical_disk(scsi_disk.clone())
                    }
                    GuestMedia::Disk {
                        disk_type,
                        read_only,
                    } => {
                        // This builds only the emulated IDE drive. The storvsp IDE accelerator
                        // counterpart (which carries any per-disk SCSI parameters via
                        // SimpleScsiDiskHandle) is offered separately as a VMBus device; the two
                        // paths are not yet unified.
                        let disk =
                            open_simple_disk(&resolver, disk_type, read_only, &driver_source)
                                .await
                                .context("failed to open IDE disk")?;

                        ide::DriveMedia::hard_disk(disk)
                    }
                };

                let old_media = ide_drives
                    .get_mut(path.channel as usize)
                    .context("invalid ide channel")?
                    .get_mut(path.drive as usize)
                    .context("invalid ide device")?
                    .replace(media);

                if old_media.is_some() {
                    anyhow::bail!(
                        "ide drive {}:{} is already in use",
                        path.channel,
                        path.drive
                    );
                }
            }
        }

        if cfg.chipset_capabilities.with_guest_watchdog {
            use vmcore::non_volatile_store::EphemeralNonVolatileStore;

            let store = match vmgs_client {
                Some(vmgs) => vmgs
                    .as_non_volatile_store(vmgs::FileId::GUEST_WATCHDOG, false)
                    .context("failed to instantiate guest watchdog store")?,
                None => EphemeralNonVolatileStore::new_boxed(),
            };

            // Create the base watchdog platform
            let mut base_watchdog_platform = BaseWatchdogPlatform::new(store).await?;

            // Create the callback that raises the watchdog halt reason on timeout
            let watchdog_callback = WatchdogTimeout {
                halt_vps: halt_vps.clone(),
                watchdog_send: None, // This is not the UEFI watchdog, so no need to send
                                     // watchdog notifications
            };

            // Add callbacks
            base_watchdog_platform.add_callback(Box::new(watchdog_callback));

            resolver.add_resolver(StaticWatchdogPlatformResolver::new(Box::new(
                base_watchdog_platform,
            )));
        }

        let initial_rtc_cmos = if matches!(cfg.load_mode, LoadMode::Pcat { .. }) {
            Some(firmware_pcat::default_cmos_values(&mem_layout))
        } else {
            None
        };

        let deps_generic_cmos_rtc = (cfg.chipset.with_generic_cmos_rtc).then(|| {
            dev::GenericCmosRtcDeps {
                irq: 8,
                time_source: SystemTimeClockHandle {
                    delta_milliseconds: cfg.rtc_delta_milliseconds,
                }
                .into_resource(),
                century_reg_idx: 0x32, // TODO: automatically sync with FADT
                initial_cmos: initial_rtc_cmos,
            }
        });

        let mut primary_disk_drive = floppy::DriveRibbon::None;
        let mut secondary_disk_drive = floppy::DriveRibbon::None;
        if cfg.chipset.with_winbond_super_io_and_floppy_full {
            let mut pri_drives = Vec::new();
            let mut sec_drives = Vec::new();
            for (index, disk_cfg) in cfg.floppy_disks.into_iter().enumerate() {
                let FloppyDiskConfig {
                    disk_type,
                    read_only,
                } = disk_cfg;

                let disk = open_simple_disk(&resolver, disk_type, read_only, &driver_source)
                    .await
                    .context("failed to open floppy disk")?;
                tracing::trace!("floppy opened based on config into DriveRibbon");

                if index == 0 {
                    pri_drives.push(disk);
                } else if index == 1 {
                    sec_drives.push(disk)
                } else {
                    tracing::error!("more than 2 floppy controllers are not supported");
                    break;
                }
            }

            primary_disk_drive = floppy::DriveRibbon::from_vec(pri_drives)?;
            secondary_disk_drive = floppy::DriveRibbon::from_vec(sec_drives)?;
        }

        // must enforce exclusivity here due to how the
        // `{primary,secondary}_disk_drive` vars get "claimed" by each device.
        let (deps_generic_isa_floppy, deps_winbond_super_io_and_floppy_full) = match (
            cfg.chipset.with_generic_isa_floppy,
            cfg.chipset.with_winbond_super_io_and_floppy_full,
        ) {
            (true, true) => anyhow::bail!("cannot have both generic and winbond floppy"),
            (true, false) => {
                if !matches!(secondary_disk_drive, floppy::DriveRibbon::None) {
                    anyhow::bail!("more than 1 generic floppy controller is not supported")
                }

                (
                    // Use "standard" ISA constants for IRQ, DMA, and IO Port
                    // assignment
                    Some(dev::GenericIsaFloppyDeps {
                        irq: 6,
                        dma_channel: 2,
                        pio_base: 0x3f0,
                        drives: primary_disk_drive,
                    }),
                    None,
                )
            }
            (false, true) => (
                None,
                Some(dev::WinbondSuperIoAndFloppyFullDeps {
                    primary_disk_drive,
                    secondary_disk_drive,
                }),
            ),
            (false, false) => (None, None),
        };

        let pci_bus_id_generic = vmotherboard::BusId::new("generic");
        let pci_bus_id_piix4 = vmotherboard::BusId::new(LEGACY_CHIPSET_PCI_BUS_NAME);

        let deps_generic_pci_bus =
            (cfg.chipset.with_generic_pci_bus).then_some(dev::GenericPciBusDeps {
                bus_id: pci_bus_id_generic.clone(),
                pio_addr: pci_bus::standard_x86_io_ports::ADDR_START,
                pio_data: pci_bus::standard_x86_io_ports::DATA_START,
            });

        let deps_generic_psp = (cfg.chipset.with_generic_psp).then_some(dev::GenericPspDeps {});

        let deps_hyperv_framebuffer =
            (cfg.chipset.with_hyperv_framebuffer).then(|| dev::HyperVFramebufferDeps {
                fb_mapper: Box::new(mapper.clone()),
                fb: cfg.framebuffer.unwrap(),
                vtl2_framebuffer_gpa_base,
            });

        let deps_hyperv_vga = if cfg.chipset.with_hyperv_vga {
            let vga_firmware = cfg.vga_firmware.as_ref().context("no VGA BIOS file")?;
            let rom_builder = RomBuilder::new("vga".into(), Box::new(mapper.clone()));
            let rom = rom_builder.build_from_file_location(vga_firmware)?;

            Some(dev::HyperVVgaDeps {
                attached_to: pci_bus_id_piix4.clone(),
                rom: Some(Box::new(rom)),
            })
        } else {
            None
        };

        let deps_piix4_pci_bus = (cfg.chipset.with_piix4_pci_bus).then(|| dev::Piix4PciBusDeps {
            bus_id: pci_bus_id_piix4.clone(),
        });

        let deps_piix4_cmos_rtc = (cfg.chipset.with_piix4_cmos_rtc).then(|| {
            dev::Piix4CmosRtcDeps {
                time_source: SystemTimeClockHandle {
                    delta_milliseconds: cfg.rtc_delta_milliseconds,
                }
                .into_resource(),
                initial_cmos: initial_rtc_cmos,
                enlightened_interrupts: true, // As advertised by the PCAT BIOS.
            }
        });

        let [primary_channel_drives, secondary_channel_drives] = ide_drives;
        let deps_hyperv_ide = (cfg.chipset.with_hyperv_ide).then_some(dev::HyperVIdeDeps {
            attached_to: pci_bus_id_piix4.clone(),
            primary_channel_drives,
            secondary_channel_drives,
        });

        let base_chipset_devices = {
            BaseChipsetDevices {
                deps_generic_cmos_rtc,
                deps_generic_isa_floppy,
                deps_generic_pci_bus,
                deps_generic_psp,
                deps_hyperv_firmware_pcat,
                deps_hyperv_framebuffer,
                deps_hyperv_ide,
                deps_hyperv_vga,
                deps_piix4_cmos_rtc,
                deps_piix4_pci_bus,
                deps_underhill_vga_proxy: None,
                deps_winbond_super_io_and_floppy_stub: None,
                deps_winbond_super_io_and_floppy_full,
            }
        };

        let BaseChipsetBuilderOutput {
            chipset_builder,
            device_interfaces: base_chipset_device_interfaces,
        } = BaseChipsetBuilder::new(
            BaseChipsetFoundation {
                is_restoring: false,
                untrusted_dma_memory: gm.clone(),
                // There is no access to encrypted memory on the host, so this
                // may be misleading. Presumably in any confidential VM
                // scenario, devices using this will not be present or will be
                // implemented by a paravisor. But it still must be set for
                // non-confidential scenarios.
                trusted_vtl0_dma_memory: gm.clone(),
                power_event_handler: halt_vps.clone(),
                debug_event_handler: halt_vps.clone(),
                vmtime: &vmtime_source,
                vmtime_unit: vmtime.handle(),
                doorbell_registration: partition.clone().into_doorbell_registration(Vtl::Vtl0),
            },
            base_chipset_devices,
        )
        .with_expected_manifest(cfg.chipset.clone())
        .with_device_handles(cfg.chipset_devices)
        .with_pci_device_handles(cfg.pci_chipset_devices)
        .with_isa_dma_handle(cfg.isa_dma_controller)
        .with_trace_unknown_pio(true) // todo: add CLI param?
        .build(&driver_source, &state_units, &resolver)
        .await?;

        if cfg.chipset.with_generic_pci_bus {
            // HACK: We don't currently have an appropriate generic bus root to
            // put on the PCI bus, so we just fake one.
            //
            // This seems to appease Linux just fine
            chipset_builder
                .arc_mutex_device("fake-bus-root")
                .on_pci_bus(pci_bus_id_generic.clone())
                .add(|services| {
                    missing_dev::MissingDev::from_manifest(
                        MissingDevManifest::new().claim_pci((0, 0, 0), 0x8086, 0x7111),
                        &mut services.register_mmio(),
                        &mut services.register_pio(),
                    )
                })?;
        }

        // Add the GIC.
        #[cfg(guest_arch = "aarch64")]
        chipset_builder.add_external_line_target(
            IRQ_LINE_SET,
            0..=vmm_core::emuplat::gic::SPI_RANGE.end() - vmm_core::emuplat::gic::SPI_RANGE.start(),
            *vmm_core::emuplat::gic::SPI_RANGE.start(),
            "gic",
            Arc::new(vmm_core::emuplat::gic::GicInterruptTarget::new(
                partition.clone().control_gic(Vtl::Vtl0),
            )),
        );

        // Add the x86 BSP's LINTs for the PIC to use.
        #[cfg(guest_arch = "x86_64")]
        chipset_builder.add_external_line_target(
            chipset_device_resources::BSP_LINT_LINE_SET,
            0..=1,
            0,
            "bsp",
            partition.clone().into_lint_target(Vtl::Vtl0),
        );

        if let Some(framebuffer) = base_chipset_device_interfaces.framebuffer_local_control {
            resolver.add_resolver(framebuffer);
        }

        let pci_inta_line = {
            const PCI_LEGACY_INTA_IRQ: u32 = 11;
            const PCI_INTA_IRQ: u32 = 16;
            if cfg.chipset_capabilities.with_i440bx_host_pci_bridge {
                // Hyper-V hard-wires this to 11.
                Some(PCI_LEGACY_INTA_IRQ)
            } else if cfg.chipset.with_generic_pci_bus {
                // Avoid an ISA interrupt to avoid conflicts and to avoid needing to
                // configure the line as level-triggered in the MADT (necessary for
                // Linux when the PIC is missing).
                if cfg.chipset_capabilities.with_pic {
                    Some(PCI_LEGACY_INTA_IRQ)
                } else {
                    Some(PCI_INTA_IRQ)
                }
            } else {
                None
            }
        };

        let mut vtl0_hvsock_relay = None;
        #[cfg(windows)]
        let mut vmbus_proxy = None;
        #[cfg(windows)]
        let mut kernel_vmnics = Vec::new();
        let mut vmbus_server = None;
        let mut vtl2_vmbus_server = None;
        let mut vtl2_hvsock_relay = None;
        let mut vmbus_redirect = false;

        // PCI Express topology

        // Deferred MSI connections for root complexes and switches.
        // These are wired after IOMMU setup so that interrupt remapping
        // can be applied when an AMD IOMMU covers the root complex.
        struct DeferredMsiConn {
            msi_conn: pci_core::msi::MsiConnection,
            segment: u16,
            #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
            rc_idx: usize,
        }
        let mut deferred_msi_conns: Vec<DeferredMsiConn> = Vec::new();

        let (mut pcie_host_bridges, pcie_root_complexes) = {
            pcie_topology::validate_pcie_root_complexes(&cfg.pcie_root_complexes)?;
            let mut pcie_host_bridges = Vec::new();
            let mut pcie_root_complexes = Vec::new();

            for (rc_idx, (rc, ranges)) in cfg
                .pcie_root_complexes
                .iter()
                .zip(resolved_pcie_root_complex_ranges)
                .enumerate()
            {
                let cxl_port_count = rc.ports.iter().filter(|rp_cfg| rp_cfg.cxl).count() as u64;
                let cxl_config = rc.cxl.as_ref();

                // Note that for each CXL enabled root port, they need 64K of MMIO space for the component registers.
                // We need to ensure that the PCI MMIO range reserved is sufficient for that.
                if cxl_port_count != 0 {
                    let required_cxl_component_bar_mmio = cxl_port_count
                        .checked_mul(CXL_COMPONENT_REGISTERS_SIZE_BYTES)
                        .context("cxl component register size overflow")?;
                    if ranges.high_mmio.len() < required_cxl_component_bar_mmio {
                        anyhow::bail!(
                            "invalid CXL root complex '{}': high MMIO range {:#x} is too small for {} CXL root-port BAR apertures (requires {:#x})",
                            rc.name,
                            ranges.high_mmio.len(),
                            cxl_port_count,
                            required_cxl_component_bar_mmio
                        );
                    }
                }

                let hdm_range = (!ranges.hdm_range.is_empty()).then_some(ranges.hdm_range);
                let chbcr_range = (!ranges.chbcr_range.is_empty()).then_some(ranges.chbcr_range);

                if cxl_port_count != 0 {
                    if cxl_config.is_none() {
                        anyhow::bail!(
                            "invalid CXL root complex '{}': CXL-capable root ports require both CHBCR and HDM ranges",
                            rc.name
                        );
                    }
                    if hdm_range.is_none() || chbcr_range.is_none() {
                        anyhow::bail!(
                            "invalid CXL root complex '{}': configured CXL CHBCR/HDM ranges were not resolved",
                            rc.name
                        );
                    }
                }

                let device_name = format!("pcie-root:{}", rc.name);

                // Create a static bus range for the root complex so that
                // root port MSI targets can lazily resolve their BDF as
                // (start_bus << 8) | devfn.
                let rc_bus_range = pci_core::bus_range::AssignedBusRange::new();
                rc_bus_range.set_bus_range(rc.start_bus, rc.end_bus);
                let msi_conn = pci_core::msi::MsiConnection::new();

                // When the AMD IOMMU is enabled for this root complex,
                // reserve device 0 for the IOMMU RCiEP and start root
                // ports at device 1.
                #[cfg(guest_arch = "x86_64")]
                let rc_iommu = rc.iommu.as_ref();
                #[cfg(guest_arch = "x86_64")]
                let root_port_start_device: u8 = if matches!(rc_iommu, Some(PcieIommuConfig::AmdVi))
                {
                    1
                } else {
                    0
                };
                #[cfg(not(guest_arch = "x86_64"))]
                let root_port_start_device: u8 = 0;

                // On the segment-0 root complex covered by an x86 IOMMU, the
                // phantom southbridge IOAPIC occupies a fixed devfn whose
                // device number must not be assigned to a root port. The
                // IOAPIC RID is on bus 0, so only reserve the device number on
                // the root complex whose bus range includes bus 0; reserving
                // it on other segment-0 root complexes would wrongly reject
                // otherwise-valid port counts. Reserve that device number so
                // that an overly large port count is rejected rather than
                // silently shadowing the IOAPIC entry.
                #[cfg(guest_arch = "x86_64")]
                let reserved_device_numbers: u32 = if rc.segment == 0
                    && rc.start_bus == 0
                    && matches!(
                        rc_iommu,
                        Some(PcieIommuConfig::AmdVi | PcieIommuConfig::IntelVtd)
                    ) {
                    1u32 << (ioapic_iommu_wiring::IOAPIC_PHANTOM_DEVFN >> 3)
                } else {
                    0
                };
                #[cfg(not(guest_arch = "x86_64"))]
                let reserved_device_numbers: u32 = 0;

                let root_complex =
                    chipset_builder
                        .arc_mutex_device(device_name)
                        .try_add(|services| {
                            let root_port_definitions = rc
                                .ports
                                .iter()
                                .map(pcie_topology::build_root_port_definition)
                                .collect();
                            GenericPcieRootComplex::builder(
                                &mut services.register_mmio(),
                                rc.start_bus..=rc.end_bus,
                                ranges.ecam_range,
                            )
                            .root_ports(
                                root_port_definitions,
                                &msi_conn.msi_target(rc_bus_range, 0),
                            )
                            .first_port_device_number(root_port_start_device)
                            .reserved_device_numbers(reserved_device_numbers)
                            .chbcr_range(chbcr_range)
                            .build()
                        })?;

                // Defer MSI wiring to after IOMMU setup so that
                // interrupt remapping can be applied if applicable.
                deferred_msi_conns.push(DeferredMsiConn {
                    msi_conn,
                    segment: rc.segment,
                    rc_idx,
                });

                let cxl = cxl_config
                    .map(|cxl| -> anyhow::Result<PcieHostBridgeCxlInfo> {
                        Ok(PcieHostBridgeCxlInfo {
                            chbcr_range: chbcr_range
                                .context("missing CHBCR range for CXL root complex")?,
                            hdm_range: hdm_range
                                .context("missing HDM range for CXL root complex")?,
                            hdm_window_restrictions: CfmwsWindowRestrictions::try_from_bits(
                                cxl.hdm_window_restrictions,
                            )
                            .context("invalid CFMWS HDM window restrictions")?,
                        })
                    })
                    .transpose()?;

                pcie_host_bridges.push(PcieHostBridge {
                    index: rc.index,
                    segment: rc.segment,
                    start_bus: rc.start_bus,
                    end_bus: rc.end_bus,
                    ecam_range: ranges.ecam_range,
                    low_mmio: ranges.low_mmio,
                    high_mmio: ranges.high_mmio,
                    cxl,
                    vnode: rc.vnode,
                    preserve_bars: rc.preserve_bars,
                    // A request to pin BARs (GPA = HPA) also requires the guest
                    // to leave the firmware's boot configuration alone.
                    preserve_boot_config: rc.preserve_bars,
                });

                pcie_root_complexes.push(root_complex.clone());

                let bus_id = vmotherboard::BusId::new(&rc.name);
                chipset_builder.register_weak_mutex_pcie_enumerator(bus_id, Box::new(root_complex));
            }

            (pcie_host_bridges, pcie_root_complexes)
        };

        // Build a port-name→(segment, bus_range) map covering all ports in
        // the PCIe topology (root complex ports and switch downstream ports).
        // The segment is used for ITS device ID composition; the bus_range is
        // a shared atomic that the config space emulator updates when the
        // guest programs secondary/subordinate bus numbers.
        struct PortInfo {
            segment: u16,
            bus_range: pci_core::bus_range::AssignedBusRange,
            rc_idx: usize,
        }
        let mut port_info: std::collections::HashMap<Arc<str>, PortInfo> =
            std::collections::HashMap::new();
        for (rc_idx, (hb, rc)) in pcie_host_bridges
            .iter()
            .zip(pcie_root_complexes.iter())
            .enumerate()
        {
            for p in rc.lock().downstream_ports() {
                if let Some(_existing) = port_info.insert(
                    p.name.clone(),
                    PortInfo {
                        segment: hb.segment,
                        bus_range: p.bus_range,
                        rc_idx,
                    },
                ) {
                    anyhow::bail!("duplicate PCIe port name '{}'", p.name);
                }
            }
        }

        for switch in cfg.pcie_switches {
            let device_name = format!("pcie-switch:{}", switch.name);

            // Inherit the segment and RC index from the switch's parent port.
            let parent_port_info = port_info.get(switch.parent_port.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "switch '{}' parent port '{}' not found in any root complex",
                    switch.name,
                    switch.parent_port
                )
            })?;
            let parent_segment = parent_port_info.segment;
            let parent_rc_idx = parent_port_info.rc_idx;

            let msi_conn = pci_core::msi::MsiConnection::new();

            // Defer MSI wiring to after IOMMU setup.
            let msi_target = msi_conn.target().clone();

            deferred_msi_conns.push(DeferredMsiConn {
                msi_conn,
                segment: parent_segment,
                rc_idx: parent_rc_idx,
            });

            let switch_device = chipset_builder
                .arc_mutex_device(device_name)
                .on_pcie_port(vmotherboard::BusId::new(&switch.parent_port))
                .try_add(|_services| {
                    let downstream_ports = switch
                        .ports
                        .iter()
                        .map(pcie_topology::build_root_port_definition)
                        .collect();
                    let definition = pcie::switch::GenericPcieSwitchDefinition {
                        name: switch.name.clone().into(),
                        downstream_ports,
                        msi_target,
                    };
                    GenericPcieSwitch::new(definition)
                })?;

            // Query the switch's actual downstream port names instead of
            // reconstructing them from the naming convention.
            for p in switch_device.lock().downstream_ports() {
                if let Some(_existing) = port_info.insert(
                    p.name.clone(),
                    PortInfo {
                        segment: parent_segment,
                        bus_range: p.bus_range,
                        rc_idx: parent_rc_idx,
                    },
                ) {
                    anyhow::bail!("duplicate PCIe port name '{}'", p.name);
                }
            }

            let bus_id = vmotherboard::BusId::new(&switch.name);
            chipset_builder.register_weak_mutex_pcie_enumerator(bus_id, Box::new(switch_device));
        }

        // Collect SRAT generic-initiator sources from the configured
        // generic-initiator entries, which can target any named port including
        // switch downstream ports. This is the single place that validates and
        // resolves each entry: we check the referenced NUMA node exists and
        // look up the port in the live topology. We capture each port's live
        // bus-range handle here rather than predict a bus number; the actual
        // secondary bus is read from it at ACPI-build time, after PCI resource
        // assignment runs. `port_info` contains every root port and switch
        // downstream port at this point.
        let num_nodes = cfg.numa.nodes.len() as u32;
        let mut generic_initiator_sources = Vec::new();
        for gi in &cfg.pcie_generic_initiators {
            if gi.node >= num_nodes {
                anyhow::bail!(
                    "PCIe generic initiator port '{}' references NUMA node {} which does not exist (num_nodes={num_nodes})",
                    gi.port_name,
                    gi.node
                );
            }
            let pi = port_info.get(gi.port_name.as_str()).with_context(|| {
                format!(
                    "generic initiator port '{}' not found in PCIe topology",
                    gi.port_name
                )
            })?;

            // A generic initiator's SRAT entry references its device by a fixed
            // segment/bus/device/function. If the guest re-enumerates PCIe and
            // reassigns bus numbers, that BDF would no longer point at the
            // device and the proximity-domain association would be lost. Ask the
            // host bridge backing this initiator's root complex to instruct the
            // guest to preserve the firmware boot configuration (via the
            // "Ignore PCI Boot Configurations" _DSM). Note this is distinct from
            // `preserve_bars`, which pins host BAR addresses for the assignment
            // algorithm — a generic initiator needs only stable bus numbers.
            pcie_host_bridges[pi.rc_idx].preserve_boot_config = true;

            generic_initiator_sources.push(GenericInitiatorSource {
                bus_range: pi.bus_range.clone(),
                segment: pi.segment,
                vnode: gi.node,
            });
        }

        // Register the VFIO resolver, which spawns a container manager task
        // internally to share containers across assigned devices.
        #[cfg(target_os = "linux")]
        let (vfio_inspect, vfio_cdev_inspect) = {
            let dma_mapper_client = memory_manager.dma_mapper_client();
            let vfio_resolver = vfio_assigned_device::resolver::VfioDeviceResolver::new(
                driver_source.builder().build("vfio-container-mgr"),
                dma_mapper_client.clone(),
            );
            let handle = vfio_resolver.inspect_handle();
            resolver.add_async_resolver::<
                vm_resource::kind::PciDeviceHandleKind,
                _,
                vfio_assigned_device_resources::VfioDeviceHandle,
                _,
            >(vfio_resolver);

            // Register the VFIO cdev + iommufd resolver for devices opened
            // via the cdev interface. Spawns a VfioCdevManager task that
            // shares IOAS contexts across devices with the same --iommu ID.
            let cdev_resolver = vfio_assigned_device::resolver::VfioCdevDeviceResolver::new(
                driver_source.builder().build("vfio-cdev-mgr"),
                dma_mapper_client,
            );
            let cdev_handle = cdev_resolver.inspect_handle();
            resolver.add_async_resolver::<
                vm_resource::kind::PciDeviceHandleKind,
                _,
                vfio_assigned_device_resources::VfioCdevDeviceHandle,
                _,
            >(cdev_resolver);

            (Some(handle), Some(cdev_handle))
        };

        // Instantiate SMMU devices and build port-level lookup maps.
        // When active, PCIe devices on the covered root complexes get
        // translating GuestMemory and SignalMsi wrappers that route DMA
        // and MSI writes through the emulated SMMUv3.
        #[cfg(guest_arch = "aarch64")]
        let smmu_devices = {
            let resolved: &[smmu_wiring::ResolvedSmmuResources] = match &resolved_iommu {
                ResolvedIommu::Smmu(resources) => resources,
                _ => &[],
            };
            smmu_wiring::setup_smmu(
                &cfg.pcie_root_complexes,
                resolved,
                &pcie_host_bridges,
                &chipset_builder,
                &gm,
            )?
        };

        // Instantiate an AMD IOMMU on each root complex listed in
        // --amd-iommu. Each IOMMU is an RCiEP at device 0, function 0 on
        // its root complex's start bus with a distinct MMIO base address.
        // Per-device wrappers are created in the PCIe device loop below.
        #[cfg(guest_arch = "x86_64")]
        let amd_devices = {
            let resolved: &[amd_iommu_wiring::ResolvedIommuResources] = match &resolved_iommu {
                ResolvedIommu::AmdVi(resources) => resources,
                _ => &[],
            };
            amd_iommu_wiring::setup_amd_iommu(
                resolved,
                &pcie_host_bridges,
                &chipset_builder,
                partition.as_ref(),
                &gm,
            )?
        };

        // Instantiate an Intel VT-d unit on each root complex listed in
        // --intel-vtd. Each unit is a pure MMIO platform device (no PCI
        // config space), discovered by the guest via the DMAR ACPI table.
        #[cfg(guest_arch = "x86_64")]
        let vtd_devices = {
            let resolved: &[intel_vtd_wiring::ResolvedVtdResources] = match &resolved_iommu {
                ResolvedIommu::IntelVtd(resources) => resources,
                _ => &[],
            };
            intel_vtd_wiring::setup_intel_vtd(
                resolved,
                &pcie_host_bridges,
                &chipset_builder,
                partition.as_ref(),
                &gm,
            )?
        };

        // Collapse the IOMMU setup results into the type-keyed enum.
        #[cfg(guest_arch = "x86_64")]
        let iommu_devices = match resolved_iommu {
            ResolvedIommu::AmdVi(_) => IommuDevices::AmdVi(amd_devices),
            ResolvedIommu::IntelVtd(_) => IommuDevices::IntelVtd(vtd_devices),
            ResolvedIommu::None => IommuDevices::None,
        };
        #[cfg(guest_arch = "aarch64")]
        let iommu_devices = match resolved_iommu {
            ResolvedIommu::Smmu(_) => IommuDevices::Smmu(smmu_devices),
            ResolvedIommu::None => IommuDevices::None,
        };

        // Set up IOAPIC routing. When an x86 IOMMU covers the southbridge
        // IOAPIC (segment 0, bus 0), wrap the hypervisor's IoApicRouting
        // through that IOMMU's interrupt remapping table so IOAPIC-sourced
        // interrupts are translated via the guest's IRTEs and invalidation
        // commands trigger retranslation. The RID and the remapper come from
        // the same IOMMU, so ACPI discovery and runtime remapping agree.
        #[cfg(guest_arch = "x86_64")]
        let ioapic_iommu = match &iommu_devices {
            IommuDevices::AmdVi(devices) => devices.ioapic_iommu.as_ref(),
            IommuDevices::IntelVtd(devices) => devices.ioapic_iommu.as_ref(),
            IommuDevices::None => None,
        };
        #[cfg(guest_arch = "x86_64")]
        let ioapic_iommu_rid: Option<u16> = ioapic_iommu.map(|sel| {
            ioapic_routing.connect_remapper(sel.ioapic_rid, sel.remapper.clone());
            sel.ioapic_rid
        });

        // Wire deferred root complex and switch MSI connections now that
        // IOMMU setup is complete. On x86_64, this applies IOMMU
        // interrupt remapping when the segment is covered.
        for deferred in deferred_msi_conns {
            #[cfg(guest_arch = "x86_64")]
            let iommu = x86_iommu_for_rc(&iommu_devices, deferred.rc_idx);
            pcie_wiring::PcieMsiPlatform {
                partition: partition.as_ref(),
                segment: deferred.segment,
                processor_topology: &processor_topology,
                #[cfg(guest_arch = "x86_64")]
                iommu,
            }
            .wrap_msi()
            .connect_to(&deferred.msi_conn);
        }

        // Resolve PCIe devices concurrently.
        //
        // When ITS is active, the root complex's ITS-wrapped SignalMsi
        // and IrqFd are shared across all devices on that complex. Each
        // device's MsiConnection carries a default BDF derived from the
        // port's AssignedBusRange, which the MsiTarget resolves lazily
        // at interrupt delivery time. When SMMU is enabled, per-device
        // wrappers translate IOVAs and MSI addresses through the emulated SMMU.
        // When the AMD IOMMU is enabled, per-device wrappers translate
        // IOVAs using the port's assigned bus range and remap MSIs using
        // the requester ID supplied by the PCI MSI path.

        try_join_all(cfg.pcie_devices.into_iter().map(|dev_cfg| {
            let chipset_builder = &chipset_builder;
            let driver_source = &driver_source;
            let resolver = &resolver;
            let gm = &gm;
            let partition = &partition;
            let mapper = &mapper;
            let port_info = &port_info;
            let processor_topology = &processor_topology;
            let iommu_devices = &iommu_devices;
            async move {
                let port_name: Arc<str> = dev_cfg.port_name.into();
                let pi = port_info.get(&port_name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "device port '{}' not found in any root complex or switch",
                        port_name
                    )
                })?;

                let msi_conn = pci_core::msi::MsiConnection::new();

                let pcie_ctx =
                    pcie_wiring::build_device_wiring(pcie_wiring::PcieDeviceWiringParams {
                        msi_platform: pcie_wiring::PcieMsiPlatform {
                            partition: partition.as_ref(),
                            segment: pi.segment,
                            processor_topology,
                            #[cfg(guest_arch = "x86_64")]
                            iommu: x86_iommu_for_rc(iommu_devices, pi.rc_idx),
                        },
                        guest_memory: gm,
                        bus_range: &pi.bus_range,
                        msi: &msi_conn,
                        #[cfg(guest_arch = "aarch64")]
                        smmu: smmu_for_rc(iommu_devices, pi.rc_idx),
                    });

                vmm_core::device_builder::build_pcie_device(
                    vmm_core::device_builder::PciDeviceResolveContext {
                        driver_source,
                        resolver,
                        resource: dev_cfg.resource,
                        doorbell_registration: partition
                            .clone()
                            .into_doorbell_registration(Vtl::Vtl0),
                        shared_mem_mapper: Some(mapper),
                    },
                    chipset_builder,
                    port_name.clone(),
                    &pcie_ctx.dma_target,
                )
                .await?;

                pcie_ctx.connect_to(&msi_conn);

                anyhow::Ok(())
            }
        }))
        .await?;

        if let Some(vmbus_cfg) = cfg.vmbus {
            if !cfg.hypervisor.with_hv {
                anyhow::bail!("vmbus required hypervisor enlightements");
            }

            let synic = partition
                .synic()
                .context("failed to get partition synic access for vmbus")?;

            vmbus_redirect = vmbus_cfg.vtl2_redirect;
            let hvsock_channel = HvsockRelayChannel::new();

            let (vtl2_vmbus, vtl2_request_send) = if let Some(vtl2_vmbus_cfg) = cfg.vtl2_vmbus {
                let (server_request_send, server_request_recv) = mesh::channel();
                let vtl2_hvsock_channel = HvsockRelayChannel::new();
                let vtl2_max_version = vtl2_vmbus_cfg
                    .vmbus_max_version
                    .map(vmbus_core::MaxVersionInfo::new);

                let vmbus_driver = driver_source.simple();
                let vtl2_vmbus =
                    VmbusServer::builder(vmbus_driver.clone(), synic.clone(), gm.clone())
                        .vtl(Vtl::Vtl2)
                        .max_version(vtl2_max_version)
                        .max_restore_version(vtl2_max_version)
                        .hvsock_notify(Some(vtl2_hvsock_channel.server_half))
                        .external_requests(Some(server_request_recv))
                        .enable_mnf(true)
                        .build()
                        .context("failed to create VTL2 vmbus server")?;

                let vtl2_vmbus = VmbusServerHandle::new(
                    &vmbus_driver,
                    state_units.add("vtl2_vmbus"),
                    vtl2_vmbus,
                )
                .context("failed to add vmbus state unit")?;

                let relay = HvsockRelay::new(
                    vmbus_driver,
                    vtl2_vmbus.control().clone(),
                    vtl2_hvsock_channel.relay_half,
                    vtl2_vmbus_cfg.vsock_path.map(Into::into),
                    vtl2_vmbus_cfg.vsock_listener,
                )
                .context("failed to create vtl2 hvsock relay")?;

                vtl2_hvsock_relay = Some(relay);

                (Some(vtl2_vmbus), Some(server_request_send))
            } else {
                (None, None)
            };

            let vmbus_max_version = vmbus_cfg
                .vmbus_max_version
                .map(vmbus_core::MaxVersionInfo::new);
            let vmbus_driver = driver_source.simple();
            let vmbus = VmbusServer::builder(vmbus_driver.clone(), synic.clone(), gm.clone())
                .hvsock_notify(Some(hvsock_channel.server_half))
                .external_server(vtl2_request_send)
                .use_message_redirect(vmbus_cfg.vtl2_redirect)
                .max_version(vmbus_max_version)
                .max_restore_version(vmbus_max_version)
                .delay_max_version(matches!(cfg.load_mode, LoadMode::Uefi { .. }))
                .enable_mnf(true)
                .build()
                .context("failed to create vmbus server")?;

            // Start the vmbus kernel proxy if it's in use.
            #[cfg(windows)]
            if let Some(proxy_handle) = vmbus_cfg.vmbusproxy_handle {
                vmbus_proxy =
                    Some(
                        vmbus_server::ProxyIntegration::builder(
                            &vmbus_driver,
                            proxy_handle,
                            vmbus_server::ProxyServerInfo::new(vmbus.control()),
                        )
                        .vtl2_server(vtl2_vmbus.as_ref().map(|server| {
                            vmbus_server::ProxyServerInfo::new(server.control().clone())
                        }))
                        .memory(Some(&gm))
                        .build()
                        .await
                        .context("failed to start the vmbus proxy")?,
                    )
            }

            let vmbus = VmbusServerHandle::new(&vmbus_driver, state_units.add("vmbus"), vmbus)
                .context("failed to add vmbus state unit")?;

            let relay = HvsockRelay::new(
                vmbus_driver,
                vmbus.control().clone(),
                hvsock_channel.relay_half,
                vmbus_cfg.vsock_path.map(Into::into),
                vmbus_cfg.vsock_listener,
            )
            .context("failed to create hvsock relay")?;

            vtl0_hvsock_relay = Some(relay);
            vmbus_server = Some(vmbus);
            vtl2_vmbus_server = vtl2_vmbus;
        }

        #[cfg(all(windows, feature = "virt_whp"))]
        fn make_ids(
            name: &str,
            instance_id: Option<guid::Guid>,
        ) -> (String, String, guid::Guid, u64) {
            let guid = instance_id.unwrap_or_else(guid::Guid::new_random);
            // TODO: clarify how the device ID is constructed
            let device_id = (guid.data2 as u64) << 16 | (guid.data3 as u64 & 0xfff8);
            let vpci_device_name = format!("vpci:{guid}");
            let device_name = format!("{name}:vpci-{guid}");
            (vpci_device_name, device_name, guid, device_id)
        }

        // Synthetic devices
        {
            #[cfg(windows)]
            for nic_config in cfg.kernel_vmnics {
                let mut nic = vmswitch::kernel::KernelVmNic::new(
                    &guid::Guid::new_random(),
                    "nic",
                    "nic",
                    nic_config.mac_address.into(),
                    &nic_config.instance_id,
                    vmbus_proxy
                        .as_ref()
                        .context("missing vmbusproxy handle")?
                        .handle(),
                )
                .context("failed to create a kernel vmnic")?;

                nic.connect(&vmswitch::kernel::SwitchPortId {
                    switch: nic_config.switch_port_id.switch,
                    port: nic_config.switch_port_id.port,
                })
                .context("failed to connect kernel vmnic")?;

                nic.resume().context("failed to resume the kernel vmnic")?;
                kernel_vmnics.push(nic);
            }

            if partition.supports_virtual_devices() {
                for dev_cfg in cfg.vpci_devices {
                    let vmbus = match dev_cfg.vtl {
                        DeviceVtl::Vtl0 => vmbus_server.as_ref().context("vmbus not enabled")?,
                        DeviceVtl::Vtl1 => anyhow::bail!("not supported"),
                        DeviceVtl::Vtl2 => vtl2_vmbus_server
                            .as_ref()
                            .context("VTL2 vmbus not enabled")?,
                    };

                    let vtl = match dev_cfg.vtl {
                        DeviceVtl::Vtl0 => Vtl::Vtl0,
                        DeviceVtl::Vtl1 => Vtl::Vtl1,
                        DeviceVtl::Vtl2 => Vtl::Vtl2,
                    };

                    vmm_core::device_builder::build_vpci_device(
                        vmm_core::device_builder::PciDeviceResolveContext {
                            driver_source: &driver_source,
                            resolver: &resolver,
                            resource: dev_cfg.resource,
                            doorbell_registration: partition
                                .clone()
                                .into_doorbell_registration(vtl),
                            shared_mem_mapper: Some(&mapper),
                        },
                        vmbus.control(),
                        &chipset_builder,
                        VpciBusConfig {
                            instance_id: dev_cfg.instance_id,
                            vtom: None,
                            vnode: dev_cfg
                                .vnode
                                .map(u16::try_from)
                                .transpose()
                                .context("vpci device vnode exceeds 65535")?,
                        },
                        gm.clone(),
                        |device_id| {
                            let hv_device = partition.new_virtual_device(
                                match dev_cfg.vtl {
                                    DeviceVtl::Vtl0 => Vtl::Vtl0,
                                    DeviceVtl::Vtl1 => Vtl::Vtl1,
                                    DeviceVtl::Vtl2 => Vtl::Vtl2,
                                },
                                device_id,
                            )?;
                            Ok((
                                hv_device.clone().target(),
                                hv_device.clone().interrupt_mapper(),
                            ))
                        },
                    )
                    .await?;
                }

                #[cfg(all(windows, feature = "virt_whp"))]
                for resource in cfg.vpci_resources {
                    let vmbus = vmbus_server
                        .as_ref()
                        .context("vmbus must be enabled to assign devices")?
                        .control()
                        .as_ref();

                    // TODO: abstract this behind the trait object properly.
                    let pd = partition.as_any();
                    let p = pd.downcast_ref::<virt_whp::WhpPartition>().unwrap();
                    let (vpci_bus_name, device_name, instance_id, device_id) =
                        make_ids("assigned-device", None);

                    let hv_device = Arc::new(
                        p.new_physical_device(Vtl::Vtl0, device_id, resource.0)
                            .context("failed to get physical device for assignment")?,
                    );

                    let device = chipset_builder
                        .arc_mutex_device(device_name)
                        .with_external_pci()
                        .try_add(|services| {
                            virt_whp::device::AssignedPciDevice::new(
                                &mut services.register_mmio(),
                                hv_device.clone(),
                            )
                        })
                        .context("failed to assign device")?;

                    chipset_builder
                        .arc_mutex_device(vpci_bus_name)
                        .try_add_async(async |services| {
                            VpciBus::new(
                                &driver_source,
                                VpciBusConfig {
                                    instance_id,
                                    vtom: None,
                                    vnode: None,
                                },
                                device,
                                &mut services.register_mmio(),
                                vmbus,
                                crate::partition::VpciDevice::interrupt_mapper(hv_device),
                            )
                            .await
                        })
                        .await?;
                }
            }
        }

        // Add vmbus devices.
        let mut vmbus_devices = Vec::new();
        for (vtl, resource) in cfg.vmbus_devices {
            let vmbus = match vtl {
                DeviceVtl::Vtl0 => vmbus_server
                    .as_ref()
                    .context("failed to find vmbus for vtl0"),
                DeviceVtl::Vtl1 => anyhow::bail!("vtl1 scsi controllers unsupported"),
                DeviceVtl::Vtl2 => vtl2_vmbus_server
                    .as_ref()
                    .context("failed to find vmbus for vtl2"),
            }
            .with_context(|| format!("failed to resolve vmbus resource {}", resource.id()))?;
            vmbus_devices.push(
                offer_vmbus_device_handle_unit(
                    &driver_source,
                    &state_units,
                    vmbus,
                    &resolver,
                    resource,
                )
                .await?,
            );
        }

        // add virtio devices

        // Construct virtio devices. Virtio-mmio device addresses are resolved
        // by the memory layout allocator; each slot is a 4 KiB Mmio32
        // allocation indexed by the order of VirtioBus::Mmio devices.
        let mut pci_device_number = 10;
        let mut virtio_mmio_index = 0;

        // Avoid an ISA interrupt to avoid conflicts and to avoid needing to
        // configure the line as level-triggered in the MADT (necessary for
        // Linux when the PIC is missing).
        let virtio_mmio_irq = {
            const VIRTIO_MMIO_IOAPIC_IRQ: u32 = 17;
            const VIRTIO_MMIO_PIC_IRQ: u32 = 5;
            if cfg.chipset_capabilities.with_pic {
                VIRTIO_MMIO_PIC_IRQ
            } else {
                VIRTIO_MMIO_IOAPIC_IRQ
            }
        };
        for (bus, device) in cfg.virtio_devices.into_iter() {
            let id = device.id().to_string();
            let device = resolver
                .resolve(
                    device,
                    VirtioResolveInput {
                        driver_source: &driver_source,
                    },
                )
                .await?;
            match bus {
                VirtioBus::Mmio => {
                    let mmio_start = virtio_mmio_region.start() + virtio_mmio_index as u64 * 0x1000;
                    virtio_mmio_index += 1;
                    let id = format!("{id}-{mmio_start}");
                    let gm = gm.clone();
                    chipset_builder.arc_mutex_device(id).try_add(|services| {
                        VirtioMmioDevice::new(
                            device.0,
                            &driver_source.simple(),
                            gm,
                            services.new_line(IRQ_LINE_SET, "interrupt", virtio_mmio_irq),
                            partition.clone().into_doorbell_registration(Vtl::Vtl0),
                            mmio_start,
                            0x1000,
                        )
                    })?;
                }
                VirtioBus::Pci => {
                    let pci_inta_line = pci_inta_line.context("missing PCI INT#A line")?;

                    let device_number = pci_device_number;
                    pci_device_number += 1;
                    pci_legacy_interrupts.push(((device_number, None), pci_inta_line));

                    let bus = if cfg.chipset.with_piix4_pci_bus {
                        pci_bus_id_piix4.clone()
                    } else {
                        pci_bus_id_generic.clone()
                    };

                    chipset_builder
                        .arc_mutex_device(format!("{id}-pci"))
                        .with_pci_addr(0, device_number, 0)
                        .on_pci_bus(bus)
                        .try_add(|services| {
                            VirtioPciDevice::new(
                                device.0,
                                &driver_source.simple(),
                                gm.clone(),
                                PciInterruptModel::IntX(
                                    PciInterruptPin::IntA,
                                    services.new_line(IRQ_LINE_SET, "interrupt", pci_inta_line),
                                ),
                                partition.clone().into_doorbell_registration(Vtl::Vtl0),
                                &mut services.register_mmio(),
                                Some(&mapper),
                            )
                        })?;
                }
            }
        }

        let (chipset, devices) = chipset_builder.build()?;
        let (fatal_error_send, _fatal_error_recv) = mesh::channel();
        let chipset = vmm_core::vmotherboard_adapter::AdaptedChipset::new(
            chipset,
            // TODO: Support this being a cmd line option
            vmm_core::vmotherboard_adapter::FatalErrorPolicy::DebugBreak(fatal_error_send),
        );

        // create a new channel to intercept guest resets
        let (halt_send, halt_recv) = mesh::channel();

        let (partition_unit, vp_runners) = PartitionUnit::new(
            driver_source.simple(),
            state_units
                .add("partition")
                .depends_on(devices.chipset_unit())
                .depends_on(vmtime.handle()),
            partition.clone().into_vm_partition(),
            PartitionUnitParams {
                processor_topology: &processor_topology,
                halt_vps,
                halt_request_recv,
                client_notify_send: halt_send,
                vtl_guest_memory: [
                    Some(&gm),
                    None,
                    cfg.hypervisor.with_vtl2.is_some().then_some(&gm),
                ],
                debugger_rpc: cfg.debugger_rpc,
            },
        )
        .context("failed to create partition unit")?;

        // Start the VP backing threads.
        try_join_all(vps.into_iter().zip(vp_runners).enumerate().map(
            |(vp_index, (mut vp, runner))| {
                let partition = partition.clone();
                let chipset = chipset.clone();
                let (send, recv) = mesh::oneshot();
                thread::Builder::new()
                    .name(format!("vp-{}", vp_index))
                    .spawn(move || match vp.bind() {
                        Ok(mut vp) => {
                            send.send(Ok(()));
                            block_on_vp(
                                partition,
                                VpIndex::new(vp_index as u32),
                                vp.run(runner, &chipset),
                            )
                        }
                        Err(err) => {
                            send.send(Err(err));
                        }
                    })
                    .unwrap();

                async move {
                    recv.await
                        .unwrap()
                        .with_context(|| format!("failed to bind vp {vp_index}"))
                }
            },
        ))
        .await?;

        let mut this = LoadedVm {
            state_units,
            running: false,
            inner: LoadedVmInner {
                driver_source,
                resolver,
                partition_unit,
                partition,
                chipset_devices: devices,
                _vmtime: vmtime,
                memory_manager,
                gm,
                vtl0_hvsock_relay,
                vtl2_hvsock_relay,
                vmbus_server,
                vtl2_vmbus_server,
                hypervisor_cfg: cfg.hypervisor,
                numa_cfg: cfg.numa,
                mem_layout,
                processor_topology,
                vmbus_redirect,
                input_distributor,
                vtl2_framebuffer_gpa_base,
                #[cfg(windows)]
                _vmbus_proxy: vmbus_proxy,
                #[cfg(windows)]
                _kernel_vmnics: kernel_vmnics,
                vmbus_devices,
                chipset_cfg: cfg.chipset,
                chipset_capabilities: cfg.chipset_capabilities,
                firmware_event_send: cfg.firmware_event_send,
                load_mode: cfg.load_mode,
                virtio_mmio_region,
                virtio_mmio_irq,
                chipset_mmio,
                pci_legacy_interrupts,
                igvm_file,
                next_igvm_file: None,
                _vmgs_task: vmgs_task,
                vmgs_client_inspect_handle,
                #[cfg(target_os = "linux")]
                vfio_inspect,
                #[cfg(target_os = "linux")]
                vfio_cdev_inspect,
                halt_recv,
                client_notify_send,
                automatic_guest_reset: cfg.automatic_guest_reset,
                chipset: chipset.chipset.clone(),
                iommu_devices,
                #[cfg(guest_arch = "x86_64")]
                ioapic_iommu_rid,
                pcie_host_bridges,
                pcie_root_complexes,
                generic_initiator_sources,
                pcie_hotplug_devices: Vec::new(),
            },
        };

        if let Some(saved_state) = saved_state {
            this.restore(saved_state)
                .await
                .context("loadedvm restore failed")?;
        } else {
            // Assign PCI bus numbers/BARs before building firmware so that the
            // ACPI tables (specifically the SRAT generic-initiator entries) can
            // read the assigned secondary bus numbers from the root ports'
            // bridge registers.
            this.assign_pci_resources().await?;
            this.inner.load_firmware(false).await?;
        }

        Ok(this)
    }
}

impl LoadedVmInner {
    fn slit_info(&self) -> Option<SlitInfo> {
        let num_nodes = self.numa_cfg.nodes.len();
        if num_nodes <= 1 && self.numa_cfg.distances.is_empty() {
            return None;
        }
        Some(SlitInfo {
            num_nodes,
            distances: self
                .numa_cfg
                .distances
                .iter()
                .map(|d| (d.src, d.dst, d.distance))
                .collect(),
        })
    }

    async fn load_firmware(&mut self, vtl2_only: bool) -> anyhow::Result<()> {
        let cache_topology = if cfg!(guest_arch = "aarch64") {
            Some(
                cache_topology::CacheTopology::from_host()
                    .context("failed to get cache topology")?,
            )
        } else {
            None
        };
        let slit_info = self.slit_info();
        // Resolve each generic-initiator source to its assigned PCI bus now that
        // PCI resource assignment has programmed the root ports' bridge
        // bus-number registers. The device is function 0 of device 0 on the
        // port's secondary bus.
        let generic_initiators: Vec<GenericInitiator> = self
            .generic_initiator_sources
            .iter()
            .map(|s| {
                let (secondary_bus, _subordinate) = s.bus_range.bus_range();
                GenericInitiator {
                    segment: s.segment,
                    bus: secondary_bus,
                    device: 0,
                    function: 0,
                    vnode: s.vnode,
                }
            })
            .collect();
        let acpi_builder = AcpiTablesBuilder {
            processor_topology: &self.processor_topology,
            mem_layout: &self.mem_layout,
            cache_topology: cache_topology.as_ref(),
            pcie_host_bridges: &self.pcie_host_bridges,
            slit_info: slit_info.as_ref(),
            generic_initiators: &generic_initiators,
            #[cfg(guest_arch = "x86_64")]
            arch: vmm_core::acpi_builder::AcpiArchConfig::X86 {
                with_ioapic: self.chipset_capabilities.with_ioapic,
                with_psp: self.chipset_cfg.with_generic_psp,
                with_pic: self.chipset_capabilities.with_pic,
                with_pit: self.chipset_capabilities.with_pit,
                pm_base: PM_BASE,
                acpi_irq: SYSTEM_IRQ_ACPI,
                iommu: match &self.iommu_devices {
                    IommuDevices::AmdVi(devices) => {
                        Some(vmm_core::acpi_builder::X86IommuAcpiConfig::AmdVi(
                            vmm_core::acpi_builder::AmdIommuIvrsConfig {
                                pa_size: amd_iommu::PA_SIZE,
                                va_size: amd_iommu::VA_SIZE,
                                iommus: devices.acpi_configs.clone(),
                                ioapic_rid: self.ioapic_iommu_rid,
                            },
                        ))
                    }
                    IommuDevices::IntelVtd(devices) => {
                        Some(vmm_core::acpi_builder::X86IommuAcpiConfig::IntelVtd(
                            vmm_core::acpi_builder::IntelVtdDmarConfig {
                                host_address_width: 48,
                                units: devices.acpi_configs.clone(),
                                ioapic_rid: self.ioapic_iommu_rid,
                            },
                        ))
                    }
                    IommuDevices::None => None,
                },
            },
            #[cfg(guest_arch = "aarch64")]
            arch: vmm_core::acpi_builder::AcpiArchConfig::Aarch64 {
                hypervisor_vendor_identity: if self.hypervisor_cfg.with_hv {
                    u64::from_le_bytes(*b"MsHyperV")
                } else {
                    0
                },
                virt_timer_ppi: self.processor_topology.virt_timer_ppi(),
                smmu: match &self.iommu_devices {
                    IommuDevices::Smmu(devices) => devices.configs.clone(),
                    IommuDevices::None => Vec::new(),
                },
            },
        };

        if vtl2_only {
            assert!(matches!(self.load_mode, LoadMode::Igvm { .. }));
        }

        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_mut))]
        let InitialLoad {
            mut regs,
            page_imports: initial_page_imports,
        } = match &self.load_mode {
            LoadMode::None => return Ok(()),
            #[cfg(guest_arch = "x86_64")]
            &LoadMode::Linux {
                ref kernel,
                ref initrd,
                ref cmdline,
                enable_serial,
                ref custom_dsdt,
                boot_mode,
            } => {
                match boot_mode {
                    openvmm_defs::config::LinuxDirectBootMode::DeviceTree => {
                        anyhow::bail!("device tree boot mode is not supported on x86_64");
                    }
                    openvmm_defs::config::LinuxDirectBootMode::Acpi => {}
                }
                let kernel_config = super::vm_loaders::linux::KernelConfig {
                    kernel,
                    initrd,
                    cmdline,
                    mem_layout: &self.mem_layout,
                    isolation: self.hypervisor_cfg.with_isolation,
                };
                super::vm_loaders::linux::load_linux_x86(&kernel_config, &self.gm, |gpa| {
                    let tables = if let Some(dsdt) = custom_dsdt {
                        acpi_builder.build_acpi_tables_custom_dsdt(gpa, dsdt)
                    } else {
                        acpi_builder.build_acpi_tables(gpa, |dsdt| {
                            add_devices_to_dsdt_x64(
                                dsdt,
                                &self.chipset_cfg,
                                &self.chipset_capabilities,
                                enable_serial,
                                self.vmbus_server.is_some(),
                                &self.chipset_mmio,
                                self.virtio_mmio_region,
                                self.virtio_mmio_irq,
                                &self.pci_legacy_interrupts,
                            )
                        })
                    };

                    loader::linux::AcpiTables {
                        rsdp: tables.rsdp,
                        tables: tables.tables,
                    }
                })?
            }
            #[cfg(guest_arch = "aarch64")]
            &LoadMode::Linux {
                ref kernel,
                ref initrd,
                ref cmdline,
                enable_serial,
                custom_dsdt: _,
                boot_mode,
            } => {
                use openvmm_defs::config::LinuxDirectBootMode;

                let kernel_config = super::vm_loaders::linux::KernelConfig {
                    kernel,
                    initrd,
                    cmdline,
                    mem_layout: &self.mem_layout,
                    isolation: self.hypervisor_cfg.with_isolation,
                };

                let build_acpi = if boot_mode == LinuxDirectBootMode::Acpi {
                    Some(|rsdp_gpa: u64| {
                        acpi_builder.build_acpi_tables(rsdp_gpa, |dsdt| {
                            add_devices_to_dsdt_arm64(
                                dsdt,
                                enable_serial,
                                self.vmbus_server.is_some(),
                                &self.chipset_mmio,
                                self.hypervisor_cfg.with_hv,
                            )
                        })
                    })
                } else {
                    None
                };

                let smmu_configs: &[vmm_core::acpi_builder::AcpiSmmuConfig] =
                    match &self.iommu_devices {
                        IommuDevices::Smmu(devices) => &devices.configs,
                        IommuDevices::None => &[],
                    };
                super::vm_loaders::linux::load_linux_arm64(
                    &kernel_config,
                    &self.gm,
                    enable_serial,
                    &self.processor_topology,
                    &self.pcie_host_bridges,
                    smmu_configs,
                    &self.chipset_mmio,
                    build_acpi,
                )?
            }
            &LoadMode::Uefi {
                ref firmware,
                enable_debugging,
                enable_memory_protections,
                disable_frontpage,
                enable_tpm,
                enable_battery,
                enable_serial,
                enable_vpci_boot,
                uefi_console_mode,
                default_boot_always_attempt,
                bios_guid,
                enable_vmbus,
                force_dma_bounce,
            } => {
                let acpi_tables = [
                    // MADT
                    Some(acpi_builder.build_madt()),
                    // SRAT
                    Some(acpi_builder.build_srat()),
                    // SLIT
                    acpi_builder.build_slit(),
                    // MCFG
                    (!self.pcie_host_bridges.is_empty()).then(|| acpi_builder.build_mcfg()),
                    // PPTT
                    cache_topology.is_some().then(|| acpi_builder.build_pptt()),
                    // IORT
                    acpi_builder.build_iort(),
                    // IVRS (AMD IOMMU)
                    acpi_builder.build_ivrs(),
                    // DMAR (Intel VT-d)
                    acpi_builder.build_dmar(),
                ];
                let acpi_tables: Vec<_> =
                    acpi_tables.iter().flatten().map(|t| t.as_ref()).collect();

                let load_settings = super::vm_loaders::uefi::UefiLoadSettings {
                    debugging: enable_debugging,
                    memory_protections: enable_memory_protections,
                    frontpage: !disable_frontpage,
                    tpm: enable_tpm,
                    battery: enable_battery,
                    guest_watchdog: self.chipset_capabilities.with_guest_watchdog,
                    vpci_boot: enable_vpci_boot,
                    serial: enable_serial,
                    uefi_console_mode,
                    default_boot_always_attempt,
                    bios_guid,
                    vmbus: enable_vmbus,
                    force_dma_bounce,
                };
                let regs =
                    super::vm_loaders::uefi::load_uefi(&super::vm_loaders::uefi::LoadUefiParams {
                        firmware,
                        gm: &self.gm,
                        processor_topology: &self.processor_topology,
                        mem_layout: &self.mem_layout,
                        pcie_host_bridges: &self.pcie_host_bridges,
                        settings: load_settings,
                        chipset_mmio: &self.chipset_mmio,
                        acpi_tables: &acpi_tables,
                    })?;

                InitialLoad {
                    regs,
                    page_imports: Vec::new(),
                }
            }
            #[cfg(guest_arch = "x86_64")]
            LoadMode::Pcat { .. } => {
                let regs = super::vm_loaders::pcat::load_pcat(&self.gm, &self.mem_layout)?;

                InitialLoad {
                    regs,
                    page_imports: Vec::new(),
                }
            }
            &LoadMode::Igvm {
                file: _,
                ref cmdline,
                vtl2_base_address,
                com_serial,
            } => {
                let madt = acpi_builder.build_madt();
                let srat = acpi_builder.build_srat();
                let slit = acpi_builder.build_slit();
                const ENTROPY_SIZE: usize = 64;
                let mut entropy = [0u8; ENTROPY_SIZE];
                getrandom::fill(&mut entropy).unwrap();

                let params = crate::worker::vm_loaders::igvm::LoadIgvmParams {
                    igvm_file: self.igvm_file.as_ref().expect("should be already read"),
                    gm: &self.gm,
                    processor_topology: &self.processor_topology,
                    mem_layout: &self.mem_layout,
                    cmdline,
                    acpi_tables: super::vm_loaders::igvm::AcpiTables {
                        madt: &madt,
                        srat: &srat,
                        slit: slit.as_deref(),
                        pptt: None,
                    },
                    vtl2_base_address,
                    vtl2_framebuffer_gpa_base: self.vtl2_framebuffer_gpa_base,
                    vtl2_only,
                    with_vmbus_redirect: self.vmbus_redirect,
                    com_serial,
                    entropy: Some(&entropy),
                    chipset_mmio: self.chipset_mmio,
                };
                super::vm_loaders::igvm::load_igvm(params)?
            }

            #[expect(clippy::allow_attributes)]
            #[allow(unreachable_patterns)]
            _ => anyhow::bail!("load mode not supported on this platform"),
        };

        // Don't setup variable MTRRs if VTL2 is present. It's expected that
        // VTL2 will setup MTRRs for VTL0 if needed.
        #[cfg(guest_arch = "x86_64")]
        if self.hypervisor_cfg.with_vtl2.is_none() {
            regs.extend(loader::common::compute_variable_mtrrs(
                &self.mem_layout,
                self.partition.caps().physical_address_width,
                self.chipset_mmio.low,
                self.chipset_mmio.high,
            ));
        }

        let initial_regs = initial_regs(
            &regs,
            self.partition.caps(),
            &self.processor_topology.vp_arch(VpIndex::BSP),
        );

        tracing::debug!(?initial_regs, "initial_registers");
        self.partition_unit
            .set_initial_regs(
                if self.hypervisor_cfg.with_vtl2.is_some() {
                    Vtl::Vtl2
                } else {
                    Vtl::Vtl0
                },
                initial_regs,
            )
            .await
            .context("failed to set initial register state")?;

        // Only finalize initial page imports on isolated partitions.
        //
        // TODO: Today with SNP guests, this issues a SNP_LAUNCH_FINISH on the
        // only supported backend KVM, so load the initial registers before
        // finalizing the imported pages. We should revisit this in the future
        // when KVM supports loading VMSA pages, which would probably be
        // imported as a page, not registers, along with revisiting other
        // isolation architectures and backends.
        if self.hypervisor_cfg.with_isolation.is_some() {
            tracing::debug!(?initial_page_imports);
            self.partition_unit
                .accept_initial_pages(initial_page_imports)
                .await
                .context("failed to finalize initial page imports")?;
        }

        Ok(())
    }
}

impl LoadedVm {
    async fn resume(&mut self) -> bool {
        if self.running {
            return false;
        }
        self.state_units.start().await;
        self.running = true;
        true
    }

    async fn pause(&mut self) -> bool {
        if !self.running {
            return false;
        }
        self.state_units.stop().await;
        self.running = false;
        true
    }

    /// Assign PCI bus numbers and BAR addresses for all boot modes.
    ///
    /// This pre-programs PCI config space (bus numbers, bridge windows,
    /// BAR addresses) before the guest starts. For UEFI, the firmware
    /// is told via a config flag to skip its own PCI enumeration and
    /// use the pre-assigned resources. For Linux direct boot, this is
    /// required since there is no firmware to do PCI enumeration.
    ///
    /// This must only be called on clean boot or reset, not on
    /// snapshot restore (where config space is already populated from
    /// saved state).
    ///
    /// Config space accesses go through device state units (via the ECAM
    /// MMIO path), so devices must be running. This method temporarily
    /// starts state units with VPs held stopped, performs the assignment,
    /// then stops state units again. The caller is responsible for
    /// resuming normally afterward.
    async fn assign_pci_resources(&mut self) -> anyhow::Result<()> {
        if self.inner.pcie_host_bridges.is_empty() {
            return Ok(());
        }

        // Hold VPs so they don't execute when state units start the
        // partition unit.
        let stop_guard = self.inner.partition_unit.temporarily_stop_vps().await;

        // Start state units so device config space is accessible.
        self.state_units.start().await;

        let result = ecam_config_access::assign_pci_resources_for_root_complexes(
            &self.inner.chipset,
            &self.inner.pcie_host_bridges,
        )
        .await
        .context("PCI resource assignment failed");

        // Stop state units again; the caller will resume normally.
        self.state_units.stop().await;
        drop(stop_guard);

        result
    }

    pub async fn run(
        mut self,
        driver: &impl Spawn,
        mut rpc_recv: mesh::Receiver<VmRpc>,
        mut worker_rpc: mesh::Receiver<WorkerRpc<RestartState>>,
    ) {
        enum Event {
            WorkerRpc(Result<WorkerRpc<RestartState>, mesh::RecvError>),
            VmRpc(Result<VmRpc, mesh::RecvError>),
            Halt(Result<HaltReason, mesh::RecvError>),
        }

        // Start a task to handle state unit inspections by filtering the worker
        // RPC requests. This is done so that inspect on state units works even
        // during state transitions.
        let (worker_rpc_send, worker_rpc_recv) = mesh::channel();
        let _filter_rpc_task = driver.spawn("loaded-vm-worker-rpc-filter", {
            let state_units = self.state_units.inspector();
            async move {
                while let Some(rpc) = worker_rpc.next().await {
                    match rpc {
                        WorkerRpc::Inspect(req) => req.respond(|resp| {
                            resp.merge(&state_units)
                                .merge(inspect::send(&worker_rpc_send, WorkerRpc::Inspect));
                        }),
                        rpc => worker_rpc_send.send(rpc),
                    }
                }
            }
        });
        let mut worker_rpc = worker_rpc_recv;

        loop {
            let event: Event = {
                let a = rpc_recv.recv().map(Event::VmRpc);
                let b = worker_rpc.recv().map(Event::WorkerRpc);
                let c = self.inner.halt_recv.recv().map(Event::Halt);
                (a, b, c).race().await
            };

            match event {
                Event::WorkerRpc(Err(_)) => break,
                Event::WorkerRpc(Ok(message)) => match message {
                    WorkerRpc::Stop => break,
                    WorkerRpc::Restart(rpc) => {
                        let mut stopped = false;
                        // First run the non-destructive operations.
                        let r = async {
                            let shared_memory = self.inner.memory_manager.shared_memory_backing();
                            if shared_memory.is_none() {
                                anyhow::bail!("restart is not supported with --private-memory");
                            }
                            if self.running {
                                self.state_units.stop().await;
                                stopped = true;
                            }
                            let saved_state = self.save().await?;
                            anyhow::Ok((shared_memory, saved_state))
                        }
                        .await;
                        match r {
                            Ok((shared_memory, saved_state)) => {
                                rpc.complete(Ok(self
                                    .serialize(rpc_recv, shared_memory, saved_state)
                                    .await));

                                return;
                            }
                            Err(err) => {
                                if stopped {
                                    self.state_units.start().await;
                                }
                                rpc.complete(Err(RemoteError::new(err)));
                            }
                        }
                    }
                    WorkerRpc::Inspect(deferred) => deferred.respond(|resp| {
                        resp.field("memory", &self.inner.memory_manager)
                            .field("memory_layout", &self.inner.mem_layout)
                            .field("resolver", &self.inner.resolver)
                            .field("vmgs", &self.inner.vmgs_client_inspect_handle);
                        #[cfg(target_os = "linux")]
                        resp.field("vfio", &self.inner.vfio_inspect)
                            .field("vfio_cdev", &self.inner.vfio_cdev_inspect);
                    }),
                },
                Event::VmRpc(Err(_)) => break,
                Event::VmRpc(Ok(message)) => match message {
                    VmRpc::Reset(rpc) => {
                        rpc.handle_failable(async |()| self.reset(true).await).await
                    }
                    VmRpc::ClearHalt(rpc) => {
                        rpc.handle(async |()| self.inner.partition_unit.clear_halt().await)
                            .await
                    }
                    VmRpc::Resume(rpc) => rpc.handle(async |()| self.resume().await).await,
                    VmRpc::Pause(rpc) => rpc.handle(async |()| self.pause().await).await,
                    VmRpc::Save(rpc) => {
                        rpc.handle_failable(async |()| self.save().await.map(ProtobufMessage::new))
                            .await
                    }
                    VmRpc::Nmi(rpc) => rpc.handle_sync(|vpindex| {
                        if vpindex < self.inner.processor_topology.vp_count() {
                            // Send an NMI MSI to the processor. We could raise
                            // LINT1 instead, which would allow the guest to
                            // reconfigure the LINT to do something other than
                            // an NMI. Since this is for diagnostics, that
                            // doesn't seem like what we want.
                            //
                            // AARCH64-TODO: is there an equivalent?
                            #[cfg(guest_arch = "x86_64")]
                            self.inner.partition.request_msi(
                                Vtl::Vtl0,
                                virt::irqcon::MsiRequest::new_x86(
                                    virt::irqcon::DeliveryMode::NMI,
                                    self.inner
                                        .processor_topology
                                        .vp_arch(VpIndex::new(vpindex))
                                        .apic_id,
                                    false,
                                    0,
                                    false,
                                ),
                            );
                        }
                    }),
                    VmRpc::AddVmbusDevice(rpc) => {
                        rpc.handle_failable(async |(vtl, resource)| {
                            let vmbus = match vtl {
                                DeviceVtl::Vtl0 => self.inner.vmbus_server.as_ref(),
                                DeviceVtl::Vtl1 => None,
                                DeviceVtl::Vtl2 => self.inner.vtl2_vmbus_server.as_ref(),
                            }
                            .context("no vmbus available")?;
                            let device = offer_vmbus_device_handle_unit(
                                &self.inner.driver_source,
                                &self.state_units,
                                vmbus,
                                &self.inner.resolver,
                                resource,
                            )
                            .await?;
                            self.inner.vmbus_devices.push(device);
                            self.state_units.start_stopped_units().await;
                            anyhow::Ok(())
                        })
                        .await
                    }
                    VmRpc::ConnectHvsock(rpc) => {
                        let ((mut ctx, service_id, vtl), response) = rpc.split();
                        if let Some(relay) = self.hvsock_relay(vtl) {
                            let fut = relay.connect(&mut ctx, service_id);
                            driver
                                .spawn("vmrpc-hvsock-connect", async move {
                                    response.complete(fut.await.map_err(RemoteError::new))
                                })
                                .detach();
                        } else {
                            response.complete(Err(RemoteError::new(anyhow::anyhow!(
                                "hvsock is not available"
                            ))));
                        }
                    }
                    VmRpc::PulseSaveRestore(rpc) => {
                        rpc.handle(async |()| {
                            if !self.inner.partition.supports_reset() {
                                return Err(PulseSaveRestoreError::ResetNotSupported);
                            }
                            let paused = self.pause().await;
                            self.save_reset_restore().await?;

                            if paused {
                                self.resume().await;
                            }
                            Ok(())
                        })
                        .await
                    }
                    VmRpc::StartReloadIgvm(rpc) => {
                        rpc.handle_failable_sync(|file| self.start_reload_igvm(&file))
                    }
                    VmRpc::CompleteReloadIgvm(rpc) => {
                        rpc.handle_failable(async |complete| {
                            self.complete_reload_igvm(complete).await
                        })
                        .await
                    }
                    VmRpc::ReadMemory(rpc) => {
                        rpc.handle_failable_sync(|(gpa, size)| {
                            let mut bytes = vec![0u8; size];
                            self.inner
                                .gm
                                .read_at(gpa, bytes.as_mut_slice())
                                .map(|_| bytes)
                        });
                    }
                    VmRpc::WriteMemory(rpc) => rpc.handle_failable_sync(|(gpa, bytes)| {
                        self.inner.gm.write_at(gpa, bytes.as_slice())
                    }),
                    VmRpc::UpdateCliParams(rpc) => {
                        rpc.handle_failable_sync(|params| match &mut self.inner.load_mode {
                            LoadMode::Igvm { cmdline, .. } => {
                                *cmdline = params;
                                Ok(())
                            }
                            _ => anyhow::bail!(
                                "Updating command line parameters is only supported for Igvm load mode"
                            ),
                        })
                    }
                    VmRpc::AddPcieDevice(rpc) => {
                        rpc.handle_failable(async |(port_name, resource)| {
                            // Find the root complex and its index for the named port.
                            let (rc_idx, rc) = self.inner.pcie_root_complexes.iter()
                                .enumerate()
                                .find(|(_, rc)| {
                                    rc.lock().downstream_ports().iter().any(|p| p.name.as_ref() == port_name.as_str())
                                })
                                .ok_or_else(|| anyhow::anyhow!("port '{}' not found in any root complex", port_name))?;

                            // Get the bus_range from the port's config space emulator.
                            let bus_range = rc.lock()
                                .downstream_ports()
                                .into_iter()
                                .find(|p| p.name.as_ref() == port_name.as_str())
                                .expect("port was just found above")
                                .bus_range;

                            let segment = self.inner.pcie_host_bridges[rc_idx].segment;
                            let msi_conn = pci_core::msi::MsiConnection::new();

                            let pcie_ctx = pcie_wiring::build_device_wiring(
                                pcie_wiring::PcieDeviceWiringParams {
                                    msi_platform: pcie_wiring::PcieMsiPlatform {
                                        partition: self.inner.partition.as_ref(),
                                        segment,
                                        processor_topology: &self.inner.processor_topology,
                                        #[cfg(guest_arch = "x86_64")]
                                        iommu: x86_iommu_for_rc(
                                            &self.inner.iommu_devices,
                                            rc_idx,
                                        ),
                                    },
                                    guest_memory: &self.inner.gm,
                                    bus_range: &bus_range,
                                    msi: &msi_conn,
                                    #[cfg(guest_arch = "aarch64")]
                                    smmu: smmu_for_rc(&self.inner.iommu_devices, rc_idx),
                                },
                            );

                            let (unit, device) = self.inner.chipset_devices.add_dyn_device(
                                &self.inner.driver_source,
                                &self.state_units,
                                format!("pcie-hotplug:{}", port_name),
                                async |register_mmio| {
                                    self.inner.resolver
                                        .resolve(
                                            resource,
                                            pci_resources::ResolvePciDeviceHandleParams {
                                                dma_target: &pcie_ctx.dma_target,
                                                register_mmio,
                                                driver_source: &self.inner.driver_source,
                                                doorbell_registration: self.inner.partition.clone().into_doorbell_registration(Vtl::Vtl0),
                                                shared_mem_mapper: None,
                                            },
                                        )
                                        .await
                                        .map(|r| r.0)
                                        .map_err(|e| anyhow::anyhow!(e))
                                },
                            ).await?;

                            // Connect the signal_msi and irqfd (possibly
                            // ITS-wrapped and/or SMMU-wrapped).
                            pcie_ctx.connect_to(&msi_conn);

                            // Wrap the device as a GenericPciBusDevice for the port.
                            // Keep a strong Arc to the device so the Weak stays valid.
                            let weak_dev: std::sync::Weak<closeable_mutex::CloseableMutex<dyn chipset_device::ChipsetDevice>> = Arc::downgrade(&(device.clone() as Arc<closeable_mutex::CloseableMutex<dyn chipset_device::ChipsetDevice>>));
                            let bus_device = Box::new(WeakMutexPciBusDevice(weak_dev));

                            self.inner.pcie_hotplug_devices.push((port_name.clone(), unit, device));

                            // Start the device unit before firing the hotplug
                            // MSI. The guest may begin probing config space
                            // immediately after receiving the interrupt, so
                            // the device must be ready first.
                            self.state_units.start_stopped_units().await;

                            // Now attach the device and notify the guest.
                            if let Err(e) = rc.lock().hotplug_add_device(
                                &port_name,
                                "hotplug-device",
                                bus_device,
                            ) {
                                // Clean up the device unit on failure
                                let (_, unit, _) = self.inner.pcie_hotplug_devices.pop().unwrap();
                                unit.remove().await;
                                return Err(e);
                            }
                            anyhow::Ok(())
                        })
                        .await
                    }
                    VmRpc::RemovePcieDevice(rpc) => {
                        rpc.handle_failable(async |port_name: String| {
                            // Only allow removing dynamically hot-added devices.
                            // Statically-attached devices don't have a tracked unit
                            // and removing them would leave their state unit/MMIO
                            // registrations running.
                            let idx = self.inner.pcie_hotplug_devices.iter()
                                .position(|(name, _, _)| name == &port_name)
                                .ok_or_else(|| anyhow::anyhow!(
                                    "no hot-added device on port '{}' (only dynamically added devices can be hot-removed)",
                                    port_name
                                ))?;

                            // Find the root complex containing the target port
                            let rc = self.inner.pcie_root_complexes.iter()
                                .find(|rc| {
                                    rc.lock().downstream_ports().iter().any(|p| p.name.as_ref() == port_name.as_str())
                                })
                                .ok_or_else(|| anyhow::anyhow!("port '{}' not found in any root complex", port_name))?;

                            rc.lock().hotplug_remove_device(&port_name)?;

                            // Remove and stop the device unit
                            let (_, unit, _device) = self.inner.pcie_hotplug_devices.remove(idx);
                            unit.remove().await;

                            anyhow::Ok(())
                        })
                        .await
                    }
                    VmRpc::DumpState(rpc) => {
                        rpc.handle_failable(async |file| self.dump_state(file).await)
                            .await
                    }
                },
                Event::Halt(Err(_)) => break,
                Event::Halt(Ok(reason)) => {
                    if matches!(reason, HaltReason::Reset) && self.inner.automatic_guest_reset {
                        tracing::info!("guest-initiated reset");
                        if let Err(err) = self.reset(true).await {
                            tracing::error!(?err, "failed to reset VM");
                            break;
                        }
                    } else {
                        self.inner.client_notify_send.send(reason);
                    }
                }
            }
        }

        self.inner.partition_unit.teardown().await;
        if let Some(vmbus) = self.inner.vmbus_server {
            vmbus.remove().await.shutdown().await;
        }
    }

    fn start_reload_igvm(&mut self, file: &File) -> anyhow::Result<()> {
        // Clear any previously staged IGVM file.
        self.inner.next_igvm_file = None;

        // Load the new IGVM file into memory.
        let igvm_file =
            super::vm_loaders::igvm::read_igvm_file(file).context("reading igvm file failed")?;

        self.inner.next_igvm_file = Some(igvm_file);
        Ok(())
    }

    async fn complete_reload_igvm(&mut self, complete: bool) -> anyhow::Result<()> {
        if !complete {
            self.inner.next_igvm_file = None;
            return Ok(());
        }

        // Grab the staged IGVM file.
        let next_igvm_file = self
            .inner
            .next_igvm_file
            .take()
            .context("no staged igvm file")?;

        // Stop the partition and VTL2 vmbus so that we can reset vmbus and
        // reset the VTL2 register state.
        //
        // When these units will be resumed when `stopped_units` is dropped.
        let vtl2_vmbus = self
            .inner
            .vtl2_vmbus_server
            .as_ref()
            .context("missing vtl2 vmbus")?;

        // Stop the VPs so that VTL2 state can be replaced.
        let stop_vps = self.inner.partition_unit.temporarily_stop_vps().await;

        // Reset vmbus VTL2 state so that all DMA transactions to VTL2
        // memory stop. We don't need to reset the individual devices, since
        // resetting vmbus will close all the channels.
        //
        // This must be done after the VPs have been stopped to avoid
        // confusing VTL2 and to ensure that VTL2 does not send any
        // additional vmbus messages.
        vtl2_vmbus
            .control()
            .force_reset()
            .await
            .context("failed to reset vtl2 vmbus")?;

        // Reload the VTL2 firmware.
        //
        // When the initial registers are set, this will implicitly reset VTL2
        // state as well.
        let _old_igvm_file = self.inner.igvm_file.replace(next_igvm_file);
        self.inner
            .load_firmware(true)
            .await
            .context("failed to reload VTL2 firmware")?;

        // OK to resume the VPs now.
        drop(stop_vps);
        Ok(())
    }

    /// Get the associated hvsock relay for a given vtl, if any.
    fn hvsock_relay(&self, vtl: DeviceVtl) -> Option<&HvsockRelay> {
        match vtl {
            DeviceVtl::Vtl0 => self.inner.vtl0_hvsock_relay.as_ref(),
            DeviceVtl::Vtl1 => None,
            DeviceVtl::Vtl2 => self.inner.vtl2_hvsock_relay.as_ref(),
        }
    }

    /// Saves the VM's processor, partition, and device state.
    ///
    /// TODO: virtio & vmbus unsupported.
    async fn save(&mut self) -> anyhow::Result<SavedState> {
        Ok(SavedState {
            units: self.state_units.save().await?,
        })
    }

    /// Restore state on the VM.
    async fn restore(&mut self, state: SavedState) -> anyhow::Result<()> {
        self.state_units.restore(state.units).await?;
        Ok(())
    }

    /// Do a save, reset, restore.
    async fn save_reset_restore(&mut self) -> anyhow::Result<()> {
        let state = self.save().await?;
        self.reset(false).await?;
        self.restore(state).await?;
        Ok(())
    }

    /// Prepares for restart, serializing the worker's state.
    async fn serialize(
        mut self,
        rpc: mesh::Receiver<VmRpc>,
        shared_memory: Option<SharedMemoryBacking>,
        saved_state: SavedState,
    ) -> RestartState {
        let notify = self.inner.partition_unit.teardown().await;
        let input = self.inner.input_distributor.remove().await.into_inner();

        if let Some(vmbus_server) = self.inner.vmbus_server.take() {
            vmbus_server.remove().await.shutdown().await;
        }

        let manifest = Manifest {
            load_mode: self.inner.load_mode,
            floppy_disks: vec![],            // TODO
            ide_disks: vec![],               // TODO
            pcie_root_complexes: vec![],     // TODO
            pcie_devices: vec![],            // TODO
            pcie_switches: vec![],           // TODO
            pcie_generic_initiators: vec![], // TODO
            vpci_devices: vec![],            // TODO
            numa: self.inner.numa_cfg,
            processor_topology: self.inner.processor_topology.to_config(),
            chipset: self.inner.chipset_cfg,
            vmbus: None,      // TODO
            vtl2_vmbus: None, // TODO
            hypervisor: self.inner.hypervisor_cfg,
            #[cfg(windows)]
            kernel_vmnics: vec![], // TODO
            input,
            framebuffer: None,      // TODO
            vga_firmware: None,     // TODO
            vtl2_gfx: false,        // TODO
            virtio_devices: vec![], // TODO
            #[cfg(all(windows, feature = "virt_whp"))]
            vpci_resources: vec![], // TODO
            vmgs: None,             // TODO
            secure_boot_enabled: false, // TODO
            custom_uefi_vars: Default::default(), // TODO
            firmware_event_send: self.inner.firmware_event_send,
            debugger_rpc: None,          // TODO
            vmbus_devices: vec![],       // TODO
            chipset_devices: vec![],     // TODO
            pci_chipset_devices: vec![], // TODO
            isa_dma_controller: None,    // TODO
            chipset_capabilities: self.inner.chipset_capabilities,
            layout: vmm_core_defs::LayoutConfig {
                chipset_low_mmio_size: 0,
                chipset_high_mmio_size: 0,
                vtl2_chipset_mmio_size: 0,
            }, // TODO
            rtc_delta_milliseconds: 0, // TODO
            automatic_guest_reset: self.inner.automatic_guest_reset,
            efi_diagnostics_log_level: Default::default(),
        };
        #[expect(unreachable_code, reason = "TODO")]
        RestartState {
            manifest,
            running: self.running,
            saved_state,
            shared_memory,
            rpc,
            notify,
            hypervisor: todo!("TODO: RestartState serialization is broken"),
        }
    }

    async fn reset(&mut self, reload_firmware: bool) -> anyhow::Result<()> {
        let resume = self.pause().await;

        self.state_units.reset().await?;
        // TODO: _vmnic
        // TODO: gdb?

        // Load again
        if reload_firmware {
            // Assign PCI resources before rebuilding firmware so the ACPI
            // tables reflect the freshly assigned bus numbers.
            self.assign_pci_resources().await?;
            self.inner.load_firmware(false).await?;
        }

        if resume {
            self.resume().await;
        }
        Ok(())
    }
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
fn add_devices_to_dsdt_x64(
    dsdt: &mut dsdt::Dsdt,
    cfg: &BaseChipsetManifest,
    capabilities: &VmChipsetCapabilities,
    serial_uarts: bool,
    with_vmbus: bool,
    chipset_mmio: &ChipsetMmioRanges,
    virtio_mmio_region: MemoryRange,
    virtio_mmio_irq: u32,
    pci_legacy_interrupts: &[((u8, Option<u8>), u32)], // ((device, function), interrupt)
) {
    dsdt.add_apic();

    // Any serial port configured means all are enabled.
    if serial_uarts {
        for (name, com_port, ddn, uid) in [
            (b"\\_SB.UAR1", ComPort::Com1, b"COM1", 1),
            (b"\\_SB.UAR2", ComPort::Com2, b"COM2", 2),
            (b"\\_SB.UAR3", ComPort::Com3, b"COM3", 3),
            (b"\\_SB.UAR4", ComPort::Com4, b"COM4", 4),
        ]
        .iter()
        .copied()
        {
            dsdt.add_uart(name, ddn, uid, com_port.io_port(), com_port.irq().into());
        }
    }

    // Virtio-mmio devices are allocated as a contiguous region by the memory
    // layout resolver. Each 4 KiB slot is a separate device.
    for i in 0..virtio_mmio_region.page_count_4k() {
        let slot_base = virtio_mmio_region.start() + i * HV_PAGE_SIZE;
        let mut device = dsdt::Device::new(format!("\\_SB.VI{i:02}").as_bytes());
        device.add_object(&dsdt::NamedString::new(b"_HID", b"LNRO0005"));
        device.add_object(&dsdt::NamedInteger::new(b"_UID", i));
        let mut crs = dsdt::CurrentResourceSettings::new();
        crs.add_resource(&dsdt::QwordMemory::new(slot_base, HV_PAGE_SIZE));
        let mut intr = dsdt::Interrupt::new(virtio_mmio_irq);
        intr.is_edge_triggered = false;
        crs.add_resource(&intr);
        device.add_object(&crs);
        dsdt.add_object(&device);
    }

    // The chipset MMIO module or PCI bus describes the chipset low/high
    // MMIO regions to the guest. Either range may be empty (e.g. when
    // VMBus is disabled, chipset high MMIO is not allocated).
    if cfg.with_generic_pci_bus || capabilities.with_i440bx_host_pci_bridge {
        // TODO: actually plumb through legacy PCI interrupts
        dsdt.add_pci(chipset_mmio.low, chipset_mmio.high, pci_legacy_interrupts);
    } else {
        dsdt.add_mmio_module(chipset_mmio.low, chipset_mmio.high);
    }

    if with_vmbus {
        dsdt.add_vmbus(
            cfg.with_generic_pci_bus || capabilities.with_i440bx_host_pci_bridge,
            None,
        );
    }
    dsdt.add_rtc();
}

#[cfg(guest_arch = "aarch64")]
fn add_devices_to_dsdt_arm64(
    dsdt: &mut dsdt::Dsdt,
    enable_serial: bool,
    with_vmbus: bool,
    chipset_mmio: &ChipsetMmioRanges,
    with_hv: bool,
) {
    // VMBus GIC INTID (PPI 2 = INTID 16 + 2 = 18), matching the DT path.
    const VMBUS_INTID: u32 = openvmm_defs::config::DEFAULT_VMBUS_PPI;
    // SBSA UART MMIO bases and sizes.
    const PL011_SERIAL0_BASE: u64 = 0xEFFEC000;
    const PL011_SERIAL1_BASE: u64 = 0xEFFEB000;
    const PL011_SERIAL_SIZE: u64 = 0x1000;
    // UART GSIVs (SPI 1 = INTID 33, SPI 2 = INTID 34).
    const PL011_SERIAL0_GSIV: u32 = 33;
    const PL011_SERIAL1_GSIV: u32 = 34;

    if with_hv {
        dsdt.add_mmio_module(chipset_mmio.low, chipset_mmio.high);
    }

    if with_vmbus {
        // VMBus on ARM64 ACPI needs a per-CPU interrupt (PPI) in _CRS.
        // Always place under VMOD, not PCI0 — ARM64 doesn't use the x86
        // PCI0 DSDT node.
        dsdt.add_vmbus(false, Some(VMBUS_INTID));
    }

    if enable_serial {
        dsdt.add_sbsa_uart(
            b"\\_SB.UAR0",
            0,
            PL011_SERIAL0_BASE,
            PL011_SERIAL_SIZE,
            PL011_SERIAL0_GSIV,
        );
        dsdt.add_sbsa_uart(
            b"\\_SB.UAR1",
            1,
            PL011_SERIAL1_BASE,
            PL011_SERIAL_SIZE,
            PL011_SERIAL1_GSIV,
        );
    }
}

struct WatchdogTimeout {
    halt_vps: Arc<Halt>,
    watchdog_send: Option<mesh::Sender<()>>,
}

#[async_trait::async_trait]
impl WatchdogCallback for WatchdogTimeout {
    async fn on_timeout(&mut self) {
        // Report the timeout as its own halt reason; the VMM's guest-watchdog
        // action decides whether to reset (default), halt, or exit.
        self.halt_vps.halt(HaltReason::Watchdog);

        if let Some(watchdog_send) = &self.watchdog_send {
            watchdog_send.send(());
        }
    }
}

#[derive(MeshPayload, Clone)]
struct OpenVmmRemoteDynamicResolvers {}

impl chipset_device_worker::RemoteDynamicResolvers for OpenVmmRemoteDynamicResolvers {
    const WORKER_ID_STR: &str = "openvmm_remote_chipset_worker";

    async fn register_remote_dynamic_resolvers(
        self,
        _resolver: &mut ResourceResolver,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

mesh_worker::register_workers! {
    chipset_device_worker::worker::RemoteChipsetDeviceWorker<OpenVmmRemoteDynamicResolvers>
}

/// Wrapper around `Weak<CloseableMutex<dyn ChipsetDevice>>` that implements
/// [`GenericPciBusDevice`] for PCIe hotplug devices.
struct WeakMutexPciBusDevice(
    std::sync::Weak<closeable_mutex::CloseableMutex<dyn chipset_device::ChipsetDevice>>,
);

impl pci_bus::GenericPciBusDevice for WeakMutexPciBusDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> Option<IoResult> {
        Some(
            self.0
                .upgrade()?
                .lock()
                .supports_pci()?
                .pci_cfg_read(offset, value),
        )
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> Option<IoResult> {
        Some(
            self.0
                .upgrade()?
                .lock()
                .supports_pci()?
                .pci_cfg_write(offset, value),
        )
    }

    fn pci_cfg_read_with_routing(
        &mut self,
        access_type: PciConfigAccessType,
        address: PciConfigAddress,
        value: ByteEnabledDwordRead<'_>,
    ) -> Option<IoResult> {
        Some(
            self.0
                .upgrade()?
                .lock()
                .supports_pci()?
                .pci_cfg_read_with_routing(access_type, address, value),
        )
    }

    fn pci_cfg_write_with_routing(
        &mut self,
        access_type: PciConfigAccessType,
        address: PciConfigAddress,
        value: ByteEnabledDwordWrite,
    ) -> Option<IoResult> {
        Some(
            self.0
                .upgrade()?
                .lock()
                .supports_pci()?
                .pci_cfg_write_with_routing(access_type, address, value),
        )
    }
}
