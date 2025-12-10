// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrappers for Hyper-V Powershell Cmdlets

use crate::CommandError;
use crate::OpenHclServicingFlags;
use crate::StorageType;
use crate::VmScreenshotMeta;
use crate::run_host_cmd;
use anyhow::Context;
use core::str;
use guid::Guid;
use jiff::Timestamp;
use powershell_builder as ps;
use powershell_builder::PowerShellBuilder;
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

/// Hyper-V VM Generation
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HyperVGeneration {
    /// Generation 1 (with emulated legacy devices and PCAT BIOS)
    One,
    /// Generation 2 (synthetic devices and UEFI)
    Two,
}

impl ps::AsVal for HyperVGeneration {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        match self {
            HyperVGeneration::One => "1",
            HyperVGeneration::Two => "2",
        }
    }
}

/// Hyper-V Guest State Isolation Type
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Debug)]
#[serde(try_from = "i32")]
pub enum HyperVGuestStateIsolationType {
    /// Trusted Launch (OpenHCL, SecureBoot, TPM)
    TrustedLaunch = 0,
    /// VBS
    Vbs = 1,
    /// SNP
    Snp = 2,
    /// TDX
    Tdx = 3,
    /// OpenHCL but no isolation
    OpenHCL = 16,
    /// No HCL and no isolation
    Disabled = -1,
}

impl TryFrom<i32> for HyperVGuestStateIsolationType {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            -1 => Ok(HyperVGuestStateIsolationType::Disabled),
            0 => Ok(HyperVGuestStateIsolationType::TrustedLaunch),
            1 => Ok(HyperVGuestStateIsolationType::Vbs),
            2 => Ok(HyperVGuestStateIsolationType::Snp),
            3 => Ok(HyperVGuestStateIsolationType::Tdx),
            16 => Ok(HyperVGuestStateIsolationType::OpenHCL),
            _ => Err(format!("Unknown isolation type: {}", value)),
        }
    }
}

impl ps::AsVal for HyperVGuestStateIsolationType {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        match self {
            HyperVGuestStateIsolationType::TrustedLaunch => "TrustedLaunch",
            HyperVGuestStateIsolationType::Vbs => "VBS",
            HyperVGuestStateIsolationType::Snp => "SNP",
            HyperVGuestStateIsolationType::Tdx => "TDX",
            HyperVGuestStateIsolationType::OpenHCL => "OpenHCL",
            HyperVGuestStateIsolationType::Disabled => "Disabled",
        }
    }
}

/// Hyper-V Secure Boot Template
#[derive(Clone, Copy)]
pub enum HyperVSecureBootTemplate {
    /// Windows Secure Boot Template
    MicrosoftWindows,
    /// Microsoft UEFI Certificate Authority Template
    MicrosoftUEFICertificateAuthority,
    /// Open Source Shielded VM Template
    OpenSourceShieldedVM,
}

impl ps::AsVal for HyperVSecureBootTemplate {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        match self {
            HyperVSecureBootTemplate::MicrosoftWindows => "MicrosoftWindows",
            HyperVSecureBootTemplate::MicrosoftUEFICertificateAuthority => {
                "MicrosoftUEFICertificateAuthority"
            }
            HyperVSecureBootTemplate::OpenSourceShieldedVM => "OpenSourceShieldedVM",
        }
    }
}

/// Arguments for the New-VM powershell cmdlet
pub struct HyperVNewVMArgs<'a> {
    /// Specifies the name of the new virtual machine.
    pub name: &'a str,
    /// Specifies the generation for the virtual machine.
    pub generation: Option<HyperVGeneration>,
    /// Specifies the Guest State Isolation Type
    pub guest_state_isolation_type: Option<HyperVGuestStateIsolationType>,
    /// Specifies the amount of memory, in bytes, to assign to the virtual machine.
    pub memory_startup_bytes: Option<u64>,
    /// Specifies the directory to store the files for the new virtual machine.
    pub path: Option<&'a Path>,
    /// Specifies the path to a virtual hard disk file.
    pub vhd_path: Option<&'a Path>,
    /// Specifies the path to the guest state file for the virtual machine
    /// being created.
    pub source_guest_state_path: Option<&'a Path>,
}

