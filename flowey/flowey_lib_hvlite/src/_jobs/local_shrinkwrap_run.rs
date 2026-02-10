// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

flowey_request! {
    /// Parameters for modifying rootfs.ext2 and running shrinkwrap.
    pub struct Params {
        /// Output directory where shrinkwrap build artifacts are located
        pub out_dir: PathBuf,
        /// Directory where shrinkwrap repo is cloned
        pub shrinkwrap_dir: PathBuf,
        /// Platform YAML file for shrinkwrap run
        pub platform_yaml: PathBuf,
        /// Path to rootfs.ext2 file
        pub rootfs_path: PathBuf,
        /// Runtime variables for shrinkwrap run (e.g., "ROOTFS=/path/to/rootfs.ext2")
        pub rtvars: Vec<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            out_dir,
            shrinkwrap_dir,
            platform_yaml,
            rootfs_path,
            rtvars,
            done,
        } = request;

        ctx.emit_rust_step("modify rootfs.ext2", |ctx| {
            done.claim(ctx);
            move |_rt| {
                // Compute paths the same way as install job
                // Get the parent directory (toolchain_dir) where everything is built
                let toolchain_dir = shrinkwrap_dir.parent()
                    .ok_or_else(|| anyhow::anyhow!("shrinkwrap_dir has no parent"))?;

                let tmk_kernel_dir = toolchain_dir.join("OpenVMM-TMK");
                let host_kernel_dir = toolchain_dir.join("OHCL-Linux-Kernel");

                let simple_tmk = tmk_kernel_dir.join("target/aarch64-minimal_rt-none/debug/simple_tmk");
                let tmk_vmm = tmk_kernel_dir.join("target/aarch64-unknown-linux-gnu/debug/tmk_vmm");
                let kernel_image_path = host_kernel_dir.join("arch/arm64/boot/Image");

                // Modify rootfs.ext2 to inject TMK binaries and kernel
                log::info!("Starting rootfs.ext2 modification...");

                // Use the rootfs path provided by the user command
                let rootfs_ext2 = rootfs_path;

                if !rootfs_ext2.exists() {
                    anyhow::bail!("rootfs.ext2 not found at {}", rootfs_ext2.display());
                }

                log::info!("Found rootfs.ext2 at {}", rootfs_ext2.display());

                // Get the directory containing rootfs.ext2 for docker mounting
                let rootfs_dir = rootfs_ext2.parent()
                    .ok_or_else(|| anyhow::anyhow!("rootfs.ext2 has no parent directory"))?;
                let rootfs_filename = rootfs_ext2.file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid rootfs path"))?
                    .to_string_lossy();

                // Step 1: Run e2fsck to check filesystem
                log::info!("Running e2fsck on rootfs.ext2...");
                let e2fsck_status = Command::new("docker")
                    .args(&["run", "--rm", "-v"])
                    .arg(format!("{}:{}", rootfs_dir.display(), rootfs_dir.display()))
                    .args(&["-w", &rootfs_dir.to_string_lossy()])
                    .args(&["ubuntu:24.04", "bash", "-lc"])
                    .arg(format!("apt-get update && apt-get install -y e2fsprogs && e2fsck -fp {}", rootfs_filename))
                    .status();

                match e2fsck_status {
                    Ok(status) if status.success() => log::info!("e2fsck completed successfully"),
                    Ok(status) => log::warn!("e2fsck exited with status: {}", status),
                    Err(e) => anyhow::bail!("Failed to run e2fsck: {}", e),
                }

                // Step 2: Resize the filesystem
                log::info!("Resizing rootfs.ext2 to 1024M...");
                let resize_status = Command::new("docker")
                    .args(&["run", "--rm", "-v"])
                    .arg(format!("{}:{}", rootfs_dir.display(), rootfs_dir.display()))
                    .args(&["-w", &rootfs_dir.to_string_lossy()])
                    .args(&["ubuntu:24.04", "bash", "-lc"])
                    .arg(format!("apt-get update && apt-get install -y e2fsprogs && e2fsck -fp {} && resize2fs {} 1024M", rootfs_filename, rootfs_filename))
                    .status();

                match resize_status {
                    Ok(status) if status.success() => log::info!("resize2fs completed successfully"),
                    Ok(status) => log::warn!("resize2fs exited with status: {}", status),
                    Err(e) => anyhow::bail!("Failed to run resize2fs: {}", e),
                }

                // Step 3: Mount rootfs, inject files, and unmount
                log::info!("Mounting rootfs.ext2 and injecting TMK binaries...");

                // Use paths from parameters
                log::info!("Using simple_tmk from: {}", simple_tmk.display());
                log::info!("Using tmk_vmm from: {}", tmk_vmm.display());
                log::info!("Using kernel Image from: {}", kernel_image_path.display());

                // Same directory as rootfs.ext2
                let guest_disk = rootfs_dir.join("guest-disk.img");
                let kvmtool_efi = rootfs_dir.join("KVMTOOL_EFI.fd");
                let lkvm = rootfs_dir.join("lkvm");

                // Copy kernel to Image_ohcl
                let image_ohcl = rootfs_dir.join("Image_ohcl");
                if kernel_image_path.exists() {
                    fs::copy(&kernel_image_path, &image_ohcl)
                        .map_err(|e| anyhow::anyhow!("Failed to copy kernel Image: {}", e))?;
                    log::info!("Copied kernel to Image_ohcl");
                } else {
                    log::warn!("Kernel image not found at {}", kernel_image_path.display());
                }

                // Build the mount/inject script
                let mount_script = format!(
                    r#"
                    set -e
                    mkdir -p mnt
                    mount {rootfs_filename} mnt
                    mkdir -p mnt/cca
                    {simple_tmk_copy}
                    {tmk_vmm_copy}
                    {guest_disk_copy}
                    {kvmtool_efi_copy}
                    {image_ohcl_copy}
                    {lkvm_copy}
                    sync
                    umount mnt || umount -l mnt || true
                    sync
                    sleep 1
                    # Try multiple times to remove the directory
                    for i in 1 2 3 4 5; do
                        if [ -d mnt ]; then
                            rmdir mnt 2>/dev/null && break || sleep 0.5
                        else
                            break
                        fi
                    done
                    # If still exists, force remove
                    [ -d mnt ] && rm -rf mnt || true
                    "#,
                    rootfs_filename = rootfs_filename,
                    simple_tmk_copy = if simple_tmk.exists() {
                        format!("cp {} mnt/cca/", simple_tmk.display())
                    } else {
                        format!("echo 'Warning: {} not found'", simple_tmk.display())
                    },
                    tmk_vmm_copy = if tmk_vmm.exists() {
                        format!("cp {} mnt/cca/", tmk_vmm.display())
                    } else {
                        format!("echo 'Warning: {} not found'", tmk_vmm.display())
                    },
                    guest_disk_copy = if guest_disk.exists() {
                        format!("cp {} mnt/cca/", guest_disk.display())
                    } else {
                        "".to_string()
                    },
                    kvmtool_efi_copy = if kvmtool_efi.exists() {
                        format!("cp {} mnt/cca/", kvmtool_efi.display())
                    } else {
                        "".to_string()
                    },
                    image_ohcl_copy = if image_ohcl.exists() {
                        format!("cp {} mnt/cca/", image_ohcl.display())
                    } else {
                        "".to_string()
                    },
                    lkvm_copy = if lkvm.exists() {
                        format!("cp {} mnt/cca/", lkvm.display())
                    } else {
                        "".to_string()
                    },
                );

                let mount_status = Command::new("sudo")
                    .arg("bash")
                    .arg("-c")
                    .arg(&mount_script)
                    .current_dir(rootfs_dir)
                    .status();

                match mount_status {
                    Ok(status) if status.success() => {
                        log::info!("rootfs.ext2 updated successfully with TMK binaries");
                    }
                    Ok(status) => {
                        anyhow::bail!("Failed to mount/inject files: exit status {}", status);
                    }
                    Err(e) => {
                        anyhow::bail!("Failed to execute mount script: {}", e);
                    }
                }

                // Step 4: Run shrinkwrap with the modified rootfs
                log::info!("Running shrinkwrap with platform YAML: {}", platform_yaml.display());

                // Get the canonical path to rootfs.ext2
                let rootfs_canonical = fs::canonicalize(&rootfs_ext2)
                    .map_err(|e| anyhow::anyhow!("Failed to canonicalize rootfs path: {}", e))?;

                // Prepare shrinkwrap command
                let shrinkwrap_exe = shrinkwrap_dir.join("shrinkwrap").join("shrinkwrap");
                let venv_dir = shrinkwrap_dir.join("venv");

                if !shrinkwrap_exe.exists() {
                    anyhow::bail!("shrinkwrap executable not found at {}", shrinkwrap_exe.display());
                }

                // Determine the platform YAML path to use
                // If platform_yaml is absolute, try to make it relative to out_dir
                // Otherwise, shrinkwrap will look for artifacts relative to the YAML location
                let platform_yaml_to_use = if platform_yaml.is_absolute() {
                    // Try to use just the filename - shrinkwrap should have copied/processed it
                    platform_yaml.file_name()
                        .map(|name| PathBuf::from(name))
                        .unwrap_or_else(|| platform_yaml.clone())
                } else {
                    platform_yaml.clone()
                };

                log::info!("Using platform YAML: {} (relative to {})",
                    platform_yaml_to_use.display(),
                    out_dir.display());

                // Build the rtvar arguments
                let mut rtvar_args = Vec::new();

                // Add the ROOTFS rtvar pointing to the modified rootfs.ext2
                rtvar_args.push("--rtvar".to_string());
                rtvar_args.push(format!("ROOTFS={}", rootfs_canonical.display()));

                // Add any additional rtvars from parameters
                for rtvar in rtvars {
                    rtvar_args.push("--rtvar".to_string());
                    rtvar_args.push(rtvar);
                }

                log::info!("Running: {} run {} {}",
                    shrinkwrap_exe.display(),
                    platform_yaml_to_use.display(),
                    rtvar_args.join(" "));

                // Set environment to use venv Python
                let venv_bin = venv_dir.join("bin");

                log::info!("Setting VIRTUAL_ENV={}", venv_dir.display());

                let shrinkwrap_run_status = Command::new(&shrinkwrap_exe)
                    .arg("run")
                    .arg(&platform_yaml_to_use)
                    .args(&rtvar_args)
                    .env("VIRTUAL_ENV", &venv_dir)
                    .env("PATH", format!("{}:{}",
                        venv_bin.display(),
                        std::env::var("PATH").unwrap_or_default()
                    ))
                    .current_dir(&out_dir)  // Run from out_dir where build artifacts are
                    .status();

                match shrinkwrap_run_status {
                    Ok(status) if status.success() => {
                        log::info!("Shrinkwrap run completed successfully");
                    }
                    Ok(status) => {
                        anyhow::bail!("Shrinkwrap run failed with exit status: {}", status);
                    }
                    Err(e) => {
                        anyhow::bail!("Failed to execute shrinkwrap run: {}", e);
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
