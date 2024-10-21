// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod offreg;

use self::offreg::Hive;
use anyhow::Context;

pub(crate) fn main() -> anyhow::Result<()> {
    let path = std::env::args_os().nth(1).context("missing path")?;
    let hive = Hive::create()?;
    {
        let mut key;
        let mut parent = hive.as_ref();
        for subkey in ["SYSTEM", "CurrentControlSet", "Services", "pipette"] {
            let new_key = parent.create_key(subkey)?;
            key = new_key;
            parent = key.as_ref();
        }

        parent.set_dword("Type", 0x10)?; // win32 service
        parent.set_dword("Start", 2)?; // auto start
        parent.set_dword("ErrorControl", 1)?; // normal
        parent.set_sz("ImagePath", "D:\\pipette.exe --service")?;
        parent.set_sz("DisplayName", "Petri pipette agent")?;
        parent.set_sz("ObjectName", "LocalSystem")?;
        parent.set_multi_sz("DependOnService", ["RpcSs"])?;
    }

    // Windows defaults to 1, so we need to set it to 2 to cause Windows to
    // apply the IMC changes on first boot.
    hive.set_dword("Sequence", 2)?;

    let _ = std::fs::remove_file(&path);
    hive.save(path.as_ref())?;
    Ok(())
}