/// Runs New-VM with the given arguments.
pub async fn run_new_vm(args: HyperVNewVMArgs<'_>) -> anyhow::Result<Guid> {
    let vmid = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("New-VM")
            .arg("Name", args.name)
            .arg_opt("Generation", args.generation)
            .arg_opt("GuestStateIsolationType", args.guest_state_isolation_type)
            .arg_opt("MemoryStartupBytes", args.memory_startup_bytes)
            .arg_opt("Path", args.path)
            .arg_opt("VHDPath", args.vhd_path)
            .arg_opt("SourceGuestStatePath", args.source_guest_state_path)
            .flag("Force")
            .pipeline()
            .cmdlet("Select-Object")
            .arg("ExpandProperty", "Id")
            .pipeline()
            .cmdlet("Select-Object")
            .arg("ExpandProperty", "Guid")
            .finish()
            .build(),
    )
    .await
    .context("new_vm")?;

    Guid::from_str(&vmid).context("invalid vmid")
}

/// Runs New-VM with the given arguments.
pub async fn run_remove_vm(vmid: &Guid) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Remove-VM")
            .flag("Force")
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("remove_vm")
}

/// Arguments for the Set-VMProcessor powershell cmdlet
pub struct HyperVSetVMProcessorArgs {
    /// Specifies the number of virtual processors to assign to the virtual
    /// machine. If not specified, the number of virtual processors is not
    /// changed.
    pub count: Option<u32>,
    /// Specifies the Hyper-V APIC mode to use for the virtual machine.
    pub apic_mode: Option<HyperVApicMode>,
    /// Specifies the number of hardware threads per core to assign to the
    /// VM.
    pub hw_thread_count_per_core: Option<u32>,
    /// The maximum number of virtual processors that can be assigned to a
    /// NUMA node.
    pub maximum_count_per_numa_node: Option<u32>,
}

/// The Hyper-V APIC mode
#[derive(Clone, Copy)]
pub enum HyperVApicMode {
    /// Default APIC mode (what is this, exactly? It seems to not always include
    /// x2apic support).
    Default,
    /// Legacy APIC mode (no x2apic support).
    Legacy,
    /// x2apic mode (enabled by default? or just supported? unclear)
    X2Apic,
}

impl ps::AsVal for HyperVApicMode {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        match self {
            HyperVApicMode::Default => "Default",
            HyperVApicMode::Legacy => "Legacy",
            HyperVApicMode::X2Apic => "x2Apic",
        }
    }
}

/// Runs Set-VMProcessor with the given arguments.
pub async fn run_set_vm_processor(
    vmid: &Guid,
    args: &HyperVSetVMProcessorArgs,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-VMProcessor")
            .arg_opt("Count", args.count)
            .arg_opt("ApicMode", args.apic_mode)
            .arg_opt("HwThreadCountPerCore", args.hw_thread_count_per_core)
            .arg_opt("MaximumCountPerNumaNode", args.maximum_count_per_numa_node)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vm_processor")
}

/// Arguments for the Set-VMMemory powershell cmdlet.
#[derive(Default)]
pub struct HyperVSetVMMemoryArgs {
    /// Specifies whether to enable dynamic memory for the virtual machine.
    pub dynamic_memory_enabled: Option<bool>,
    /// Specifies the maximum amount of memory, in bytes, to assign to the virtual
    /// machine.
    pub maximum_bytes: Option<u64>,
    /// Specifies the minimum amount of memory, in bytes, to assign to the virtual
    /// machine.
    pub minimum_bytes: Option<u64>,
    /// Specifies the startup amount of memory, in bytes, to assign to the
    /// virtual machine.
    pub startup_bytes: Option<u64>,
}

/// Runs Set-VMMemory with the given arguments.
pub async fn run_set_vm_memory(vmid: &Guid, args: &HyperVSetVMMemoryArgs) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-VMMemory")
            .arg_opt("DynamicMemoryEnabled", args.dynamic_memory_enabled)
            .arg_opt("MaximumBytes", args.maximum_bytes)
            .arg_opt("MinimumBytes", args.minimum_bytes)
            .arg_opt("StartupBytes", args.startup_bytes)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vm_memory")
}

/// Arguments for the Add-VMHardDiskDrive powershell cmdlet
pub struct HyperVAddVMHardDiskDriveArgs<'a> {
    /// Specifies the ID of the virtual machine to which the hard disk
    /// drive is to be added.
    pub vmid: &'a Guid,
    /// Specifies the type of controller to which the hard disk drive is
    /// to be added.
    pub controller_type: ControllerType,
    /// Specifies the number of the location on the controller at which the
    /// hard disk drive is to be added. If not specified, the first available
    /// location in the controller specified with the ControllerNumber parameter
    /// is used.
    pub controller_location: Option<u8>,
    /// Specifies the number of the controller to which the hard disk drive is
    /// to be added. If not specified, this parameter assumes the value of the
    /// first available controller at the location specified in the
    /// ControllerLocation parameter.
    pub controller_number: Option<u32>,
    /// Specifies the full path of the hard disk drive file to be added.
    pub path: Option<&'a Path>,
}

