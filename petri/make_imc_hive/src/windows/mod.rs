// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod offreg;

use self::offreg::Hive;
use anyhow::Context;
use offreg::OwnedKey;

pub(crate) fn main() -> anyhow::Result<()> {
    let ty = std::env::args().nth(1).context("missing type")?;
    let path = std::env::args_os().nth(2).context("missing path")?;
    let hive = Hive::create()?;

    match &*ty {
        "pipette" => fill_hive_pipette(&hive)?,
        // TODO: Once we have support for running pipette with VSM, also call
        // fill_hive_pipette here.
        "vsm" => fill_hive_vsm(&hive)?,
        _ => anyhow::bail!("unknown type"),
    }

    // Windows defaults to 1, so we need to set it to 2 to cause Windows to
    // apply the IMC changes on first boot.
    hive.set_dword("Sequence", 2)?;

    let _ = std::fs::remove_file(&path);
    hive.save(path.as_ref())?;
    Ok(())
}

fn subkey(hive: &Hive, path: &str) -> anyhow::Result<OwnedKey> {
    let mut key = None;
    let mut parent = hive.as_ref();
    for subkey in path.split('\\') {
        let new_key = parent.create_key(subkey)?;
        key = Some(new_key);
        parent = key.as_ref().unwrap();
    }
    Ok(key.unwrap())
}

/// Insert the pipette startup keys into the hive.
fn fill_hive_pipette(hive: &Hive) -> anyhow::Result<()> {
    let svc_key = subkey(hive, r"SYSTEM\CurrentControlSet\Services\pipette")?;
    svc_key.set_dword("Type", 0x10)?; // win32 service
    svc_key.set_dword("Start", 2)?; // auto start
    svc_key.set_dword("ErrorControl", 1)?; // normal
    svc_key.set_sz("ImagePath", "D:\\pipette.exe --service")?;
    svc_key.set_sz("DisplayName", "Petri pipette agent")?;
    svc_key.set_sz("ObjectName", "LocalSystem")?;
    svc_key.set_multi_sz("DependOnService", ["RpcSs"])?;
    Ok(())
}

fn fill_hive_vsm(hive: &Hive) -> anyhow::Result<()> {
    // Enable VBS
    let vbs_key = subkey(hive, r"SYSTEM\CurrentControlSet\Control\DeviceGuard")?;
    vbs_key.set_dword("EnableVirtualizationBasedSecurity", 1)?;

    // Enable Credential Guard - https://learn.microsoft.com/en-us/windows/security/identity-protection/credential-guard/configure?tabs=reg
    let cg_key = subkey(hive, r"SYSTEM\CurrentControlSet\Control\Lsa")?;
    cg_key.set_dword("LsaCfgFlags", 2)?;

    // Enable HVCI - https://learn.microsoft.com/en-us/windows/security/hardware-security/enable-virtualization-based-protection-of-code-integrity?tabs=reg
    let hvci_key = subkey(
        hive,
        r"SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity",
    )?;
    hvci_key.set_dword("Enabled", 1)?;

    Ok(())
}
