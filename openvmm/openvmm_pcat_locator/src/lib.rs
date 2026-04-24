// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper code for finding PCAT binaries.

#![forbid(unsafe_code)]

use anyhow::Context;
use mesh::MeshPayload;
use resource_dll_parser::DllResourceDescriptor;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, MeshPayload)]
/// The location of discovered ROM data.
pub struct RomFileLocation {
    /// The opened file containing the data.
    pub file: File,
    /// The starting byte offset of the data.
    pub start: u64,
    /// The length of the data.
    pub len: usize,
}

/// Returns path to the "Windows\System32" directory.
fn system32_path() -> String {
    // Other approaches could be using the WinDir env. variable,
    // or the unsafe `GetWindowsDirectoryW` function from the `windows` crate.
    let windows_dir = std::env::var("SystemRoot").unwrap_or(String::from(r"C:\Windows"));
    format!("{windows_dir}\\System32")
}

/// Attempt to automatically find and open the PCAT BIOS. Will always prefer
/// the more recently updated vmfirmwarepcat.dll over vmfirmware.dll.
pub fn find_pcat_bios(command_line_path: Option<&Path>) -> anyhow::Result<RomFileLocation> {
    const NAME: &str = "pcat_firmware";
    const BIOS_DESCRIPTOR: DllResourceDescriptor = DllResourceDescriptor::new(b"VMFW", 13500);
    const EXPECTED_BIOS_SIZE: usize = 256 * 1024;

    if let Some(p) = command_line_path {
        parse_rom_file(p, NAME, BIOS_DESCRIPTOR, EXPECTED_BIOS_SIZE)
    } else {
        let system32_path = system32_path();
        // Newer windows hosts have a specific file for pcat firmware.
        let default_pcat_firmware_file = format!(r"{system32_path}\vmfirmwarepcat.dll");

        let result = match parse_rom_file(
            &translate_path(Path::new(default_pcat_firmware_file.as_str()))?,
            NAME,
            BIOS_DESCRIPTOR,
            EXPECTED_BIOS_SIZE,
        ) {
            Ok(r) => r,
            Err(_) => {
                // Older hosts have a single file for both pcat and uefi.
                let legacy_pcat_firmware_file = format!(r"{system32_path}\vmfirmware.dll");

                parse_rom_file(
                    &translate_path(Path::new(legacy_pcat_firmware_file.as_str()))?,
                    NAME,
                    BIOS_DESCRIPTOR,
                    EXPECTED_BIOS_SIZE,
                )?
            }
        };
        Ok(result)
    }
}

/// Attempt to automatically find and open the SVGA video device BIOS from
/// vmemulateddevices.dll.
pub fn find_svga_bios(command_line_path: Option<&Path>) -> anyhow::Result<RomFileLocation> {
    const SVGA_BIOS_DESCRIPTOR: DllResourceDescriptor = DllResourceDescriptor::new(b"BIOS", 13501);
    const NAME: &str = "vga_firmware";
    const EXPECTED_SIZE: usize = 48 * 1024;

    if let Some(p) = command_line_path {
        parse_rom_file(p, NAME, SVGA_BIOS_DESCRIPTOR, EXPECTED_SIZE)
    } else {
        let system32_path = system32_path();
        // TODO: Also load the splash screen from the same dll?
        let default_svga_firmware_file = format!(r"{system32_path}\vmemulateddevices.dll");

        parse_rom_file(
            &translate_path(Path::new(default_svga_firmware_file.as_str()))?,
            NAME,
            SVGA_BIOS_DESCRIPTOR,
            EXPECTED_SIZE,
        )
    }
}

/// Translate a Windows path to an OS-appropriate path.
fn translate_path(path: &Path) -> anyhow::Result<PathBuf> {
    let file_path = if cfg!(windows) {
        path.into()
    } else if cfg!(target_os = "linux") {
        // WSL
        let output = Command::new("wslpath")
            .arg(path)
            .output()
            .context("Failed to translate path to windows. Are you not on WSL?")?;

        String::from_utf8_lossy(&output.stdout).trim().into()
    } else {
        anyhow::bail!("No path specified for firmware and no default is configured for this OS.")
    };

    Ok(file_path)
}

/// Reads the ROM data from the given file and returns it.
/// Autodetects if the file is a dll or not, and attempts to parse appropriately.
fn parse_rom_file(
    file_path: &Path,
    firmware_name: &str,
    descriptor: DllResourceDescriptor,
    expected_len: usize,
) -> anyhow::Result<RomFileLocation> {
    tracing::debug!(
        ?file_path,
        ?firmware_name,
        "Attempting to load firmware file."
    );

    let file = fs_err::File::open(file_path)?;

    let (start, len) = if let Some(maybe_resource) =
        resource_dll_parser::try_find_resource_from_dll(&file, &descriptor)?
    {
        maybe_resource
    } else {
        (0, file.metadata()?.len() as usize)
    };

    if len != expected_len {
        tracing::warn!(
            firmware_name,
            len,
            expected_len,
            "ROM data length does not match expected length, trying anyways",
        );
    }

    Ok(RomFileLocation {
        file: file.into(),
        start,
        len,
    })
}
