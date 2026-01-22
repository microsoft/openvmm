// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build script that compiles the Windows RPC server for test IGVM agent.

use serde_json::Value;
use std::env;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

struct CrossConfig {
    include: Vec<PathBuf>,
    lib: Vec<PathBuf>,
    bin_dirs: Vec<PathBuf>,
}

fn main() {
    println!("cargo:rerun-if-changed=idl/IGVmAgentRpcApi.idl");
    println!("cargo:rerun-if-env-changed=MIDL");

    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        // Stub interface is only needed when targeting Windows.
        return;
    }

    let host = env::var("HOST").unwrap_or_default();
    let host_is_windows = host.contains("windows");

    // Construct the version of MIDL to use based on host architecture but windows target.
    let host_arch = host
        .split_once("-")
        .expect("HOST target triple should contain hyphen separating architecture and vendor")
        .0;
    let midl_env = format!("{}_pc_windows_msvc", host_arch);
    println!("cargo:rerun-if-env-changed=MIDLRT_{}", midl_env);
    let midl_info = locate_midl(&midl_env);

    if midl_info.is_none() {
        if host_is_windows {
            panic!(
                "MIDL compiler not found. Please install the Windows SDK which includes MIDL, \
                or set the MIDL environment variable to point to midl.exe. \
                You can download the Windows SDK from: \
                https://developer.microsoft.com/en-us/windows/downloads/windows-sdk/"
            );
        } else {
            panic!(
                "MIDL compiler is required to build for Windows targets from non-Windows hosts. \
                Set MIDL or MIDLRT_{} environment variable to point to a cross-compilation MIDL tool.",
                midl_env
            );
        }
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let idl_path = Path::new("idl/IGVmAgentRpcApi.idl");

    let (midl, uses_cross_tool) = midl_info.unwrap_or_else(|| ("midl".to_owned(), false));
    let mut cmd = Command::new(&midl);
    cmd.arg("/nologo");

    let cross_cfg = if !host_is_windows {
        match load_cross_config() {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                panic!("Failed to load cross-compilation configuration: {err}");
            }
        }
    } else {
        None
    };

    match (uses_cross_tool, cross_cfg.as_ref()) {
        (true, Some(cfg)) => {
            if let Some(cl_path) = locate_cl_exe(cfg) {
                if let Some(bin_dir) = cl_path.parent() {
                    let mut path_value = env::var_os("PATH").unwrap_or_default();
                    if !path_value.is_empty() {
                        path_value.push(":");
                    }
                    path_value.push(bin_dir.as_os_str());
                    cmd.env("PATH", &path_value);

                    let mut wslenv = env::var("WSLENV").unwrap_or_default();
                    if !wslenv.split(':').any(|entry| entry == "PATH/wl") {
                        if !wslenv.is_empty() {
                            wslenv.push(':');
                        }
                        wslenv.push_str("PATH/wl");
                    }
                    cmd.env("WSLENV", wslenv);
                } else {
                    panic!(
                        "Missing parent directory for cl.exe in cross-compilation configuration"
                    );
                }
            } else {
                panic!("Unable to locate cl.exe in cross-compilation configuration");
            }
        }
        (false, Some(cfg)) => {
            if let Err(err) = configure_cross_env(&mut cmd, cfg) {
                panic!("Failed to configure cross-compilation environment: {err}");
            }
        }
        _ => {
            // When building on native Windows, set up the MSVC environment for MIDL
            if host_is_windows {
                if let Err(err) = setup_msvc_env_for_midl(&mut cmd) {
                    panic!("Failed to set up MSVC environment for MIDL: {err}");
                }
            }
        }
    }

    // Determine the MIDL target environment based on the Cargo target.
    // xtask-fmt allow-target-arch dependency
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_owned());
    let midl_env = match arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        unsupported => panic!("Unsupported architecture for MIDL: {}", unsupported),
    };
    cmd.args(["/env", midl_env]);

    let out_dir_arg = path_for_midl(&out_dir, host_is_windows);
    cmd.arg("/out");
    cmd.arg(&out_dir_arg);

    let idl_arg = path_for_midl(idl_path, host_is_windows);
    cmd.arg(&idl_arg);

    let status = cmd.status().unwrap_or_else(|err| {
        panic!("Failed to execute MIDL `{midl}`: {err}. Install the Windows MIDL compiler.")
    });
    if !status.success() {
        panic!("midl failed: status {status}");
    }

    let stub_c = out_dir.join("IGVmAgentRpcApi_s.c");

    if !stub_c.exists() {
        panic!("MIDL did not produce expected stub: {}", stub_c.display());
    }

    cc::Build::new()
        .file(&stub_c)
        .include(&out_dir)
        .compile("igvm_agent_rpc_stub");

    println!("cargo:rustc-link-lib=Rpcrt4");
}

