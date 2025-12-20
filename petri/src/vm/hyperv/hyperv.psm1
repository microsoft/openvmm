# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

#
# Constants
#

$ROOT_HYPER_V_NAMESPACE = "root\virtualization\v2"
$SCSI_CONTROLLER_TYPE = "Microsoft:Hyper-V:Synthetic SCSI Controller"
$IDE_CONTROLLER_TYPE = "Microsoft:Hyper-V:Emulated IDE Controller"
$HARD_DRIVE_TYPE = "Microsoft:Hyper-V:Synthetic Disk Drive"
$DVD_DRIVE_TYPE = "Microsoft:Hyper-V:Synthetic DVD Drive"
$HARD_DISK_TYPE = "Microsoft:Hyper-V:Virtual Hard Disk"
$DVD_DISK_TYPE = "Microsoft:Hyper-V:Virtual CD/DVD Disk"

#
# Hyper-V Helpers
#

function Get-MsvmComputerSystem
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object]
        $Vm
    )

    $vmid = $Vm.Id
    $msvmComputerSystem = Get-CimInstance -namespace $ROOT_HYPER_V_NAMESPACE -query "select * from Msvm_ComputerSystem where Name = '$vmid'"

    if (-not $msvmComputerSystem)
    {
        throw "Unable to find a virtual machine with id $vmid."
    }

    $msvmComputerSystem
}

function Get-VmSystemSettings
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object]
        $Vm
    )

    Get-MsvmComputerSystem $Vm | Get-CimAssociatedInstance -ResultClass "Msvm_VirtualSystemSettingData" -Association "Msvm_SettingsDefineState"
}

function Get-Vmms
{
    Get-CimInstance -Namespace $ROOT_HYPER_V_NAMESPACE -Class Msvm_VirtualSystemManagementService
}

function Get-VmGuestManagementService
{
    Get-CimInstance -Namespace $ROOT_HYPER_V_NAMESPACE -Class Msvm_VirtualSystemGuestManagementService
}

function Set-VmSystemSettings {
    param(
        [ValidateNotNullOrEmpty()]
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [Microsoft.Management.Infrastructure.CimInstance] $Vssd
    )

    $vmms = Get-Vmms
    $vmms | Invoke-CimMethod -Name "ModifySystemSettings" -Arguments @{
        "SystemSettings" = ($Vssd | ConvertTo-CimEmbeddedString)
    } | Trace-CimMethodExecution -MethodName "ModifySystemSettings" -CimInstance $vmms
}

function Set-VmResourceSettings {
    param(
        [ValidateNotNullOrEmpty()]
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [Microsoft.Management.Infrastructure.CimInstance] $Rasd
    )

    $vmms = Get-Vmms
    $vmms | Invoke-CimMethod -Name "ModifyResourceSettings" -Arguments @{
        "ResourceSettings" = @($Rasd | ConvertTo-CimEmbeddedString)
    } | Trace-CimMethodExecution -MethodName "ModifyResourceSettings" -CimInstance $vmms
}

function Add-VmResourceSettings {
    param(
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [Microsoft.Management.Infrastructure.CimInstance] $Rasd
    )

    $vssd = Get-VmSystemSettings $Vm
    $vmms = Get-Vmms
    $vmms | Invoke-CimMethod -Name "AddResourceSettings" -Arguments @{
        "AffectedConfiguration" = $vssd;
        "ResourceSettings" = @($Rasd | ConvertTo-CimEmbeddedString)
    } | Trace-CimMethodExecution -MethodName "AddResourceSettings" -CimInstance $vmms
}

function Remove-VmResourceSettings {
    param(
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [Microsoft.Management.Infrastructure.CimInstance] $Rasd
    )

    $vmms = Get-Vmms
    $vmms | Invoke-CimMethod -Name "RemoveResourceSettings" -Arguments @{
        "ResourceSettings" = @([Microsoft.Management.Infrastructure.CimInstance[]] $Rasd)
    } | Trace-CimMethodExecution -MethodName "RemoveResourceSettings" -CimInstance $vmms
}

function Get-DefaultRasd {
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [string] $ResourceSubType
    )

    $allocCap = Get-CimInstance -Namespace "root/virtualization/v2" -ClassName "Msvm_AllocationCapabilities" | Where-Object { $_.ResourceSubType -eq $ResourceSubType }
    $allocCap | Get-CimAssociatedInstance -ResultClassName "CIM_ResourceAllocationSettingData" -Association "Msvm_SettingsDefineCapabilities" | Where-Object { $_.InstanceId.EndsWith("Default") }
}

