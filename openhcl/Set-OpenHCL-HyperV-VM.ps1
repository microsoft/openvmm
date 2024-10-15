
<#
.SYNOPSIS
    Sets OpenHCL for the VM on the current Hyper-V host. 

.DESCRIPTION
    Sets OpenHCL for the VM on the current Hyper-V host. 

.PARAMETER CIMInstanceOfVM
    The CIMInstance of the WMI VM object you want to modify.

.EXAMPLE
    \openhcl\Set-OpenHCL-HyperV-VM.ps1 -CIMInstanceOfVM $CIMInstanceOfVM
#>

param
(
    [Parameter(Mandatory)]
    [Microsoft.Management.Infrastructure.CimInstance] $CIMInstanceOfVM
)

$ROOT_HYPER_V_NAMESPACE = "root\virtualization\v2"

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
                else { # The class has no error description for the error code, so just return the error code
                    Throw "ReturnCode: ", $errorCode, " - when calling $MethodName"
                }
            }
            else { # The error code is not found in the ValueMap, so just return the error code
                Throw "ReturnCode: ", $errorCode, " ErrorMessage: 'MessageNotFound' - when calling $MethodName"
            }
        }
        else {
            Throw "ReturnCode: ", $errorCode, "When calling $MethodName - for rich error messages provide classpath and method name."
        }
    }
    return $returnObject
}
function ConvertTo-CimEmbeddedString {
    [CmdletBinding()]
    param(
        [Parameter(ValueFromPipeline)]
        [Microsoft.Management.Infrastructure.CimInstance]$CimInstance
    )
    if ($null -eq $CimInstance) {
        return ""
    }
    $cimSerializer = [Microsoft.Management.Infrastructure.Serialization.CimSerializer]::Create()
    $serializedObj = $cimSerializer.Serialize($CimInstance, [Microsoft.Management.Infrastructure.Serialization.InstanceSerializationOptions]::None)
    return [System.Text.Encoding]::Unicode.GetString($serializedObj)
}

function Get-WMIVSManagementService {
    [CmdletBinding()]
    param()
    Get-CimInstance -Namespace $ROOT_HYPER_V_NAMESPACE -Class Msvm_VirtualSystemManagementService
}

function Set-VmSystemSettings {
    param(
        [ValidateNotNullOrEmpty()]
        [Parameter(Position = 0, Mandatory = $true, ValueFromPipeline = $true)]
        [Microsoft.Management.Infrastructure.CimInstance]$CimRasd
    )
    Begin {
        $vmms = Get-WMIVSManagementService
    }
    Process {
        $vmms |
        Invoke-CimMethod -Name "ModifySystemSettings" -Arguments @{
            "SystemSettings" = ($CimRasd | ConvertTo-CimEmbeddedString)
        } |
        Trace-CimMethodExecution -CimInstance $vmms -MethodName "ModifySystemSettings" | Out-Null
    }
}

if (Get-VMHostSupportedVersion | Where-Object { $_.Version -eq "13.0" }) { 
$CIMInstanceOfVM.GuestFeatureSet = 0x00000201
Set-VmSystemSettings $CIMInstanceOfVM
}else {
    write-output "This Windows host does not support VMs version 12.0; please update your Windows version according to the instruction and try again."
}



