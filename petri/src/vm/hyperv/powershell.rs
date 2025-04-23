// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrappers for Hyper-V Powershell Cmdlets

use super::CommandError;
use anyhow::Context;
use core::str;
use guid::Guid;
use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::str::FromStr;

/// Hyper-V VM Generation
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HyperVGeneration {
    /// Generation 1 (with emulated legacy devices and PCAT BIOS)
    One,
    /// Generation 2 (synthetic devices and UEFI)
    Two,
}

impl AsRef<OsStr> for HyperVGeneration {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(match self {
            HyperVGeneration::One => "1",
            HyperVGeneration::Two => "2",
        })
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

impl AsRef<OsStr> for HyperVGuestStateIsolationType {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(match self {
            HyperVGuestStateIsolationType::TrustedLaunch => "TrustedLaunch",
            HyperVGuestStateIsolationType::Vbs => "VBS",
            HyperVGuestStateIsolationType::Snp => "SNP",
            HyperVGuestStateIsolationType::Tdx => "TDX",
            HyperVGuestStateIsolationType::OpenHCL => "OpenHCL",
            HyperVGuestStateIsolationType::Disabled => "Disabled",
        })
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

impl AsRef<OsStr> for HyperVSecureBootTemplate {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(match self {
            HyperVSecureBootTemplate::SecureBootDisabled => "SecureBootDisabled",
            HyperVSecureBootTemplate::MicrosoftWindows => "MicrosoftWindows",
            HyperVSecureBootTemplate::MicrosoftUEFICertificateAuthority => {
                "MicrosoftUEFICertificateAuthority"
            }
            HyperVSecureBootTemplate::OpenSourceShieldedVM => "OpenSourceShieldedVM",
        })
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
        .arg_opt_string("MemoryStartupBytes", args.memory_startup_bytes)
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
        .arg_string("Id", vmid)
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

impl AsRef<OsStr> for HyperVApicMode {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(match self {
            HyperVApicMode::Default => "Default",
            HyperVApicMode::Legacy => "Legacy",
            HyperVApicMode::X2Apic => "x2Apic",
        })
    }
}

/// Runs Set-VMProcessor with the given arguments.
pub fn run_set_vm_processor(vmid: &Guid, args: &HyperVSetVMProcessorArgs) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg_string("Id", vmid)
        .pipeline()
        .cmdlet("Set-VMProcessor")
        .arg_opt_string("Count", args.count)
        .arg_opt("ApicMode", args.apic_mode)
        .arg_opt_string("HwThreadCountPerCore", args.hw_thread_count_per_core)
        .arg_opt_string("MaximumCountPerNumaNode", args.maximum_count_per_numa_node)
        .finish()
        .output(true)
        .map(|_| ())
        .context("set_vm_processor")
}

/// Arguments for the Add-VMHardDiskDrive powershell cmdlet
pub struct HyperVAddVMHardDiskDriveArgs<'a> {
    /// Specifies the ID of the virtual machine to which the hard disk
    /// drive is to be added.
    pub vmid: &'a Guid,
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

/// Runs Add-VMHardDiskDrive with the given arguments.
pub fn run_add_vm_hard_disk_drive(args: HyperVAddVMHardDiskDriveArgs<'_>) -> anyhow::Result<()> {
    PowerShellBuilder::new()
        .cmdlet("Get-VM")
        .arg_string("Id", args.vmid)
        .pipeline()
        .cmdlet("Add-VMHardDiskDrive")
        .arg_opt_string("ControllerLocation", args.controller_location)
        .arg_opt_string("ControllerNumber", args.controller_number)
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
        .arg_string("Id", args.vmid)
        .pipeline()
        .cmdlet("Add-VMDvdDrive")
        .arg_opt_string("ControllerLocation", args.controller_location)
        .arg_opt_string("ControllerNumber", args.controller_number)
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
        .arg_string("Id", vmid)
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
        .arg_string("Id", vmid)
        .pipeline()
        .cmdlet("Set-VMScsiControllerTargetVtl")
        .arg_string("ControllerNumber", controller_number)
        .arg_string("TargetVtl", target_vtl)
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
        .arg_string("Id", args.vmid)
        .pipeline();

    builder = match args.secure_boot_template {
        Some(HyperVSecureBootTemplate::SecureBootDisabled) | None => builder
            .cmdlet("Set-VMFirmware")
            .arg("EnableSecureBoot", "Off")
            .finish(),
        Some(template) => builder
            .cmdlet("Set-VMFirmware")
            .arg("EnableSecureBoot", "On")
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
        .arg_string("Id", vmid)
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
    // TODO: make this generic, add escaping.
    let command_line = format!(r#""{command_line}""#);
    PowerShellBuilder::new()
        .cmdlet("Import-Module")
        .positional(ps_mod)
        .next()
        .cmdlet("Get-VM")
        .arg_string("Id", vmid)
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
        .arg_string("Id", vmid)
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
        .arg_string("Id", vmid)
        .pipeline()
        .cmdlet("Set-VMComPort")
        .arg_string("Number", port)
        .arg("Path", path)
        .finish()
        .output(true)
        .map(|_| ())
        .context("set_vm_com_port")
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
        filter.push(format!(
            "LogName={}",
            log_name
                .iter()
                .map(|x| format!("'{x}'"))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(start_time) = start_time {
        filter.push(format!("StartTime=\"{start_time}\""));
    }
    if !ids.is_empty() {
        filter.push(format!(
            "Id={}",
            ids.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    let filter = filter.join("; ");

    const OUTPUT_VARNAME: &str = "events";

    let mut builder = PowerShellBuilder::new()
        .cmdlet_to_var("Get-WinEvent", OUTPUT_VARNAME)
        .flag("Oldest")
        .arg("FilterHashtable", format!("@{{ {filter} }}"))
        .pipeline();

    if let Some(find) = find {
        builder = builder
            .cmdlet("where")
            .positional("message")
            .arg("Match", find)
            .pipeline();
    }

    let output = builder.cmdlet("Select-Object")
        .positional(r#"@{label="TimeCreated";expression={Get-Date $_.TimeCreated -Format o}}, ProviderName, Level, Id, Message"#)
        .next()
        .cmdlet("ConvertTo-Json")
        .arg_var("InputObject", OUTPUT_VARNAME, true)
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
        .arg_string("Name", name)
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
        .arg_string("Id", vmid)
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
        .arg_string("Id", vmid)
        .pipeline()
        .cmdlet("Remove-VMNetworkAdapter")
        .finish()
        .output(true)
        .map(|_| ())
        .context("remove_vm_network_adapters")
}

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
    pub fn cmdlet<S: AsRef<OsStr>>(self, cmdlet: S) -> PowerShellCmdletBuilder {
        PowerShellCmdletBuilder(self.0).positional(cmdlet)
    }

    /// Assign the output of the cmdlet to a variable
    pub fn cmdlet_to_var<S: AsRef<OsStr>, T: AsRef<OsStr>>(
        self,
        cmdlet: S,
        varname: T,
    ) -> PowerShellCmdletBuilder {
        PowerShellCmdletBuilder(self.0)
            .positional_var(varname, false)
            .positional("=")
            .finish()
            .cmdlet(cmdlet)
    }

    /// Run the PowerShell script and return the output
    pub fn output(mut self, log_stdout: bool) -> Result<String, CommandError> {
        self.0.stderr(Stdio::piped()).stdin(Stdio::null());

        let ps_cmd = self.cmd();
        tracing::debug!(ps_cmd, "executing powershell command");

        let start = Timestamp::now();
        let output = self.0.output()?;
        let time_elapsed = Timestamp::now() - start;

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
    pub fn positional<S: AsRef<OsStr>>(mut self, positional: S) -> Self {
        self.0.arg(positional);
        self
    }

    /// Add a positional argument to the cmdlet
    pub fn positional_string<S: ToString>(self, positional: S) -> Self {
        self.positional(positional.to_string())
    }

    /// Optionally add a positional argument to the cmdlet
    pub fn positional_opt<S: AsRef<OsStr>>(self, positional: Option<S>) -> Self {
        if let Some(positional) = positional {
            self.positional(positional)
        } else {
            self
        }
    }

    /// Optionally add a positional argument to the cmdlet
    pub fn positional_opt_string<S: ToString>(self, positional: Option<S>) -> Self {
        self.positional_opt(positional.map(|x| x.to_string()))
    }

    /// Add a PowerShell variable as a positional argument to the cmdlet
    pub fn positional_var<S: AsRef<OsStr>>(self, varname: S, as_array: bool) -> Self {
        let mut ps_var = OsString::new();
        if as_array {
            ps_var.push("@(");
        }
        ps_var.push("$");
        ps_var.push(varname);
        if as_array {
            ps_var.push(")");
        }
        self.positional(ps_var)
    }

    /// Add a named argument to the cmdlet
    pub fn arg<S: AsRef<OsStr>, T: AsRef<OsStr>>(self, name: S, value: T) -> Self {
        self.flag(name).positional(value)
    }

    /// Add a named argument to the cmdlet
    pub fn arg_string<S: AsRef<OsStr>, T: ToString>(self, name: S, value: T) -> Self {
        self.arg(name, value.to_string())
    }

    /// Optionally add a named argument to the cmdlet
    pub fn arg_opt<S: AsRef<OsStr>, T: AsRef<OsStr>>(self, name: S, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.arg(name, value)
        } else {
            self
        }
    }

    /// Optionally add a named argument to the cmdlet
    pub fn arg_opt_string<S: AsRef<OsStr>, T: ToString>(self, name: S, value: Option<T>) -> Self {
        self.arg_opt(name, value.map(|x| x.to_string()))
    }

    /// Add a PowerShell variable as a named argument to the cmdlet
    pub fn arg_var<S: AsRef<OsStr>, T: AsRef<OsStr>>(
        self,
        name: S,
        varname: T,
        as_array: bool,
    ) -> Self {
        self.flag(name).positional_var(varname, as_array)
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