/// The type of controller to which a hard disk drive is to be added.
#[derive(Copy, Clone, Debug)]
pub enum ControllerType {
    /// IDE controller
    Ide,
    /// SCSI controller
    Scsi,
    /// Persistent memory controller
    Pmem,
}

impl ps::AsVal for ControllerType {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        match self {
            ControllerType::Ide => "IDE",
            ControllerType::Scsi => "SCSI",
            ControllerType::Pmem => "PMem",
        }
    }
}

impl From<StorageType> for ControllerType {
    fn from(value: StorageType) -> Self {
        match value {
            StorageType::Ide => Self::Ide,
            StorageType::Scsi => Self::Scsi,
            StorageType::Nvme => todo!(),
        }
    }
}

/// Runs Add-VMHardDiskDrive with the given arguments.
pub async fn run_add_vm_hard_disk_drive(
    args: HyperVAddVMHardDiskDriveArgs<'_>,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", args.vmid)
            .pipeline()
            .cmdlet("Add-VMHardDiskDrive")
            .arg("ControllerType", args.controller_type)
            .arg_opt("ControllerLocation", args.controller_location)
            .arg_opt("ControllerNumber", args.controller_number)
            .arg_opt("Path", args.path)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("add_vm_hard_disk_drive")
}

/// Arguments for the Add-VMDvdDrive powershell cmdlet
pub struct HyperVAddVMDvdDriveArgs<'a> {
    /// Specifies the ID of the virtual machine on which the DVD drive
    /// is to be configured.
    pub vmid: &'a Guid,
    /// Specifies the IDE controller location of the DVD drives to be
    /// configured. If not specified, DVD drives in all controller locations
    /// are configured.
    pub controller_location: Option<u32>,
    /// Specifies the IDE controller of the DVD drives to be configured.
    /// If not specified, DVD drives attached to all controllers are configured.
    pub controller_number: Option<u32>,
    /// Specifies the path to the ISO file or physical DVD drive that will serv
    /// as media for the virtual DVD drive.
    pub path: Option<&'a Path>,
}

/// Runs Add-VMDvdDrive with the given arguments.
pub async fn run_add_vm_dvd_drive(args: HyperVAddVMDvdDriveArgs<'_>) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", args.vmid)
            .pipeline()
            .cmdlet("Add-VMDvdDrive")
            .arg_opt("ControllerLocation", args.controller_location)
            .arg_opt("ControllerNumber", args.controller_number)
            .arg_opt("Path", args.path)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("add_vm_dvd_drive")
}

/// Runs Add-VMScsiController with the given arguments.
///
/// Returns the controller number and controller instance guid.
pub async fn run_add_vm_scsi_controller(ps_mod: &Path, vmid: &Guid) -> anyhow::Result<(u32, Guid)> {
    let output = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Add-VMScsiController")
            .flag("Passthru")
            .pipeline()
            .cmdlet("Get-VmScsiControllerProperties")
            .finish()
            .build(),
    )
    .await
    .context("add_vm_scsi_controller")?;

    let mut out = output.trim().split(',');
    let controller_number = out
        .next()
        .context("no output")?
        .parse::<u32>()
        .context("invalid controller number")?;
    let vsid = out
        .next()
        .context("no vsid")?
        .parse::<Guid>()
        .context("vsid not a guid")?;

    Ok((controller_number, vsid))
}

/// Sets the target VTL for a SCSI controller.
pub async fn run_set_vm_scsi_controller_target_vtl(
    ps_mod: &Path,
    vmid: &Guid,
    controller_number: u32,
    target_vtl: u32,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-VMScsiControllerTargetVtl")
            .arg("ControllerNumber", controller_number)
            .arg("TargetVtl", target_vtl)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vm_scsi_controller_target_vtl")
}

/// Runs Dismount-VHD with the given arguments.
pub async fn run_dismount_vhd(path: &Path) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Dismount-VHD")
            .arg("Path", path)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("dismount_vhd")
}