fn locate_midl(target_env: &str) -> Option<(String, bool)> {
    // Check for cross-compilation MIDL tool first
    let key = format!("MIDLRT_{}", target_env);
    if let Ok(path) = env::var(&key) {
        if !path.is_empty() {
            return Some((path, true));
        }
    }

    if let Ok(path) = env::var("MIDL") {
        if !path.is_empty() {
            return Some((path, false));
        }
    }

    // On Windows, try to find MIDL from the Windows SDK
    #[cfg(windows)]
    {
        if let Some(midl_path) = find_windows_sdk_midl() {
            return Some((midl_path, false));
        }
    }

    None
}

#[cfg(windows)]
fn find_windows_sdk_midl() -> Option<String> {
    use std::process::Command;

    // Try common Windows SDK paths directly
    // The SDK can be installed standalone without Visual Studio
    let sdk_dirs = [
        "C:\\Program Files (x86)\\Windows Kits\\10\\bin",
        "C:\\Program Files\\Windows Kits\\10\\bin",
    ];

    for sdk_dir in &sdk_dirs {
        // Try to find the latest SDK version
        if let Ok(entries) = std::fs::read_dir(sdk_dir) {
            let mut versions: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| {
                    let name = e.file_name();
                    let name_str = name.to_string_lossy();
                    // Check if it looks like a version number (starts with digit)
                    if name_str.chars().next()?.is_ascii_digit() {
                        Some((name_str.to_string(), e.path()))
                    } else {
                        None
                    }
                })
                .collect();

            // Sort versions in reverse order to get the latest
            versions.sort_by(|a, b| b.0.cmp(&a.0));

            for (_, version_path) in versions {
                // Check both x64 and x86 subdirectories
                for arch_subdir in &["x64", "x86"] {
                    let midl_path = version_path.join(arch_subdir).join("midl.exe");
                    if midl_path.exists() {
                        return Some(midl_path.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    // Fallback: check if "midl" is in PATH
    if Command::new("midl").arg("/?").output().is_ok() {
        return Some("midl".to_string());
    }

    None
}

#[cfg(windows)]
fn setup_msvc_env_for_midl(cmd: &mut Command) -> Result<(), String> {
    // Use the cc crate to get the MSVC compiler tool, which will give us
    // access to the properly configured environment including cl.exe path
    let tool = cc::Build::new().get_compiler();

    // Get the path to cl.exe
    let cl_path = tool.path();
    if let Some(bin_dir) = cl_path.parent() {
        // Add the MSVC bin directory to PATH so MIDL can find cl.exe
        let mut path_value = env::var_os("PATH").unwrap_or_default();
        if !path_value.is_empty() {
            path_value.push(";");
        }
        path_value.push(bin_dir.as_os_str());
        cmd.env("PATH", &path_value);

        // Also set up INCLUDE and LIB environment variables that MIDL/cl.exe need
        for (key, value) in tool.env() {
            cmd.env(key, value);
        }
    } else {
        return Err("Failed to find parent directory of cl.exe".to_string());
    }

    Ok(())
}

#[cfg(not(windows))]
fn setup_msvc_env_for_midl(_cmd: &mut Command) -> Result<(), String> {
    Ok(())
}

fn configure_cross_env(cmd: &mut Command, cfg: &CrossConfig) -> Result<(), String> {
    let include = join_windows_paths(&cfg.include)?;
    let lib = join_windows_paths(&cfg.lib)?;
    let mut path_entries = join_windows_paths(&cfg.bin_dirs)?;

    if let Some(existing) = env::var_os("PATH") {
        if !existing.is_empty() {
            if !path_entries.is_empty() {
                path_entries.push(";");
            }
            path_entries.push(existing);
        }
    }

    cmd.env("INCLUDE", &include);
    cmd.env("LIB", &lib);
    cmd.env("PATH", &path_entries);

    Ok(())
}

fn load_cross_config() -> Result<CrossConfig, String> {
    let arch = env::var("CARGO_CFG_TARGET_ARCH")
        .map_err(|_| "CARGO_CFG_TARGET_ARCH not set".to_string())?;
    let tool = env::var_os("OPENVMM_WINDOWS_CROSS_TOOL")
        .ok_or_else(|| "OPENVMM_WINDOWS_CROSS_TOOL not set".to_string())?;

    let output = Command::new(&tool)
        .arg("--arch")
        .arg(&arch)
        .arg("--dump")
        .output()
        .map_err(|err| format!("failed to run cross tool `{tool:?}`: {err}"))?;

    if !output.status.success() {
        return Err("cross tool did not return success".to_string());
    }

    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("failed to parse cross tool output: {err}"))?;

    let include = extract_paths(&value, "include")?;
    let lib = extract_paths(&value, "lib")?;
    let mut bin_dirs = extract_paths(&value, "sdk")?;

    if let Some(msvc_bin) = msvc_bin_dir(&include, &arch) {
        bin_dirs.push(msvc_bin);
    }

    Ok(CrossConfig {
        include,
        lib,
        bin_dirs,
    })
}

fn extract_paths(value: &Value, key: &str) -> Result<Vec<PathBuf>, String> {
    let array = value
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("missing `{key}` array in cross tool output"))?;

    let mut paths = Vec::new();
    for entry in array {
        let path = entry
            .as_str()
            .ok_or_else(|| format!("invalid `{key}` entry in cross tool output"))?;
        paths.push(PathBuf::from(path));
    }

    if paths.is_empty() {
        return Err(format!("no entries found for `{key}`"));
    }

    Ok(paths)
}

fn msvc_bin_dir(include_paths: &[PathBuf], target_arch: &str) -> Option<PathBuf> {
    for include in include_paths {
        let mut base = include.parent()?.to_path_buf();
        base.push("bin");
        base.push("Hostx64");
        base.push(match target_arch {
            "aarch64" | "arm64" => "arm64",
            _ => "x64",
        });

        if base.join("clang-cl.exe").exists() || base.join("cl.exe").exists() {
            return Some(base);
        }
    }

    None
}

fn locate_cl_exe(cfg: &CrossConfig) -> Option<PathBuf> {
    for dir in &cfg.bin_dirs {
        let candidate = dir.join("cl.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

fn join_windows_paths(paths: &[PathBuf]) -> Result<OsString, String> {
    let mut parts = Vec::with_capacity(paths.len());
    for path in paths {
        let converted = path_for_midl(path, false);
        let piece = converted.to_string_lossy().into_owned();
        if piece.is_empty() {
            return Err(format!(
                "unable to convert path `{}` to Windows format",
                path.display()
            ));
        }
        parts.push(piece);
    }

    Ok(OsString::from(parts.join(";")))
}

fn path_for_midl(path: &Path, host_is_windows: bool) -> OsString {
    if host_is_windows {
        return path.as_os_str().to_os_string();
    }

    if matches!(path.components().next(), Some(Component::Prefix(_))) {
        return path.as_os_str().to_os_string();
    }

    match Command::new("wslpath").arg("-w").arg(path).output() {
        Ok(output) if output.status.success() => {
            let converted = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if converted.is_empty() {
                path.as_os_str().to_os_string()
            } else {
                OsString::from(converted)
            }
        }
        _ => path.as_os_str().to_os_string(),
    }
}