function Get-VmRasd
{
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [string] $ResourceSubType = $null
    )

    $rasds = Get-VmSystemSettings $Vm | Get-CimAssociatedInstance -ResultClassName "Msvm_ResourceAllocationSettingData"

    if ($ResourceSubType) {
        return $rasds | Where-Object { $_.ResourceSubType -eq $ResourceSubType }
    } else {
        return $rasds
    }       
}

function Get-VmSasd
{
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm
    )

    Get-VmSystemSettings $Vm | Get-CimAssociatedInstance -ResultClassName "Msvm_StorageAllocationSettingData"  
}

#
# Hyper-V Configuration Cmdlets
#

function Set-InitialMachineConfiguration
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [string] $ImcHive
    )

    $msvmComputerSystem = Get-MsvmComputerSystem $Vm

    $imcHiveData = Get-Content -Encoding Byte $ImcHive
    $length = [System.BitConverter]::GetBytes([int32]$imcHiveData.Length + 4)
    if ([System.BitConverter]::IsLittleEndian)
    {
        [System.Array]::Reverse($length);
    }
    $imcData = $length + $imcHiveData

    $vmms = Get-Vmms
    $vmms | Invoke-CimMethod -name "SetInitialMachineConfigurationData" -Arguments @{
        "TargetSystem" = $msvmComputerSystem;
        "ImcData" = [byte[]]$imcData
    } | Trace-CimMethodExecution -MethodName "SetInitialMachineConfigurationData" -CimInstance $vmms
}

function Set-OpenHCLFirmware
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [string] $IgvmFile,

        [switch] $IncreaseVtl2Memory
    )

    $vssd = Get-VmSystemSettings $Vm
    # Enable OpenHCL by feature
    $vssd.GuestFeatureSet = 0x00000201
    # Set the OpenHCL image file path
    $vssd.FirmwareFile = $IgvmFile

    if ($IncreaseVtl2Memory) {
        # Configure VM for auto placement mode
        $vssd.Vtl2AddressSpaceConfigurationMode = 1
        # 1GB of OpenHCL address space
        $vssd.Vtl2AddressRangeSize = 1024
        # 512 MB of OpenHCL MMIO space. So total OpenHCL ram = Vtl2AddressRangeSize- Vtl2MmioAddressRangeSize.
        $vssd.Vtl2MmioAddressRangeSize = 512
    }

    Set-VmSystemSettings $vssd
}

function Set-VmCommandLine
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string] $CommandLine
    )

    $vssd = Get-VmSystemSettings $Vm
    $vssd.FirmwareParameters = [System.Text.Encoding]::UTF8.GetBytes($CommandLine)
    Set-VmSystemSettings $vssd
}

function Get-VmCommandLine
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm
    )

    $vssd = Get-VmSystemSettings $Vm
    [System.Text.Encoding]::UTF8.GetString($vssd.FirmwareParameters)
}

function Get-VmScsiControllerProperties
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Controller
    )

    $vm = Get-VM -Id $Controller.VMId;
    $ControllerNumber = $Controller.ControllerNumber;

    $rasds = $vm | Get-VmRasd -ResourceSubType $SCSI_CONTROLLER_TYPE;
    $rasd = $rasds[$ControllerNumber];

    return "$ControllerNumber,$($rasd.VirtualSystemIdentifiers[0])"
}

