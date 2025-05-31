// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrappers for Hyper-V Powershell Cmdlets

use self::ps::PowerShellBuilder;
use super::CommandError;
use anyhow::Context;
use core::str;
use guid::Guid;
use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsStr;
use std::path::Path;
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
#[derive(Clone, Copy)]
pub enum HyperVGuestStateIsolationType {
    /// Trusted Launch (OpenHCL, SecureBoot, TPM)
    TrustedLaunch,
    /// VBS
    Vbs,
    /// SNP
    Snp,
    /// TDX
    Tdx,
    /// OpenHCL but no isolation
    OpenHCL,
    /// No HCL and no isolation
    Disabled,
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
    /// Secure Boot Disabled
    SecureBootDisabled,
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
            HyperVSecureBootTemplate::SecureBootDisabled => "SecureBootDisabled",
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
}

/// Runs New-VM with the given arguments.
pub fn run_new_vm(args: HyperVNewVMArgs<'_>) -> anyhow::Result<Guid> {
    let vmid = PowerShellBuilder::new()
        .cmdlet("New-VM")
        .arg("Name", args.name)
        .arg_opt("Generation", args.generation)
        .arg_opt("GuestStateIsolationType", args.guest_state_isolation_type)
        .arg_opt("MemoryStartupBytes", args.memory_startup_bytes)
        .arg_opt("Path", args.path)
        .arg_opt("VHDPath", args.vhd_path)
        .flag("Force")
        .pipeline()
        .cmdlet("Select-Object")
        .arg("ExpandProperty", "Id")
        .pipeline()
        .cmdlet("Select-Object")
        .arg("ExpandProperty", "Guid")
        .finish()
        .output(true)
        .context("new_vm")?;

    Guid::from_str(&vmid).context("invalid vmid")
}

/// Runs New-VM with the given arguments.
pub fn run_remove_vm(vmid: &Guid) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Remove-VM")
        .flag("Force")
        .finish()
        .output(true)
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
pub fn run_set_vm_processor(vmid: &Guid, args: &HyperVSetVMProcessorArgs) -> anyhow::Result<()> {
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
        .output(true)
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
pub fn run_set_vm_memory(vmid: &Guid, args: &HyperVSetVMMemoryArgs) -> anyhow::Result<()> {
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
        .output(true)
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
    pub controller_location: Option<u32>,
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

/// Runs Add-VMHardDiskDrive with the given arguments.
pub fn run_add_vm_hard_disk_drive(args: HyperVAddVMHardDiskDriveArgs<'_>) -> anyhow::Result<()> {
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
        .output(true)
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
pub fn run_add_vm_dvd_drive(args: HyperVAddVMDvdDriveArgs<'_>) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", args.vmid)
        .pipeline()
        .cmdlet("Add-VMDvdDrive")
        .arg_opt("ControllerLocation", args.controller_location)
        .arg_opt("ControllerNumber", args.controller_number)
        .arg_opt("Path", args.path)
        .finish()
        .output(true)
        .map(|_| ())
        .context("add_vm_dvd_drive")
}

/// Runs Add-VMScsiController with the given arguments.
///
/// Returns the controller number.
pub fn run_add_vm_scsi_controller(vmid: &Guid) -> anyhow::Result<u32> {
    let output = PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Add-VMScsiController")
        .flag("Passthru")
        .pipeline()
        .cmdlet("Select-Object")
        .arg("ExpandProperty", "ControllerNumber")
        .finish()
        .output(true)
        .context("add_vm_scsi_controller")?;
    Ok(output.trim().parse::<u32>()?)
}

/// Sets the target VTL for a SCSI controller.
pub fn run_set_vm_scsi_controller_target_vtl(
    ps_mod: &Path,
    vmid: &Guid,
    controller_number: u32,
    target_vtl: u32,
) -> anyhow::Result<()> {
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
        .output(true)
        .map(|_| ())
        .context("set_vm_scsi_controller_target_vtl")
}

/// Create a new differencing VHD with the provided parent.
pub fn create_child_vhd(path: &Path, parent_path: &Path) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("New-VHD")
        .arg("Path", path)
        .arg("ParentPath", parent_path)
        .flag("Differencing")
        .finish()
        .output(true)
        .map(|_| ())
        .context("create_child_vhd")
}

/// Runs Dismount-VHD with the given arguments.
pub fn run_dismount_vhd(path: &Path) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Dismount-VHD")
        .arg("Path", path)
        .finish()
        .output(true)
        .map(|_| ())
        .context("dismount_vhd")
}

/// Arguments for the Set-VMFirmware powershell cmdlet
pub struct HyperVSetVMFirmwareArgs<'a> {
    /// Specifies the ID of virtual machines for which you want to modify the
    /// firmware configuration.
    pub vmid: &'a Guid,
    /// Specifies the name of the secure boot template. If secure boot is
    /// enabled, you must have a valid secure boot template for the guest
    /// operating system to start.
    pub secure_boot_template: Option<HyperVSecureBootTemplate>,
}

