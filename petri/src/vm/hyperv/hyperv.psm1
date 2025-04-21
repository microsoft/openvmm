# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

$ROOT_HYPER_V_NAMESPACE = "root\virtualization\v2"

function Get-MsvmComputerSystem
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object]
        $Vm
    )

    $vmid = $Vm.Id
    $msvm_ComputerSystem = Get-CimInstance -namespace $ROOT_HYPER_V_NAMESPACE -query "select * from Msvm_ComputerSystem where Name = '$vmid'"

    if (-not $msvm_ComputerSystem)
    {
        throw "Unable to find a virtual machine with id $vmid."
    }

    $msvm_ComputerSystem
}

function Get-Vssd
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

function Set-InitialMachineConfiguration
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object]
        $Vm,

        [Parameter(Mandatory = $true)]
        [string] $ImcHive
    )

    $msvm_ComputerSystem = Get-MsvmComputerSystem $Vm

    $imcHiveData = Get-Content -Encoding Byte $ImcHive
    $length = [System.BitConverter]::GetBytes([int32]$imcHiveData.Length + 4)
    if ([System.BitConverter]::IsLittleEndian)
    {
        [System.Array]::Reverse($length);
    }
    $imcData = $length + $imcHiveData

    $vmms = Get-Vmms
    $vmms | Invoke-CimMethod -name "SetInitialMachineConfigurationData" -Arguments @{
        "TargetSystem" = $msvm_ComputerSystem;
        "ImcData" = [byte[]]$imcData
    }
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
    }
}

function Set-OpenHCLFirmware
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object]
        $Vm,

        [Parameter(Mandatory = $true)]
        [string] $IgvmFile,

        [switch] $IncreaseVtl2Memory
    )

    $vssd = Get-Vssd $Vm
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
        [System.Object]
        $Vm,

        [Parameter(Mandatory = $true)]
        [string] $CommandLine
    )

    $vssd = Get-Vssd $Vm
    $vssd.FirmwareParameters = [System.Text.Encoding]::UTF8.GetBytes($CommandLine)
    Set-VmSystemSettings $vssd
}

function Get-VmCommandLine
{
    [CmdletBinding()]
    Param (
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [System.Object]
        $Vm
    )

    $vssd = Get-Vssd $Vm
    [System.Text.Encoding]::UTF8.GetString($vssd.FirmwareParameters)
}