function Get-VmScsiConfiguration
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm
    )

    $controllers = @($Vm | Get-VmRasd -ResourceSubType $SCSI_CONTROLLER_TYPE)

    for ($i = 0; $i -lt $controllers.Count; $i++) {
        $controllerPath = Get-CimInstancePath $controllers[$i]
        $iid = $controllers[$i].InstanceId
        $vsid = $controllers[$i].VirtualSystemIdentifiers[0]
        $vtl = $controllers[$i].TargetVtl

        Write-Host $i $vtl $vsid $iid

        $drives = $Vm | Get-VmRasd | Where-Object {
            (($_.ResourceSubType -eq $HARD_DRIVE_TYPE) -or ($_.ResourceSubType -eq $DVD_DRIVE_TYPE)) -and
            ($_.Parent -eq $controllerPath)
        }

        $drives | ForEach-Object {
            $drivePath = Get-CimInstancePath $_
            $iid = $_.InstanceId
            $lun = $_.AddressOnParent
            $type = if ($_.ResourceSubType -eq $HARD_DRIVE_TYPE) {
                "hdd"
            } elseif ($_.ResourceSubType -eq $DVD_DRIVE_TYPE) {
                "dvd"
            } else {
                "unknown"
            }

            Write-Host "   " $lun $type $iid

            $disk = $Vm | Get-VmSasd | Where-Object {
                (($_.ResourceSubType -eq $HARD_DISK_TYPE) -or ($_.ResourceSubType -eq $DVD_DISK_TYPE)) -and
                ($_.Parent -eq $drivePath)
            }

            if ($disk) {
                $iid = $disk.InstanceId
                $path = $disk.HostResource[0]

                Write-Host "       " $path $iid
            }
        }
    }
}

function Get-VmScsiControllerNumberWithId
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [Guid] $Vsid
    )

    $vsid = $Vsid.ToString()
    $controllers = @($Vm | Get-VmRasd -ResourceSubType $SCSI_CONTROLLER_TYPE)

    for ($i = 0; $i -lt $controllers.Count; $i++) {
        if ($controllers[$i].VirtualSystemIdentifiers[0] -eq "{$vsid}") {
            return $i
        }
    }

    $vmid = $Vm.Id
    throw "controller $vsid does not exist on vm $vmid"
}

function Get-VmScsiControllerIdByNumber
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [int] $ControllerNumber
    )

    $controllers = @($Vm | Get-VmRasd -ResourceSubType $SCSI_CONTROLLER_TYPE)

    if (($ControllerNumber -lt 0) -or ($ControllerNumber -ge $controllers.Count)) {
        $vmid = $Vm.Id
        throw "controller number $ControllerNumber does not exist on vm $vmid"
    }

    $controllers[$ControllerNumber].VirtualSystemIdentifiers[0]
}

function Get-VmIdeController
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [int] $ControllerNumber,

        [bool] $Expected = $true
    )

    $vmid = $Vm.Id
    $controller = $Vm | Get-VmRasd -ResourceSubType $IDE_CONTROLLER_TYPE | Where-Object { $_.Address -eq $ControllerNumber }

    if ($Expected -and (-not $controller)) {
        throw "ide controller $ControllerNumber does not exist on vm $vmid"
    }

    if ((-not $Expected) -and $controller) {
        throw "ide controller $ControllerNumber already exists on vm $vmid"
    }

    return $controller
}

function Get-VmScsiControllerWithId
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [Guid] $Vsid,

        [bool] $Expected = $true
    )

    $vmid = $Vm.Id
    $vsid = $Vsid.ToString()
    $controller = $Vm | Get-VmRasd -ResourceSubType $SCSI_CONTROLLER_TYPE | Where-Object { $_.VirtualSystemIdentifiers[0] -eq "{$vsid}" }

    if ($Expected -and (-not $controller)) {
        throw "scsi controller $vsid does not exist on vm $vmid"
    }

    if ((-not $Expected) -and $controller) {
        throw "scsi controller $vsid already exists on vm $vmid"
    }

    return $controller
}

function Add-VmScsiControllerWithId
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [Guid] $Vsid,

        [Parameter(Mandatory = $true)]
        [int] $TargetVtl
    )

    $Vm | Get-VmScsiControllerWithId -Vsid $Vsid -Expected $false
    
    $vsid = $Vsid.ToString()
    $template = Get-DefaultRasd $SCSI_CONTROLLER_TYPE
    $controllerConfig = Copy-CimInstanceWithNewProperties $template @{ "VirtualSystemIdentifiers" = @("{$vsid}"); "TargetVtl" = $TargetVtl }
    $controllerAddResult = $Vm | Add-VmResourceSettings -Rasd $controllerConfig
    $controller = $controllerAddResult.ResultingResourceSettings[0]
    Write-Host "added controller:" $controller.InstanceId
    
    return $controller
}