/// Arguments for the Set-VMFirmware powershell cmdlet
pub struct HyperVSetVMFirmwareArgs<'a> {
    /// Specifies the ID of virtual machines for which you want to modify the
    /// firmware configuration.
    pub vmid: &'a Guid,
    /// Whether to enable secure boot
    pub secure_boot_enabled: Option<bool>,
    /// Specifies the name of the secure boot template. If secure boot is
    /// enabled, you must have a valid secure boot template for the guest
    /// operating system to start.
    pub secure_boot_template: Option<HyperVSecureBootTemplate>,
}

/// Runs Set-VMFirmware with the given arguments.
pub async fn run_set_vm_firmware(args: HyperVSetVMFirmwareArgs<'_>) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", args.vmid)
            .pipeline()
            .cmdlet("Set-VMFirmware")
            .arg_opt(
                "EnableSecureBoot",
                args.secure_boot_enabled.map(|enabled| {
                    if enabled {
                        ps::RawVal::new("On")
                    } else {
                        ps::RawVal::new("Off")
                    }
                }),
            )
            .arg_opt("SecureBootTemplate", args.secure_boot_template)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vm_firmware")
}

/// Runs Set-OpenHCLFirmware with the given arguments.
pub async fn run_set_openhcl_firmware(
    vmid: &Guid,
    ps_mod: &Path,
    igvm_file: &Path,
    increase_vtl2_memory: bool,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-OpenHCLFirmware")
            .arg("IgvmFile", igvm_file)
            .flag_opt(increase_vtl2_memory.then_some("IncreaseVtl2Memory"))
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_openhcl_firmware")
}

/// Runs Set-VmCommandLine with the given arguments.
pub async fn run_set_vm_command_line(
    vmid: &Guid,
    ps_mod: &Path,
    command_line: impl AsRef<str>,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-VmCommandLine")
            .arg("CommandLine", command_line.as_ref())
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vm_command_line")
}

/// Sets the initial machine configuration for a VM
pub async fn run_set_initial_machine_configuration(
    vmid: &Guid,
    ps_mod: &Path,
    imc_hive: &Path,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-InitialMachineConfiguration")
            .arg("ImcHive", imc_hive)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_initial_machine_configuration")
}

/// Enables the specified vm com port and binds it to the named pipe path
pub async fn run_set_vm_com_port(vmid: &Guid, port: u8, path: &Path) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-VMComPort")
            .arg("Number", port)
            .arg("Path", path)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vm_com_port")
}

/// Run Set-VMBusRelay commandlet
pub async fn run_set_vmbus_redirect(
    vmid: &Guid,
    ps_mod: &Path,
    enable: bool,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-VMBusRedirect")
            .arg("Enable", enable)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_vmbus_redirect")
}

/// Runs Restart-OpenHCL, which will perform and OpenHCL servicing operation.
pub async fn run_restart_openhcl(
    vmid: &Guid,
    ps_mod: &Path,
    flags: OpenHclServicingFlags,
) -> anyhow::Result<()> {
    // No NVMe storage, so no keepalive. Prevent us from silently thinking that we're testing this feature.
    // Tracked by #1649.
    if flags.enable_nvme_keepalive {
        return Err(anyhow::anyhow!(
            "enable_nvme_keepalive is not yet supported for HyperV VMs"
        ));
    }
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Restart-OpenHCL")
            .arg_opt("TimeoutHintSeconds", flags.stop_timeout_hint_secs)
            .flag_opt(
                flags
                    .override_version_checks
                    .then_some("OverrideVersionChecks"),
            )
            .flag_opt((!flags.enable_nvme_keepalive).then_some("DisableNvmeKeepalive"))
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("restart_openhcl")
}

/// Windows event log as retrieved by `run_get_winevent`
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct WinEvent {
    /// Time of event
    pub time_created: Timestamp,
    /// Event provider name
    pub provider_name: String,
    /// Event level (see winmeta.h)
    pub level: u8,
    /// Event ID
    pub id: u32,
    /// Message content
    pub message: String,
}

