// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod offreg;

use self::offreg::Hive;
use crate::windows::offreg::OwnedKey;
use anyhow::Context;

pub(crate) fn main() -> anyhow::Result<()> {
    let path = std::env::args_os().nth(1).context("missing path")?;
    let hive = Hive::create()?;

    // Create the pipette service and set it to auto start
    let service_key = create_subkeys(
        &hive,
        &["SYSTEM", "CurrentControlSet", "Services", "pipette"],
    )?;
    service_key.set_dword("Type", 0x10)?; // win32 service
    service_key.set_dword("Start", 2)?; // auto start
    service_key.set_dword("ErrorControl", 1)?; // normal
    service_key.set_sz("ImagePath", "D:\\pipette.exe --service")?;
    service_key.set_sz("DisplayName", "Petri pipette agent")?;
    service_key.set_sz("ObjectName", "LocalSystem")?;
    service_key.set_multi_sz("DependOnService", ["RpcSs"])?;

    // Allow VMBus devices when isolated - namely pipette
    let vmbus_key = create_subkeys(
        &hive,
        &[
            "SYSTEM",
            "CurrentControlSet",
            "Services",
            "VMBus",
            "Parameters",
        ],
    )?;
    vmbus_key.set_dword("AllowAllDevicesWhenIsolated", 1)?;

    // Enable kernel mode crash dumps
    let crash_control_key = create_subkeys(
        &hive,
        &["SYSTEM", "CurrentControlSet", "Control", "CrashControl"],
    )?;
    crash_control_key.set_dword("CrashDumpEnabled", 2)?;
    crash_control_key.set_expand_sz("DumpFile", "E:\\memory.dmp")?;

    // Enable user mode crash dumps
    let wer_key = create_subkeys(
        &hive,
        &[
            "SOFTWARE",
            "Microsoft",
            "Windows",
            "Windows Error Reporting",
            "LocalDumps",
        ],
    )?;
    wer_key.set_dword("DumpType", 2)?;
    wer_key.set_expand_sz("DumpFolder", "E:\\")?;

    // Windows defaults this to 1, so we need to set it to 2 to cause Windows
    // to apply the IMC changes on first boot.
    hive.set_dword("Sequence", 2)?;

    let _ = std::fs::remove_file(&path);
    hive.save(path.as_ref())?;
    Ok(())
}

fn create_subkeys(hive: &Hive, path: &[&str]) -> anyhow::Result<OwnedKey> {
    let mut parent = hive.as_ref();
    let mut key = parent.create_key(path[0])?;
    parent = key.as_ref();
    for subkey in &path[1..] {
        key = parent.create_key(subkey)?;
        parent = key.as_ref();
    }
    Ok(key)
}