function Remove-VmScsiControllerWithId
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [Guid] $Vsid
    )

    $controller = $Vm | Get-VmScsiControllerWithId -Vsid $Vsid -Expected $true
    $controllerPath = Get-CimInstancePath $controller

    $drives = $Vm | Get-VmRasd | Where-Object {
        (($_.ResourceSubType -eq $HARD_DRIVE_TYPE) -or ($_.ResourceSubType -eq $DVD_DRIVE_TYPE)) -and
        ($_.Parent -eq $controllerPath)
    }
    $drives | ForEach-Object { $Vm | Remove-VmDrive -Drive $_ }

    Write-Host "removing controller:" $controller.InstanceId
    $controller | Remove-VmResourceSettings
}

function Remove-VmDrive
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [System.Object] $Drive
    )

    $drivePath = Get-CimInstancePath $Drive

    $disk = $Vm | Get-VmSasd | Where-Object {
        (($_.ResourceSubType -eq $HARD_DISK_TYPE) -or ($_.ResourceSubType -eq $DVD_DISK_TYPE)) -and
        ($_.Parent -eq $drivePath)
    }

    Write-Host $disk.InstanceId $drivePath

    if ($disk) {
        Write-Host "removing disk:" $disk.InstanceId
        $disk | Remove-VmResourceSettings
    }

    Write-Host "removing drive:" $Drive.InstanceId
    $Drive | Remove-VmResourceSettings
}

function Set-VmDrive
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [guid] $ControllerVsid,

        [int] $ControllerNumber = 0,

        [Parameter(Mandatory = $true)]
        [int] $Lun,

        [string] $DiskPath = $null,

        [switch] $Dvd,

        [switch] $AllowModifyExisting
    )
    
    if ($ControllerVsid) {
        $controller = $Vm | Get-VmScsiControllerWithId -Vsid $ControllerVsid
        $controllerId = $controller.VirtualSystemIdentifiers[0]
    } else {
        $controller = $Vm | Get-VmIdeController -ControllerNumber $ControllerNumber
        $controllerId = $controller.Address
    }

    $vmid = $Vm.Id
    

    $controllerPath = Get-CimInstancePath $controller
    Write-Host "modifying controller:" $controller.InstanceId

    if ($Dvd) {
        $driveType = $DVD_DRIVE_TYPE
        $diskType = $DVD_DISK_TYPE
    } else {
        $driveType = $HARD_DRIVE_TYPE
        $diskType = $HARD_DISK_TYPE
    }
    
    # check if the drive already exists
    $drive = $Vm | Get-VmRasd | Where-Object {
        (($_.ResourceSubType -eq $HARD_DRIVE_TYPE) -or ($_.ResourceSubType -eq $DVD_DRIVE_TYPE)) -and
        ($_.AddressOnParent -eq $lun) -and
        ($_.Parent -eq $controllerPath)
    }

    if ($drive -and (-not $AllowModifyExisting)) {
        throw "drive $Lun on controller $controllerId already exists on vm $vmid"
    }

    # (re-)create the drive if necessary
    if ((-not $drive) -or ($drive.ResourceSubType -ne $driveType)) {
        if ($drive) {
            $Vm | Remove-VmDrive -Drive $drive
        }

        $driveTemplate = Get-DefaultRasd $driveType
        $driveConfig = Copy-CimInstanceWithNewProperties $driveTemplate @{ "AddressOnParent" = $lun; "Parent" = $controllerPath }
        $driveAddResult = $Vm | Add-VmResourceSettings -Rasd $driveConfig
        $drive = $driveAddResult.ResultingResourceSettings[0]
        Write-Host "added drive:" $drive.InstanceId
    } else {
        Write-Host "found drive:" $drive.InstanceId
    }

    # remove disk if already inserted
    $drivePath = Get-CimInstancePath $drive
    $disk = $Vm | Get-VmSasd | Where-Object {
        (($_.ResourceSubType -eq $HARD_DISK_TYPE) -or ($_.ResourceSubType -eq $DVD_DISK_TYPE)) -and
        ($_.Parent -eq $drivePath)
    }
    if ($disk) {
        Write-Host "removing disk:" $disk.InstanceId
        $disk | Remove-VmResourceSettings
    }
    
    # insert the disk if provided
    if ($DiskPath) {
        $diskTemplate = Get-DefaultRasd $diskType
        $diskConfig = Copy-CimInstanceWithNewProperties $diskTemplate @{ "Parent" = $drivePath; "HostResource" = @($DiskPath) }
        $diskAddResult = $Vm | Add-VmResourceSettings -Rasd $diskConfig
        $disk = $diskAddResult.ResultingResourceSettings[0]
        Write-Host "added disk:" $disk.InstanceId
    }
}