/// Get event logs
pub async fn run_get_winevent(
    log_name: &[&str],
    start_time: Option<&Timestamp>,
    find: Option<&str>,
    ids: &[u32],
) -> anyhow::Result<Vec<WinEvent>> {
    let mut filter = Vec::new();
    if !log_name.is_empty() {
        filter.push(("LogName", ps::Value::new(ps::Array::new(log_name))));
    }
    if let Some(start_time) = start_time {
        filter.push(("StartTime", ps::Value::new(start_time)));
    }
    if !ids.is_empty() {
        filter.push(("Id", ps::Value::new(ps::Array::new(ids))));
    }
    let filter = ps::HashTable::new(filter);

    let output_var = ps::Variable::new("events");

    let mut builder = PowerShellBuilder::new()
        .cmdlet_to_var("Get-WinEvent", &output_var)
        .flag("Oldest")
        .arg("FilterHashtable", filter)
        .pipeline();

    if let Some(find) = find {
        builder = builder
            .cmdlet("where")
            .positional("message")
            .arg("Match", find)
            .pipeline();
    }

    let props = ps::Array::new([
        ps::Value::new(ps::HashTable::new([
            ("label", ps::Value::new("TimeCreated")),
            (
                "expression",
                ps::Value::new(ps::Script::new("Get-Date $_.TimeCreated -Format o")),
            ),
        ])),
        ps::Value::new("ProviderName"),
        ps::Value::new("Level"),
        ps::Value::new("Id"),
        ps::Value::new("Message"),
    ]);

    let output = run_host_cmd(
        builder
            .cmdlet("Select-Object")
            .positional(props)
            .next()
            .cmdlet("ConvertTo-Json")
            .arg("InputObject", ps::Array::new([&output_var]))
            .finish()
            .build(),
    )
    .await;

    match output {
        Ok(logs) => serde_json::from_str(&logs).context("parsing winevents"),
        Err(e) => match e {
            CommandError::Command(_, err_output)
                if err_output.contains(
                    "No events were found that match the specified selection criteria.",
                ) =>
            {
                Ok(Vec::new())
            }
            e => Err(e).context("get_winevent"),
        },
    }
}

const HYPERV_WORKER_TABLE: &str = "Microsoft-Windows-Hyper-V-Worker-Admin";
const HYPERV_VMMS_TABLE: &str = "Microsoft-Windows-Hyper-V-VMMS-Admin";

/// Get Hyper-V event logs for a VM
pub async fn hyperv_event_logs(
    vmid: Option<&Guid>,
    start_time: &Timestamp,
) -> anyhow::Result<Vec<WinEvent>> {
    let vmid = vmid.map(|id| id.to_string());
    run_get_winevent(
        &[HYPERV_WORKER_TABLE, HYPERV_VMMS_TABLE],
        Some(start_time),
        vmid.as_deref(),
        &[],
    )
    .await
}

/// The vm successfully booted an operating system.
pub const MSVM_BOOT_RESULTS_SUCCESS: u32 = 18601;
/// The vm successfully booted an operating system, but at least one boot source failed secure boot validation.
pub const MSVM_BOOT_RESULTS_SUCCESS_SECURE_BOOT_FAILURES: u32 = 18602;
/// The vm failed to boot an operating system.
pub const MSVM_BOOT_RESULTS_FAILURE: u32 = 18603;
/// The vm failed to boot an operating system. At least one boot source failed secure boot validation.
pub const MSVM_BOOT_RESULTS_FAILURE_SECURE_BOOT_FAILURES: u32 = 18604;
/// The vm failed to boot an operating system. No bootable devices are configured.
pub const MSVM_BOOT_RESULTS_FAILURE_NO_DEVICES: u32 = 18605;
/// The vm is attempting to boot an operating system. (PCAT only)
pub const MSVM_BOOT_RESULTS_ATTEMPT: u32 = 18606;

const BOOT_EVENT_IDS: [u32; 6] = [
    MSVM_BOOT_RESULTS_SUCCESS,
    MSVM_BOOT_RESULTS_SUCCESS_SECURE_BOOT_FAILURES,
    MSVM_BOOT_RESULTS_FAILURE,
    MSVM_BOOT_RESULTS_FAILURE_SECURE_BOOT_FAILURES,
    MSVM_BOOT_RESULTS_FAILURE_NO_DEVICES,
    MSVM_BOOT_RESULTS_ATTEMPT,
];

/// Get Hyper-V boot event logs for a VM
pub async fn hyperv_boot_events(
    vmid: &Guid,
    start_time: &Timestamp,
) -> anyhow::Result<Vec<WinEvent>> {
    let vmid = vmid.to_string();
    run_get_winevent(
        &[HYPERV_WORKER_TABLE],
        Some(start_time),
        Some(&vmid),
        &BOOT_EVENT_IDS,
    )
    .await
}

