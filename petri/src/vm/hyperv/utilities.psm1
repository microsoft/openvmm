# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

#
# Constants
#

$ROOT_HYPER_V_NAMESPACE = "root\virtualization\v2"

function Get-Vmms
{
    Get-CimInstance -Namespace $ROOT_HYPER_V_NAMESPACE -Class Msvm_VirtualSystemManagementService
}

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
            if (($timer -ne $null) -and ($timer.Elapsed.TotalSeconds -gt $TimeoutSeconds)) {
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