function Set-VmScsiControllerTargetVtl
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [int] $ControllerNumber,

        [Parameter(Mandatory = $true)]
        [int] $TargetVtl
    )

    $vssd = Get-VmSystemSettings $Vm
    $rasds = $vssd | Get-CimAssociatedInstance -ResultClassName "Msvm_ResourceAllocationSettingData" | Where-Object { $_.ResourceSubType -eq $SCSI_CONTROLLER_TYPE }
    $rasd = $rasds[$ControllerNumber]
    $rasd.TargetVtl = $TargetVtl
    $rasd | Set-VmResourceSettings
}

function Set-VMBusRedirect
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [bool] $Enable
    )

    $vssd = Get-VmSystemSettings $Vm
    $vssd | ForEach-Object {
            $_.VMBusMessageRedirection = [int]$Enable
            $_
        }
    Set-VmSystemSettings $vssd
}

function Restart-OpenHCL
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [int] $TimeoutHintSeconds = 15, # Ends up as the deadline in GuestSaveRequest (see the handling of
                                        # SaveGuestVtl2StateNotification in guest_emulation_transport). Keep O(15 seconds).
                                        #
                                        # Also used as the hint for how long to wait (in this cmdlet) for the
                                        # ReloadManagementVtl method to complete.
        [switch] $OverrideVersionChecks,
        [switch] $DisableNvmeKeepalive
    )
    
    $vmid = $Vm.Id.tostring();
    $guestManagementService = Get-VmGuestManagementService;
    $options = 0;
    if ($OverrideVersionChecks) {
        $options = $options -bor 1;
    }
    if ($DisableNvmeKeepalive) {
        $options = $options -bor 16;
    }
    $result = $guestManagementService | Invoke-CimMethod -name "ReloadManagementVtl" -Arguments @{
        "VmId"            = $vmid
        "Options"         = $options
        "TimeoutHintSecs" = $TimeoutHintSeconds
    }

    $result | Trace-CimMethodExecution -CimInstance $guestManagementService -MethodName "ReloadManagementVtl" -TimeoutSeconds $TimeoutHintSeconds
}

function Get-VmScreenshot
{
    [CmdletBinding()]
    Param(
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $vmms = Get-Vmms
    $vmcs = Get-MsvmComputerSystem $Vm

    # Get the resolution of the screen at the moment
    $videoHead = $vmcs | Get-CimAssociatedInstance -ResultClassName "Msvm_VideoHead"
    $x = $videoHead.CurrentHorizontalResolution
    $y = $videoHead.CurrentVerticalResolution

    # Get screenshot
    $image = $vmms | Invoke-CimMethod -MethodName "GetVirtualSystemThumbnailImage" -Arguments @{
        TargetSystem = $vmcs
        WidthPixels = $x
        HeightPixels = $y
    } | Trace-CimMethodExecution -MethodName "GetVirtualSystemThumbnailImage" -CimInstance $vmms

    [IO.File]::WriteAllBytes($Path, $image.ImageData)

    return "$x,$y"
}

function Set-TurnOffOnGuestRestart
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [bool] $Enable
    )

    $vssd = Get-VmSystemSettings $Vm
    $vssd.TurnOffOnGuestRestart = $Enable
    Set-VmSystemSettings $vssd
}

function Get-GuestStateFile
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm
    )

    $vssd = Get-VmSystemSettings $Vm
    $guestStateDataRoot = $vssd.GuestStateDataRoot
    $guestStateFile = $vssd.GuestStateFile
    
    return "$guestStateDataRoot\$guestStateFile"
}