/// The vm was turned off.
pub const MSVM_HOST_STOP_SUCCESS: u32 = 18502;
/// The vm was shut down using the Shutdown Integration Component.
pub const MSVM_HOST_SHUTDOWN_SUCCESS: u32 = 18504;
/// The vm was shut down by the guest operating system.
pub const MSVM_GUEST_SHUTDOWN_SUCCESS: u32 = 18508;
/// The vm was shut down using the Shutdown Integration Component.
pub const MSVM_HOST_RESET_SUCCESS: u32 = 18512;
/// The vm was shut down by the guest operating system.
pub const MSVM_GUEST_RESET_SUCCESS: u32 = 18514;
/// The vm was shut down for a reset initiated by the guest operating system.
pub const MSVM_STOP_FOR_GUEST_RESET_SUCCESS: u32 = 18515;
/// The vm was turned off as it could not recover from a critical error.
pub const MSVM_STOP_CRITICAL_SUCCESS: u32 = 18528;
/// The vm was reset because the guest operating system requested an operation
/// that is not supported by Hyper-V or an unrecoverable error occurred.
/// This caused a triple fault.
pub const MSVM_TRIPLE_FAULT_GENERAL_ERROR: u32 = 18539;
/// The vm was reset because the guest operating system requested an operation
/// that is not supported by Hyper-V. This request caused a triple fault.
pub const MSVM_TRIPLE_FAULT_UNSUPPORTED_FEATURE_ERROR: u32 = 18540;
/// The vm was reset because an unrecoverable error occurred while accessing a
/// virtual processor register which caused a triple fault.
pub const MSVM_TRIPLE_FAULT_INVALID_VP_REGISTER_ERROR: u32 = 18550;
/// The vm was reset because an unrecoverable error occurred on a virtual
/// processor that caused a triple fault.
pub const MSVM_TRIPLE_FAULT_UNRECOVERABLE_EXCEPTION_ERROR: u32 = 18560;
/// The vm was hibernated successfully.
pub const MSVM_GUEST_HIBERNATE_SUCCESS: u32 = 18608;
/// The vm has quit unexpectedly (the worker process terminated).
pub const MSVM_VMMS_VM_TERMINATE_ERROR: u32 = 14070;

const HALT_EVENT_IDS: [u32; 13] = [
    MSVM_HOST_STOP_SUCCESS,
    MSVM_HOST_SHUTDOWN_SUCCESS,
    MSVM_GUEST_SHUTDOWN_SUCCESS,
    MSVM_HOST_RESET_SUCCESS,
    MSVM_GUEST_RESET_SUCCESS,
    MSVM_STOP_FOR_GUEST_RESET_SUCCESS,
    MSVM_STOP_CRITICAL_SUCCESS,
    MSVM_TRIPLE_FAULT_GENERAL_ERROR,
    MSVM_TRIPLE_FAULT_UNSUPPORTED_FEATURE_ERROR,
    MSVM_TRIPLE_FAULT_INVALID_VP_REGISTER_ERROR,
    MSVM_TRIPLE_FAULT_UNRECOVERABLE_EXCEPTION_ERROR,
    MSVM_GUEST_HIBERNATE_SUCCESS,
    MSVM_VMMS_VM_TERMINATE_ERROR,
];

/// Get Hyper-V halt event logs for a VM
pub async fn hyperv_halt_events(
    vmid: &Guid,
    start_time: &Timestamp,
) -> anyhow::Result<Vec<WinEvent>> {
    let vmid = vmid.to_string();
    run_get_winevent(
        &[HYPERV_WORKER_TABLE, HYPERV_VMMS_TABLE],
        Some(start_time),
        Some(&vmid),
        &HALT_EVENT_IDS,
    )
    .await
}

/// Get the IDs of the VM(s) with the specified name
pub async fn vm_id_from_name(name: &str) -> anyhow::Result<Vec<Guid>> {
    let output = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Name", name)
            .pipeline()
            .cmdlet("Select-Object")
            .arg("ExpandProperty", "Id")
            .pipeline()
            .cmdlet("Select-Object")
            .arg("ExpandProperty", "Guid")
            .finish()
            .build(),
    )
    .await
    .context("vm_id_from_name")?;
    let mut vmids = Vec::new();
    for s in output.lines() {
        vmids.push(Guid::from_str(s)?);
    }
    Ok(vmids)
}

/// Hyper-V VM Shutdown Integration Component Status
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VmShutdownIcStatus {
    /// The VM is off
    Off,
    /// The component is operating normally.
    Ok,
    /// The component is operating normally but the guest component negotiated
    /// a compatiable communications protocol version.
    Degraded,
    /// The guest does not support a compatible protocol version.
    NonRecoverableError,
    /// The guest component is not installed or has not yet been contacted.
    NoContact,
    /// The guest component is no longer responding normally.
    LostCommunication,
}

