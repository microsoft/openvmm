# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
    Grants or revokes a Windows user-right (privilege) for an account by SID
    via the LSA API.

.DESCRIPTION
    Makes it easy to grant a privilege such as SeLockMemoryPrivilege ("Lock
    pages in memory") to an account. It calls LsaAddAccountRights (or
    LsaRemoveAccountRights with -Remove) directly, which takes a raw SID and
    works for any account.

    Run from an ELEVATED PowerShell prompt. Privilege changes take effect at
    the next logon, so sign out and back in, then verify with `whoami /priv`.

.PARAMETER Privilege
    The privilege constant to grant or revoke (default: SeLockMemoryPrivilege).

.PARAMETER Sid
    The account SID to change it for. Defaults to the current user's SID.

.PARAMETER Remove
    Revoke the privilege instead of granting it.

.EXAMPLE
    .\grant-privilege.ps1
    Grants SeLockMemoryPrivilege to the current user.

.EXAMPLE
    .\grant-privilege.ps1 -Privilege SeServiceLogonRight -Sid S-1-12-1-...
    Grants a specific privilege to a specific SID.

.EXAMPLE
    .\grant-privilege.ps1 -Remove
    Revokes SeLockMemoryPrivilege from the current user.
#>
[CmdletBinding()]
param(
    [string]$Privilege = 'SeLockMemoryPrivilege',
    [string]$Sid = ([System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value),
    [switch]$Remove
)

$ErrorActionPreference = 'Stop'

# Require elevation — the LSA account-rights APIs need admin rights.
$isAdmin = ([System.Security.Principal.WindowsPrincipal] `
    [System.Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [System.Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    throw "This script must be run from an elevated (Administrator) PowerShell prompt."
}

$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Lsa {
  [StructLayout(LayoutKind.Sequential)] struct LSA_UNICODE_STRING { public ushort Length, MaximumLength; public IntPtr Buffer; }
  [StructLayout(LayoutKind.Sequential)] struct LSA_OBJECT_ATTRIBUTES { public int Length; public IntPtr RootDirectory, ObjectName; public int Attributes; public IntPtr SecurityDescriptor, SecurityQualityOfService; }
  [DllImport("advapi32.dll", SetLastError=true)] static extern uint LsaOpenPolicy(IntPtr s, ref LSA_OBJECT_ATTRIBUTES o, int a, out IntPtr h);
  [DllImport("advapi32.dll", SetLastError=true)] static extern uint LsaAddAccountRights(IntPtr h, byte[] sid, LSA_UNICODE_STRING[] r, int c);
  [DllImport("advapi32.dll", SetLastError=true)] static extern uint LsaRemoveAccountRights(IntPtr h, byte[] sid, [MarshalAs(UnmanagedType.U1)] bool all, LSA_UNICODE_STRING[] r, int c);
  [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr h);
  [DllImport("advapi32.dll")] static extern int LsaNtStatusToWinError(uint s);
  static IntPtr OpenPolicy() {
    var oa = new LSA_OBJECT_ATTRIBUTES(); oa.Length = Marshal.SizeOf(oa);
    IntPtr h;
    // POLICY_CREATE_ACCOUNT (0x10) | POLICY_LOOKUP_NAMES (0x800)
    uint st = LsaOpenPolicy(IntPtr.Zero, ref oa, 0x00000010 | 0x00000800, out h);
    if (st != 0) throw new Exception("LsaOpenPolicy failed: Win32 error " + LsaNtStatusToWinError(st));
    return h;
  }
  static LSA_UNICODE_STRING[] MakeRight(string right) {
    var us = new LSA_UNICODE_STRING[1];
    us[0].Buffer = Marshal.StringToHGlobalUni(right);
    us[0].Length = (ushort)(right.Length * 2);
    us[0].MaximumLength = (ushort)((right.Length + 1) * 2);
    return us;
  }
  static byte[] SidBytes(string sidStr) {
    var sid = new System.Security.Principal.SecurityIdentifier(sidStr);
    var sidBytes = new byte[sid.BinaryLength]; sid.GetBinaryForm(sidBytes, 0);
    return sidBytes;
  }
  public static void Grant(string sidStr, string right) {
    var sidBytes = SidBytes(sidStr);
    IntPtr h = OpenPolicy();
    var us = MakeRight(right);
    uint st = LsaAddAccountRights(h, sidBytes, us, 1);
    LsaClose(h);
    Marshal.FreeHGlobal(us[0].Buffer);
    if (st != 0) throw new Exception("LsaAddAccountRights failed: Win32 error " + LsaNtStatusToWinError(st));
  }
  public static void Revoke(string sidStr, string right) {
    var sidBytes = SidBytes(sidStr);
    IntPtr h = OpenPolicy();
    var us = MakeRight(right);
    uint st = LsaRemoveAccountRights(h, sidBytes, false, us, 1);
    LsaClose(h);
    Marshal.FreeHGlobal(us[0].Buffer);
    if (st != 0) throw new Exception("LsaRemoveAccountRights failed: Win32 error " + LsaNtStatusToWinError(st));
  }
}
'@

Add-Type -TypeDefinition $sig
if ($Remove) {
    [Lsa]::Revoke($Sid, $Privilege)
    Write-Host "Revoked $Privilege from $Sid"
} else {
    [Lsa]::Grant($Sid, $Privilege)
    Write-Host "Granted $Privilege to $Sid"
}
Write-Host "Sign out and back in for it to take effect, then verify with: whoami /priv"