function Set-Vtl2Settings {
    [CmdletBinding()]
    param (
        [Parameter(Position = 0, Mandatory = $true)]
        [Guid] $VmId,

        [Parameter(Mandatory = $true)]
        [ValidateNotNullOrEmpty()]
        [string]$Namespace,

        [Parameter(Mandatory = $true)]
        [string]$SettingsFile,

        [string]$ClientName = 'Petri'
    )

    $settingsContent = Get-Content -Raw -Path $SettingsFile

    $guestManagement = Get-VmGuestManagementService

    $options = New-Object Microsoft.Management.Infrastructure.Options.CimOperationOptions
    $options.SetCustomOption("ClientName", $ClientName, $false)

    # Parameter - VmId
    $p1 = [Microsoft.Management.Infrastructure.CimMethodParameter]::Create("VmId", $VmId.ToString(), [Microsoft.Management.Infrastructure.cimtype]::String, [Microsoft.Management.Infrastructure.CimFlags]::In)

    # Parameter - Namespace
    $p2 = [Microsoft.Management.Infrastructure.CimMethodParameter]::Create("Namespace", $Namespace, [Microsoft.Management.Infrastructure.cimtype]::String, [Microsoft.Management.Infrastructure.CimFlags]::In)

    # Parameter - Settings
    # The input is a byte buffer with the size prepended.
    # Size is a uint32 in network byte order (i.e. Big Endian)
    # Size includes the size itself and the payload.

    $bytes = [system.Text.Encoding]::UTF8.GetBytes($settingsContent)

    $header = [System.BitConverter]::GetBytes([uint32]($bytes.Length + 4))
    if ([System.BitConverter]::IsLittleEndian) {
        [System.Array]::Reverse($header)
    }
    $bytes = $header + $bytes

    $p3 = [Microsoft.Management.Infrastructure.CimMethodParameter]::Create("Settings", $bytes, [Microsoft.Management.Infrastructure.cimtype]::UInt8Array, [Microsoft.Management.Infrastructure.CimFlags]::In)

    $result = $guestManagement | Invoke-CimMethod -MethodName GetManagementVtlSettings -Arguments @{"VmId" = $VmId.ToString(); "Namespace" = $Namespace } |
    Trace-CimMethodExecution -CimInstance $guestManagement -MethodName "GetManagementVtlSettings"
    $updateId = $result.CurrentUpdateId

    $p4 = [Microsoft.Management.Infrastructure.CimMethodParameter]::Create("CurrentUpdateId", $updateId, [Microsoft.Management.Infrastructure.cimtype]::UInt64, [Microsoft.Management.Infrastructure.CimFlags]::In)

    $params = New-Object Microsoft.Management.Infrastructure.CimMethodParametersCollection
    $params.Add($p1); $params.Add($p2); $params.Add($p3); $params.Add($p4)

    $cimSession = New-CimSession
    $cimSession.InvokeMethod($ROOT_HYPER_V_NAMESPACE, $guestManagement, "SetManagementVtlSettings", $params, $options) |
    Trace-CimMethodExecution -CimInstance $guestManagement -MethodName "SetManagementVtlSettings" | Out-Null

    $cimSession | Remove-CimSession | Out-Null
}

function Set-GuestStateIsolationMode
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object] $Vm,

        [int] $Mode
    )

    $vssd = Get-VmSystemSettings $Vm
    $vssd.GuestStateIsolationMode = $Mode
    Set-VmSystemSettings $vssd
}

#
# CIM Helpers
#

function ConvertTo-CimEmbeddedString
{
    [CmdletBinding()]
    param(
        [Parameter(ValueFromPipeline)]
        [Microsoft.Management.Infrastructure.CimInstance] $CimInstance
    )

    if ($null -eq $CimInstance)
    {
        return ""
    }

    $cimSerializer = [Microsoft.Management.Infrastructure.Serialization.CimSerializer]::Create()
    $serializedObj = $cimSerializer.Serialize($CimInstance, [Microsoft.Management.Infrastructure.Serialization.InstanceSerializationOptions]::None)
    return [System.Text.Encoding]::Unicode.GetString($serializedObj)
}

