// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for building import libs from .def files.


use anyhow::Context;
use std::process::Command;

/// Makes an import lib for `name`.dll, where the .def file is `name`.def.
pub fn build_import_lib(name: &str) -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed={}.def", name);

    let import_lib_path = format!("{}/import_libs", std::env::var("OUT_DIR").unwrap());
    std::fs::create_dir_all(&import_lib_path).context("failed to create import_libs dir")?;
    println!("cargo:rustc-link-search={import_lib_path}");

    if let Some(dlltool) = get_tool_var("DLLTOOL") {
        let mut dlltool = Command::new(dlltool);

        // xtask-fmt allow-target-arch sys-crate
        let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
            "x86" => "i386",
            "x86_64" => "i386:x86-64",
            "aarch64" => "arm64",
            a => panic!("unsupported architecture {}", a),
        };

        dlltool.args(["-d", &format!("{}.def", name)]);
        dlltool.args(["-m", arch]);
        dlltool.args(["-l", &format!("{import_lib_path}/{name}.lib")]);
        if !dlltool
            .spawn()
            .context("failed to spawn dlltool")?
            .wait()
            .context("failed to wait for dlltool")?
            .success()
        {
            anyhow::bail!("dlltool failed");
        }
    } else {
        let mut lib = match get_tool_var("AR") {
            Some(path) => Command::new(path),
            None => cc::windows_registry::find(&std::env::var("TARGET").unwrap(), "lib.exe")
                .context("cannot find lib.exe")?,
        };

        // xtask-fmt allow-target-arch sys-crate
        let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
            "x86" => "X86",
            "x86_64" => "X64",
            "aarch64" => "ARM64",
            a => anyhow::bail!("unsupported architecture {a}"),
        };

        lib.arg(format!("/machine:{}", arch));
        lib.arg(format!("/def:{}.def", name));
        lib.arg(format!("/out:{import_lib_path}/{name}.lib"));
        if !lib
            .spawn()
            .context("failed to spawn lib.exe")?
            .wait()
            .context("failed to wait for lib.exe")?
            .success()
        {
            anyhow::bail!("lib.exe failed");
        }
    }
    Ok(())
}

fn get_tool_var(name: &str) -> Option<String> {
    let target = std::env::var("TARGET").unwrap().replace('-', "_");
    let var = format!("{}_{}", name, target);
    println!("cargo:rerun-if-env-changed={}", var);
    std::env::var(var)
        .or_else(|_| {
            println!("cargo:rerun-if-env-changed={}", name);
            std::env::var(name)
        })
        .ok()
}