/// Get the VM's shutdown IC status
pub async fn vm_shutdown_ic_status(vmid: &Guid) -> anyhow::Result<VmShutdownIcStatus> {
    let status = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Get-VMIntegrationService")
            .arg("Name", "Shutdown")
            .pipeline()
            .cmdlet("Select-Object")
            .arg("ExpandProperty", "PrimaryStatusDescription")
            .finish()
            .build(),
    )
    .await
    .context("vm_shutdown_ic_status")?;

    Ok(match status.as_str() {
        "" => VmShutdownIcStatus::Off,
        "OK" => VmShutdownIcStatus::Ok,
        "Degraded" => VmShutdownIcStatus::Degraded,
        "Non-Recoverable Error" => VmShutdownIcStatus::NonRecoverableError,
        "No Contact" => VmShutdownIcStatus::NoContact,
        "Lost Communication" => VmShutdownIcStatus::LostCommunication,
        s => anyhow::bail!("Unknown VM shutdown status: {s}"),
    })
}

/// Runs Remove-VmNetworkAdapter to remove all network adapters from a VM.
pub async fn run_remove_vm_network_adapter(vmid: &Guid) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Remove-VMNetworkAdapter")
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("remove_vm_network_adapters")
}

/// Runs Remove-VMScsiController with the given arguments.
pub async fn run_remove_vm_scsi_controller(
    vmid: &Guid,
    controller_number: u32,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Get-VMScsiController")
            .arg("ControllerNumber", controller_number)
            .pipeline()
            .cmdlet("Remove-VMScsiController")
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("remove_vm_scsi_controller")
}

/// Run Get-VmScreenshot commandlet
pub async fn run_get_vm_screenshot(
    vmid: &Guid,
    image: &mut Vec<u8>,
    ps_mod: &Path,
    temp_bin_path: &Path,
) -> anyhow::Result<VmScreenshotMeta> {
    // execute wmi via powershell
    let output = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Get-VmScreenshot")
            .arg("Path", temp_bin_path)
            .finish()
            .build(),
    )
    .await
    .context("get_vm_screenshot")?;

    // parse output
    let (x, y) = output.split_once(',').context("invalid dimensions")?;
    let x = x.parse().context("invalid x dimension")?;
    let y = y.parse().context("invalid y dimension")?;
    let (widthsize, heightsize) = (x as usize, y as usize);
    let mut image_rgb565 = fs_err::read(temp_bin_path)?;

    // calculate length and truncate
    const IN_BYTES_PER_PIXEL: usize = 2;
    const OUT_BYTES_PER_PIXEL: usize = 3;
    let in_len = widthsize * heightsize * IN_BYTES_PER_PIXEL;
    let out_len = widthsize * heightsize * OUT_BYTES_PER_PIXEL;
    image_rgb565.truncate(in_len);
    if image_rgb565.len() != in_len {
        anyhow::bail!("did not get enough bytes for screenshot");
    }

    // convert from rgb565 to rgb888
    image.resize(out_len, 0);
    for (out_pixel, in_pixel) in image
        .chunks_exact_mut(OUT_BYTES_PER_PIXEL)
        .zip(image_rgb565.chunks_exact(IN_BYTES_PER_PIXEL))
    {
        // convert from rgb565 ( gggbbbbb rrrrrggg )
        // to rgb888 ( rrrrrrrr gggggggg bbbbbbbb )

        // red
        out_pixel[0] = in_pixel[1] & 0b11111000;
        // green
        out_pixel[1] = ((in_pixel[1] & 0b00000111) << 5) + ((in_pixel[0] & 0b11100000) >> 3);
        // blue
        out_pixel[2] = in_pixel[0] << 3;
    }

    Ok(VmScreenshotMeta {
        color: image::ExtendedColorType::Rgb8,
        width: x,
        height: y,
    })
}

/// Run Set-TurnOffOnGuestRestart commandlet
pub async fn run_set_turn_off_on_guest_restart(
    vmid: &Guid,
    ps_mod: &Path,
    enable: bool,
) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-TurnOffOnGuestRestart")
            .arg("Enable", enable)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_turn_off_on_guest_restart")
}