# CIM is strict and won't let you write read-only properties on instances, so
# we need to create instances with the read-only properties set to what we need them
# to be. Use this helper function to clone RASDD instances with the specified
# properties and values as given by NewPropertiesDict. Throws if a property that did not
# originally exist on the object is given.
function Copy-CimInstanceWithNewProperties {
    param(
        [parameter(Mandatory = $true)]
        [Microsoft.Management.Infrastructure.CimInstance] $CimInstance,
        [parameter(Mandatory = $true)]
        [System.Collections.Hashtable] $NewPropertiesDict
    )

    $newProperties = @{ }

    $class = Get-CimClass -Namespace $CimInstance.CimSystemProperties.Namespace `
        -ClassName $CimInstance.CimSystemProperties.ClassName

    $compareArgs = @{ReferenceObject = $class.CimClassProperties.Name;
        DifferenceObject             = @($NewPropertiesDict.Keys);
        PassThru                     = $true;
        CaseSensitive                = $false
    };

    $invalidProperties = Compare-Object @compareArgs | Where-Object { $_.SideIndicator -eq "=>" }
    if ($invalidProperties) {
        throw "Invalid properties are specified - $($invalidProperties -join ',')"
    }

    foreach ($prop in $class.CimClassProperties) {
        if ($NewPropertiesDict.ContainsKey("$($prop.Name)")) {
            $newProperties["$($prop.Name)"] = $NewPropertiesDict["$($prop.Name)"]
        }
        else {
            $newProperties["$($prop.Name)"] = $CimInstance."$($prop.Name)"
        }
    }

    return ($class | New-CimInstance -ClientOnly -Property $newProperties)
}

<#
.SYNOPSIS
    Helper function that processes a CIMMethodResult/Msvm_ConcreteJob.

.DESCRIPTION
    Helper function that processes a CIMMethodResult/Msvm_ConcreteJob.

.PARAMETER WmiClass
    Supplies the WMI class object from where the method is being called.

.PARAMETER MethodName
    Supplies the method name that the job called.

.PARAMETER TimeoutSeconds
    Supplies the duration in seconds to wait for job completion.

.INPUTS
    Input a CIMMethodResult object through the pipeline, or any object with
    a ReturnValue property and optionally a Job property that is an Msvm_ConcreteJob.

.OUTPUTS
    Returns the input object on success; throws on error.

.EXAMPLE
    $job | Trace-CimMethodExecution -WmiClass $VMMS -MethodName ExportSystemDefinition
        Processes a job for the given class and method, shows progress until it reaches completion.
#>
filter Trace-CimMethodExecution {
    param (
        [Alias("WmiClass")]
        [Microsoft.Management.Infrastructure.CimInstance]$CimInstance = $null,
        [string] $MethodName = $null,
        [int] $TimeoutSeconds = 0
    )

    $errorCode = 0
    $returnObject = $_
    $job = $null
    $shouldProcess = $true
    $timer = $null

    if ($_.CimSystemProperties.ClassName -eq "Msvm_ConcreteJob") {
        $job = $_
    }
    elseif ((Get-Member -InputObject $_ -name "ReturnValue" -MemberType Properties)) {
        if ((Get-Member -InputObject $_.ReturnValue -name "Value" -MemberType Properties)) {
            # InvokeMethod from New-CimSession return object
            $returnValue = $_.ReturnValue.Value
        }
        else {
            # Invoke-CimMethod return object
            $returnValue = $_.ReturnValue
        }

        if (($returnValue -ne 0) -and ($returnValue -ne 4096)) {
            # An error occurred
            $errorCode = $returnValue
            $shouldProcess = $false
        }
        elseif ($returnValue -eq 4096) {
            if ((Get-Member -InputObject $_ -name "Job" -MemberType Properties) -and $_.Job) {
                # Invoke-CimMethod return object
                # CIM does not seem to actually populate the non-key fields on a reference, so we need
                # to go get the actual instance of the job object we got.
                $job = ($_.Job | Get-CimInstance)
            }
            elseif ((Get-Member -InputObject $_ -name "OutParameters" -MemberType Properties) -and $_.OutParameters["Job"]) {
                # InvokeMethod from New-CimSession return object
                $job = ($_.OutParameters["Job"].Value | Get-CimInstance)
            }
            else {
                throw "ReturnValue of 4096 with no Job object!"
            }
        }
        else {
            # No job and no error, just exit.
            return $returnObject
        }
    }
    else {
        throw "Pipeline input object is not a job or CIM method result!"
    }

    if ($shouldProcess) {
        $caption = if ($job.Caption) { $job.Caption } else { "Job in progress (no caption available)" }
        $jobStatus = if ($job.JobStatus) { $job.JobState } else { "No job status available" }
        $percentComplete = if ($job.PercentComplete) { $job.PercentComplete } else { 0 }

        if (($job.JobState -eq 4) -and $TimeoutSeconds -gt 0) {
            $timer = [Diagnostics.Stopwatch]::StartNew()
        }

        while ($job.JobState -eq 4) {
            if (($timer -ne $null) -and ($timer.Elapsed.Seconds -gt $TimeoutSeconds)) {
                throw "Job did not complete within $TimeoutSeconds seconds!"
            }
            Write-Progress -Activity $caption -Status ("{0} - {1}%" -f $jobStatus, $percentComplete) -PercentComplete $percentComplete
            Start-Sleep -seconds 1
            $job = $job | Get-CimInstance
        }

        if ($timer) { $timer.Stop() }

        if ($job.JobState -ne 7) {
            if (![string]::IsNullOrEmpty($job.ErrorDescription)) {
                Throw $job.ErrorDescription
            }
            else {
                $errorCode = $job.ErrorCode
            }
        }
        Write-Progress -Activity $caption -Status $jobStatus -PercentComplete 100 -Completed:$true
    }

    if ($errorCode -ne 0) {
        if ($CimInstance -and $MethodName) {
            $cimClass = Get-CimClass -ClassName $CimInstance.CimSystemProperties.ClassName `
                -Namespace $CimInstance.CimSystemProperties.Namespace -ComputerName $CimInstance.CimSystemProperties.ServerName

            $methodQualifierValues = ($cimClass.CimClassMethods[$MethodName].Qualifiers["ValueMap"].Value)
            $indexOfError = [System.Array]::IndexOf($methodQualifierValues, [string]$errorCode)

            if (($indexOfError -ne "-1") -and $methodQualifierValues) {
                # If the class in question has an error description defined for the error in its Values collection, use it
                if ($cimClass.CimClassMethods[$MethodName].Qualifiers["Values"] -and $indexOfError -lt $cimClass.CimClassMethods[$MethodName].Qualifiers["Values"].Value.Length) {
                    Throw "ReturnCode: ", $errorCode, " ErrorMessage: '", $cimClass.CimClassMethods[$MethodName].Qualifiers["Values"].Value[$indexOfError], "' - when calling $MethodName"
                }
                else {
                    # The class has no error description for the error code, so just return the error code
                    Throw "ReturnCode: ", $errorCode, " - when calling $MethodName"
                }
            }
            else {
                # The error code is not found in the ValueMap, so just return the error code
                Throw "ReturnCode: ", $errorCode, " ErrorMessage: 'MessageNotFound' - when calling $MethodName"
            }
        }
        else {
            Throw "ReturnCode: ", $errorCode, "When calling $MethodName - for rich error messages provide classpath and method name."
        }
    }

    return $returnObject
}

