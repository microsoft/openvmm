# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Periodically sends an ETW capture-state request for a provider to a running
# trace session. Exits after a final request once the stop file appears, or
# when the parent process exits or the session is stopped.
#
# Hypervisor diagnostics events are buffered in a circular hypervisor buffer
# and forwarded to ETW by hvservice when it receives a capture-state request,
# so requests are sent promptly to avoid losing events to buffer wraparound.

param(
    [Parameter(Mandatory)] [string] $Session,
    [Parameter(Mandatory)] [guid] $Provider,
    [Parameter(Mandatory)] [string] $StopPath,
    [Parameter(Mandatory)] [string] $LogPath,
    [Parameter(Mandatory)] [string] $HypervisorInfoPath,
    [Parameter(Mandatory)] [int] $ParentPid,
    [int] $IntervalSeconds = 5
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class PetriEtwCaptureState
{
    [DllImport("advapi32.dll", EntryPoint = "ControlTraceW", CharSet = CharSet.Unicode)]
    static extern uint ControlTrace(ulong handle, string name, IntPtr properties, uint controlCode);

    [DllImport("advapi32.dll")]
    static extern uint EnableTraceEx2(ulong handle, ref Guid provider, uint controlCode, byte level,
        ulong matchAnyKeyword, ulong matchAllKeyword, uint timeout, IntPtr enableParameters);

    // sizeof(EVENT_TRACE_PROPERTIES) on 64-bit Windows.
    const int PropertiesSize = 120;
    const int NameBytes = 2048;
    const uint EVENT_TRACE_CONTROL_QUERY = 0;
    const uint EVENT_CONTROL_CODE_CAPTURE_STATE = 2;

    // Returns a Win32 error code.
    public static uint Capture(string session, Guid provider)
    {
        int size = PropertiesSize + 2 * NameBytes;
        IntPtr properties = Marshal.AllocHGlobal(size);
        try
        {
            for (int i = 0; i < size; i++)
            {
                Marshal.WriteByte(properties, i, 0);
            }
            // Wnode.BufferSize, LogFileNameOffset, and LoggerNameOffset.
            Marshal.WriteInt32(properties, 0, size);
            Marshal.WriteInt32(properties, 112, PropertiesSize + NameBytes);
            Marshal.WriteInt32(properties, 116, PropertiesSize);
            uint error = ControlTrace(0, session, properties, EVENT_TRACE_CONTROL_QUERY);
            if (error != 0)
            {
                return error;
            }
            // Wnode.HistoricalContext holds the session handle.
            ulong handle = (ulong)Marshal.ReadInt64(properties, 8);
            return EnableTraceEx2(handle, ref provider, EVENT_CONTROL_CODE_CAPTURE_STATE, 5,
                ulong.MaxValue, 0, 0, IntPtr.Zero);
        }
        finally
        {
            Marshal.FreeHGlobal(properties);
        }
    }
}
'@

function Write-Log([string] $Message) {
    Add-Content -LiteralPath $LogPath -Value "$([DateTime]::UtcNow.ToString('o')) $Message"
}

function Request-CaptureState([string] $Reason) {
    $result = [PetriEtwCaptureState]::Capture($Session, $Provider)
    Write-Log "$Reason capture-state result=$result"
    return $result
}

# Record which hypervisor the host has, since the diagnostics events are only
# emitted by instrumented hypervisor builds.
try {
    $info = @(Get-Item -Path "$env:SystemRoot\System32\hv*64.exe" | ForEach-Object {
            '{0} version={1} size={2} modified={3:o}' -f
            $_.Name, $_.VersionInfo.FileVersion, $_.Length, $_.LastWriteTimeUtc
        })
    $info += @(bcdedit /enum '{current}' | Select-String -Pattern 'hypervisor')
    $info | Set-Content -LiteralPath $HypervisorInfoPath
} catch {
    Write-Log "failed to record hypervisor image: $_"
}

$ERROR_WMI_INSTANCE_NOT_FOUND = 4201
while ($true) {
    if (Test-Path -LiteralPath $StopPath) {
        Request-CaptureState 'final' | Out-Null
        # Give hvservice time to forward the drained events before the session
        # is stopped.
        Start-Sleep -Seconds 3
        break
    }
    if (-not (Get-Process -Id $ParentPid -ErrorAction SilentlyContinue)) {
        Write-Log "parent process $ParentPid exited"
        break
    }
    if ((Request-CaptureState 'periodic') -eq $ERROR_WMI_INSTANCE_NOT_FOUND) {
        Write-Log 'trace session stopped'
        break
    }
    Start-Sleep -Seconds $IntervalSeconds
}