/// Runs Set-VMFirmware with the given arguments.
pub fn run_set_vm_firmware(args: HyperVSetVMFirmwareArgs<'_>) -> anyhow::Result<()> {
    let mut builder = PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", args.vmid)
        .pipeline();

    builder = match args.secure_boot_template {
        Some(HyperVSecureBootTemplate::SecureBootDisabled) | None => builder
            .cmdlet("Set-VMFirmware")
            .arg("EnableSecureBoot", ps::RawVal::new("Off"))
            .finish(),
        Some(template) => builder
            .cmdlet("Set-VMFirmware")
            .arg("EnableSecureBoot", ps::RawVal::new("On"))
            .arg("SecureBootTemplate", template)
            .finish(),
    };

    builder.output(true).map(|_| ()).context("set_vm_firmware")
}

/// Runs Set-VMFirmware with the given arguments.
pub fn run_set_openhcl_firmware(
    vmid: &Guid,
    ps_mod: &Path,
    igvm_file: &Path,
    increase_vtl2_memory: bool,
) -> anyhow::Result<()> {
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
        .output(true)
        .map(|_| ())
        .context("set_openhcl_firmware")
}

/// Runs Set-VmCommandLine with the given arguments.
pub fn run_set_vm_command_line(
    vmid: &Guid,
    ps_mod: &Path,
    command_line: &str,
) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Import-Module")
        .positional(ps_mod)
        .next()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Set-VmCommandLine")
        .arg("CommandLine", command_line)
        .finish()
        .output(true)
        .map(|_| ())
        .context("set_vm_command_line")
}

/// Sets the initial machine configuration for a VM
pub fn run_set_initial_machine_configuration(
    vmid: &Guid,
    ps_mod: &Path,
    imc_hive: &Path,
) -> anyhow::Result<()> {
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
        .output(true)
        .map(|_| ())
        .context("set_initial_machine_configuration")
}

/// Enables the specified vm com port and binds it to the named pipe path
pub fn run_set_vm_com_port(vmid: &Guid, port: u8, path: &Path) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Set-VMComPort")
        .arg("Number", port)
        .arg("Path", path)
        .finish()
        .output(true)
        .map(|_| ())
        .context("set_vm_com_port")
}

/// Run Enable-VmBusRelay commandlet
pub fn run_enable_vmbus_relay(vmid: &Guid) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Import-Module")
        .positional(ps_mod)
        .next()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Enable-VmBusRelay")
        .finish()
        .output(true)
        .map(|_| ())
        .context("enable_vmbus_relay")
}