<#
.SYNOPSIS
    Get the __PATH property from a CIMInstance object.

.DESCRIPTION
    The Get-CIMInstance cmdlet by default doesn't display the WMI system properties
    like __SERVER. The properties are available in the CimSystemProperties property
    except for __PATH. This function will construct the __PATH property and return it.

.EXAMPLE
    get-ciminstance win32_memorydevice | get-ciminstancepath

    \\SERVER01\root\cimv2:Win32_MemoryDevice.DeviceID="Memory Device 0"
    \\SERVER01\root\cimv2:Win32_MemoryDevice.DeviceID="Memory Device 1"

.INPUTS
    A CIMInstance object

.OUTPUTS
    String representing the path of the input object
#>
function Get-CimInstancePath {
    [CmdletBinding()]
    param (
        [Parameter(Position = 0, ValueFromPipeline = $True)]
        [ValidateNotNullorEmpty()]
        [Microsoft.Management.Infrastructure.CimInstance]$CimInstance
    )

    $key = $CimInstance.CimClass.CimClassProperties |
    Where-Object { $_.Qualifiers.Name -contains "key" } |
    Select-Object -ExpandProperty Name

    $path = ('\\{0}\{1}:{2}{3}' -f $CimInstance.CimSystemProperties.ServerName.ToUpper(),
        $CimInstance.CimSystemProperties.Namespace.Replace("/", "\"),
        $CimInstance.CimSystemProperties.ClassName,
        $(if ($key -is [array]) {
                # Need a string with every key in the array, keys separated by commas
                $sep = ""
                $s = [string]"."
                foreach ($k in $key) {
                    $s += "$($sep)$($k)=""$($CimInstance.($k))"""
                    $sep = ","
                }
                $s
            }
            elseif ($key) {
                # just a single key
                ".$($key)=""$($CimInstance.$key)"""
            }
            else {
                #no key
                '=@'
            }).Replace('\', '\\')
    )

    return $path
}