/// Hyper-V Get VM Host Output
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HyperVGetVmHost {
    /// GuestIsolationTypes supported on the host. While GuestStateIsolationTypes contains values
    /// for SNP and TDX, there are other factors that determine SNP/TDX support than just hardware
    /// compatibility, hence we rely on SnpStatus and TdxStatus for that information.
    #[serde(rename = "GuestIsolationTypes")]
    pub guest_isolation_types: Vec<HyperVGuestStateIsolationType>,
    /// Whether SNP is supported on the host.
    #[serde(rename = "SnpStatus", deserialize_with = "int_to_bool")]
    pub snp_status: bool,
    /// Whether TDX is supported on the host.
    #[serde(rename = "TdxStatus", deserialize_with = "int_to_bool")]
    pub tdx_status: bool,
}

fn int_to_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = i32::deserialize(deserializer)?;
    Ok(v == 1)
}

/// Gets the VM host information and returns the output string
pub async fn run_get_vm_host() -> anyhow::Result<HyperVGetVmHost> {
    let output = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VMHost")
            .pipeline()
            .cmdlet("ConvertTo-Json")
            .arg("Depth", 3)
            .flag("Compress")
            .finish()
            .build(),
    )
    .await
    .context("get_vm_host")?;

    serde_json::from_str::<HyperVGetVmHost>(&output)
        .map_err(|e| anyhow::anyhow!("failed to parse HyperVGetVmHost: {}", e))
}

/// Runs Get-GuestStateFile with the given arguments.
pub async fn run_get_guest_state_file(vmid: &Guid, ps_mod: &Path) -> anyhow::Result<PathBuf> {
    let output = run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Get-GuestStateFile")
            .finish()
            .build(),
    )
    .await
    .context("get_guest_state_file")?;

    Ok(PathBuf::from(output))
}

/// Sets the VTL2 settings (in the `Base` namespace) for a VM.
///
/// This should include the fixed VTL2 settings, as well as any storage
/// settings.
///
/// TODO FUTURE: Detect if the settings should be in `json` or `protobuf` format
/// based on what is already there (or let the caller specify explicitly so that
/// we can test the handling of both deserializers).
pub async fn run_set_base_vtl2_settings(
    vmid: &Guid,
    ps_mod: &Path,
    vtl2_settings: &vtl2_settings_proto::Vtl2Settings,
) -> anyhow::Result<()> {
    // Pass the settings via a file to avoid challenges escaping the string across
    // the command line.
    let mut tempfile = tempfile::NamedTempFile::new().context("creating tempfile")?;
    tempfile
        .write_all(serde_json::to_string(vtl2_settings)?.as_bytes())
        .context("writing settings to tempfile")?;

    tracing::trace!(?tempfile, ?vtl2_settings, ?vmid, "set base vtl2 settings");

    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Set-Vtl2Settings")
            .arg("VmId", vmid)
            .arg("SettingsFile", tempfile.path())
            .arg("Namespace", "Base")
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_base_vtl2_settings")
}

/// Guest state isolation modes for Hyper-V VMs.
#[derive(Debug)]
pub enum HyperVGuestStateIsolationMode {
    /// Default isolation mode.
    Default = 0,
    /// No persistent secrets isolation mode.
    NoPersistentSecrets = 1,
    /// No management VTL isolation mode.
    NoManagementVtl = 2,
}

impl ps::AsVal for HyperVGuestStateIsolationMode {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        match self {
            HyperVGuestStateIsolationMode::Default => "0",
            HyperVGuestStateIsolationMode::NoPersistentSecrets => "1",
            HyperVGuestStateIsolationMode::NoManagementVtl => "2",
        }
    }
}

/// Sets the guest state isolation mode for a VM.
pub async fn run_set_guest_state_isolation_mode(
    vmid: &Guid,
    ps_mod: &Path,
    mode: HyperVGuestStateIsolationMode,
) -> anyhow::Result<()> {
    tracing::trace!(?mode, ?vmid, "set guest state isolation mode");

    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Import-Module")
            .positional(ps_mod)
            .next()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Set-GuestStateIsolationMode")
            .arg("Mode", mode)
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("set_guest_state_isolation_mode")
}

/// Runs Enable-VMTPM
pub async fn run_enable_vmtpm(vmid: &Guid) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Enable-VMTPM")
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("run_enable_vmtpm")
}

/// Runs Disable-VMTPM
pub async fn run_disable_vmtpm(vmid: &Guid) -> anyhow::Result<()> {
    run_host_cmd(
        PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .arg("Id", vmid)
            .pipeline()
            .cmdlet("Disable-VMTPM")
            .finish()
            .build(),
    )
    .await
    .map(|_| ())
    .context("run_disable_vmtpm")
}