/// Run Disable-VmBusRelay commandlet
pub fn run_disable_vmbus_relay(vmid: &Guid) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Import-Module")
        .positional(ps_mod)
        .next()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Disable-VmBusRelay")
        .finish()
        .output(true)
        .map(|_| ())
        .context("disable_vmbus_relay")
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
pub fn run_get_winevent(
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

    let output = builder
        .cmdlet("Select-Object")
        .positional(props)
        .next()
        .cmdlet("ConvertTo-Json")
        .arg("InputObject", ps::Array::new([&output_var]))
        .finish()
        .output(false);

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
pub fn hyperv_event_logs(vmid: &Guid, start_time: &Timestamp) -> anyhow::Result<Vec<WinEvent>> {
    let vmid = vmid.to_string();
    run_get_winevent(
        &[HYPERV_WORKER_TABLE, HYPERV_VMMS_TABLE],
        Some(start_time),
        Some(&vmid),
        &[],
    )
}

/// boot succeeded
pub const EVENT_ID_BOOT_SUCCESS: u32 = 18601;
/// boot succeeded, secure boot failed
pub const EVENT_ID_BOOT_SUCCESS_SECURE_BOOT_FAILED: u32 = 18602;
/// boot failed
pub const EVENT_ID_BOOT_FAILURE: u32 = 18603;
/// boot failed due to secure boot failure
pub const EVENT_ID_BOOT_FAILURE_SECURE_BOOT_FAILED: u32 = 18604;
/// boot failed because there was no boot device
pub const EVENT_ID_NO_BOOT_DEVICE: u32 = 18605;
/// boot attempted (pcat only)
pub const EVENT_ID_BOOT_ATTEMPT: u32 = 18606;

const BOOT_EVENT_IDS: [u32; 6] = [
    EVENT_ID_BOOT_SUCCESS,
    EVENT_ID_BOOT_SUCCESS_SECURE_BOOT_FAILED,
    EVENT_ID_BOOT_FAILURE,
    EVENT_ID_BOOT_FAILURE_SECURE_BOOT_FAILED,
    EVENT_ID_NO_BOOT_DEVICE,
    EVENT_ID_BOOT_ATTEMPT,
];

/// Get Hyper-V event logs for a VM
pub fn hyperv_boot_events(vmid: &Guid, start_time: &Timestamp) -> anyhow::Result<Vec<WinEvent>> {
    let vmid = vmid.to_string();
    run_get_winevent(
        &[HYPERV_WORKER_TABLE],
        Some(start_time),
        Some(&vmid),
        &BOOT_EVENT_IDS,
    )
}

/// Get the IDs of the VM(s) with the specified name
pub fn vm_id_from_name(name: &str) -> anyhow::Result<Vec<Guid>> {
    let output = PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Name", name)
        .pipeline()
        .cmdlet("Select-Object")
        .arg("ExpandProperty", "Id")
        .pipeline()
        .cmdlet("Select-Object")
        .arg("ExpandProperty", "Guid")
        .finish()
        .output(true)
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
pub fn vm_shutdown_ic_status(vmid: &Guid) -> anyhow::Result<VmShutdownIcStatus> {
    let status = PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Get-VMIntegrationService")
        .arg("Name", "Shutdown")
        .pipeline()
        .cmdlet("Select-Object")
        .arg("ExpandProperty", "PrimaryStatusDescription")
        .finish()
        .output(true)
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
pub fn run_remove_vm_network_adapter(vmid: &Guid) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Remove-VMNetworkAdapter")
        .finish()
        .output(true)
        .map(|_| ())
        .context("remove_vm_network_adapters")
}

/// Runs Remove-VMScsiController with the given arguments.
pub fn run_remove_vm_scsi_controller(vmid: &Guid, controller_number: u32) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg("Id", vmid)
        .pipeline()
        .cmdlet("Get-VMScsiController")
        .arg("ControllerNumber", controller_number)
        .pipeline()
        .cmdlet("Remove-VMScsiController")
        .finish()
        .output(true)
        .map(|_| ())
        .context("remove_vm_scsi_controller")
}

mod ps {
    use crate::hyperv::CommandError;
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Stdio;

    /// A PowerShell script builder
    pub struct PowerShellBuilder(Command);

    impl PowerShellBuilder {
        /// Create a new PowerShell command
        pub fn new() -> Self {
            PowerShellCmdletBuilder(Command::new("powershell.exe"))
                .flag("NoProfile")
                .finish()
        }

        /// Start a new Cmdlet
        pub fn cmdlet<S: AsRef<str>>(self, cmdlet: S) -> PowerShellCmdletBuilder {
            PowerShellCmdletBuilder(self.0).positional(RawVal::new(cmdlet.as_ref()))
        }

        /// Assign the output of the cmdlet to a variable
        pub fn cmdlet_to_var<S: AsRef<str>>(
            self,
            cmdlet: S,
            varname: &Variable,
        ) -> PowerShellCmdletBuilder {
            PowerShellCmdletBuilder(self.0)
                .positional(varname)
                .positional(RawVal::new("="))
                .finish()
                .cmdlet(cmdlet)
        }

        /// Run the PowerShell script and return the output
        pub fn output(mut self, log_stdout: bool) -> Result<String, CommandError> {
            self.0.stderr(Stdio::piped()).stdin(Stdio::null());

            let ps_cmd = self.cmd();
            tracing::debug!(ps_cmd, "executing powershell command");

            let start = jiff::Timestamp::now();
            let output = self.0.output()?;
            let time_elapsed = jiff::Timestamp::now() - start;

            let ps_stdout = (log_stdout || !output.status.success())
                .then(|| String::from_utf8_lossy(&output.stdout).to_string());
            let ps_stderr = String::from_utf8_lossy(&output.stderr).to_string();
            tracing::debug!(
                ps_cmd,
                ps_stdout,
                ps_stderr,
                "powershell command exited in {:.3}s with status {}",
                time_elapsed.total(jiff::Unit::Second).unwrap_or(-1.0),
                output.status
            );

            if !output.status.success() {
                return Err(CommandError::Command(output.status, ps_stderr));
            }

            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        }

        /// Get the command to be run
        pub fn cmd(&self) -> String {
            format!(
                "{} {}",
                self.0.get_program().to_string_lossy(),
                self.0
                    .get_args()
                    .collect::<Vec<_>>()
                    .join(OsStr::new(" "))
                    .to_string_lossy()
            )
        }
    }

    /// A PowerShell Cmdlet builder
    pub struct PowerShellCmdletBuilder(Command);

    impl PowerShellCmdletBuilder {
        /// Add a flag to the cmdlet
        pub fn flag<S: AsRef<OsStr>>(mut self, flag: S) -> Self {
            let mut arg = OsString::from("-");
            arg.push(flag);
            self.0.arg(arg);
            self
        }

        /// Optionally add a flag to the cmdlet
        pub fn flag_opt<S: AsRef<OsStr>>(self, flag: Option<S>) -> Self {
            if let Some(flag) = flag {
                self.flag(flag)
            } else {
                self
            }
        }

        /// Add a positional argument to the cmdlet
        pub fn positional<S: AsVal>(mut self, positional: S) -> Self {
            self.0.arg(positional.as_val());
            self
        }

        /// Optionally add a positional argument to the cmdlet
        #[expect(dead_code)]
        pub fn positional_opt<S: AsVal>(self, positional: Option<S>) -> Self {
            if let Some(positional) = positional {
                self.positional(positional)
            } else {
                self
            }
        }

        /// Add a named argument to the cmdlet
        pub fn arg<S: AsRef<OsStr>, T: AsVal>(self, name: S, value: T) -> Self {
            self.flag(name).positional(value)
        }

        /// Optionally add a named argument to the cmdlet
        pub fn arg_opt<S: AsRef<OsStr>, T: AsVal>(self, name: S, value: Option<T>) -> Self {
            if let Some(value) = value {
                self.arg(name, value)
            } else {
                self
            }
        }

        /// Finish the cmdlet
        pub fn finish(self) -> PowerShellBuilder {
            PowerShellBuilder(self.0)
        }

        /// Finish the cmdlet with a pipeline operator
        pub fn pipeline(mut self) -> PowerShellBuilder {
            self.0.arg("|");
            self.finish()
        }

        /// Finish the cmdlet with a semicolon
        pub fn next(mut self) -> PowerShellBuilder {
            self.0.arg(";");
            self.finish()
        }
    }

    pub struct Value(OsString);

    impl Value {
        pub fn new(val: impl AsVal) -> Self {
            Self(val.as_val().as_ref().to_owned())
        }
    }

    impl AsVal for Value {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            &self.0
        }
    }

    pub trait AsVal {
        fn as_val(&self) -> impl '_ + AsRef<OsStr>;
    }

    impl<T: AsVal + ?Sized> AsVal for &T {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            (*self).as_val()
        }
    }

    fn quote_str(s: &OsStr) -> OsString {
        let mut quoted = OsString::new();
        quoted.push("\"");
        // TODO: escape this properly.
        quoted.push(s);
        quoted.push("\"");
        quoted
    }

    macro_rules! str {
        ($($ty:ty),* $(,)?) => {
            $(
                impl AsVal for $ty {
                    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
                        quote_str(self.as_ref())
                    }
                }
            )*
        }
    }

    str!(&str, String, Path, PathBuf);

    macro_rules! disp_str {
        ($($ty:ty),* $(,)?) => {
            $(
                impl AsVal for $ty {
                    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
                        quote_str(self.to_string().as_ref())
                    }
                }
            )*
        }
    }

    disp_str!(jiff::Timestamp, guid::Guid);

    impl AsVal for bool {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            if *self { "$true" } else { "$false" }
        }
    }

    macro_rules! disp {
        ($($ty:ty),* $(,)?) => {
            $(
                impl AsVal for $ty {
                    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
                        self.to_string()
                    }
                }
            )*
        }
    }

    disp!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

    pub struct RawVal<T>(T);

    impl<T: AsRef<OsStr>> RawVal<T> {
        pub fn new(arg: T) -> Self {
            Self(arg)
        }
    }

    impl<T: AsRef<OsStr>> AsVal for RawVal<T> {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            &self.0
        }
    }

    pub struct Variable(String);

    impl Variable {
        pub fn new(name: impl AsRef<str>) -> Self {
            Self(format!("${}", name.as_ref()))
        }
    }

    impl AsVal for Variable {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            &self.0
        }
    }

    pub struct Array(OsString);

    impl Array {
        pub fn new<T: AsVal>(v: impl IntoIterator<Item = T>) -> Self {
            let mut args = OsString::new();
            args.push("@(");
            let mut first = true;
            for arg in v {
                if !first {
                    args.push("; ");
                }
                args.push(arg.as_val());
                first = false;
            }
            args.push(")");
            Self(args)
        }
    }

    impl AsVal for Array {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            &self.0
        }
    }

    pub struct HashTable<K, V>(Vec<(K, V)>);

    impl<K: AsRef<str>, V: AsVal> HashTable<K, V> {
        pub fn new(v: impl IntoIterator<Item = (K, V)>) -> Self {
            Self(v.into_iter().collect())
        }
    }

    impl<K: AsRef<str>, V: AsVal> AsVal for HashTable<K, V> {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            let mut args = OsString::new();
            args.push("@{");
            let mut first = true;
            for (k, v) in &self.0 {
                if !first {
                    args.push("; ");
                }
                args.push(k.as_ref());
                args.push("=");
                args.push(v.as_val());
                first = false;
            }
            args.push("}");
            args
        }
    }

    pub struct Script(String);

    impl Script {
        pub fn new(script: impl AsRef<str>) -> Self {
            Self(format!("{{ {} }}", script.as_ref()))
        }
    }

    impl AsVal for Script {
        fn as_val(&self) -> impl '_ + AsRef<OsStr> {
            &self.0
        }
    }
}
