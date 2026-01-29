// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install Shrinkwrap and its dependencies on Ubuntu.

use flowey::node::prelude::*;

const ARM_GNU_TOOLCHAIN_URL: &str = "https://developer.arm.com/-/media/Files/downloads/gnu/14.3.rel1/binrel/arm-gnu-toolchain-14.3.rel1-x86_64-aarch64-none-elf.tar.xz";
const OHCL_LINUX_KERNEL_REPO: &str = "https://github.com/weiding-msft/OHCL-Linux-Kernel.git";
const OHCL_LINUX_KERNEL_PLANE0_BRANCH: &str = "with-arm-rebased-planes";
const OPENVMM_TMK_REPO: &str = "https://github.com/Flgodd67/openvmm.git";
const OPENVMM_TMK_BRANCH: &str = "cca-enablement";
const SHRINKWRAP_REPO: &str = "https://git.gitlab.arm.com/tooling/shrinkwrap.git";

flowey_request! {
    pub struct Params {
        /// Directory where shrinkwrap repo will be cloned (e.g. <out_dir>/shrinkwrap)
        pub shrinkwrap_dir: PathBuf,
        /// If true, run apt-get and pip installs (requires sudo).
        /// If false, only clones repo and writes instructions.
        pub do_installs: bool,
        /// If true, run `git pull --ff-only` if the repo already exists.
        pub update_repo: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            shrinkwrap_dir,
            do_installs,
            update_repo,
            done,
        } = request;

        ctx.emit_rust_step("install shrinkwrap", |ctx| {
            done.claim(ctx);
            move |_rt| {
                let sh = xshell::Shell::new()?;

                // 0) Create parent dir
                if let Some(parent) = shrinkwrap_dir.parent() {
                    fs_err::create_dir_all(parent)?;
                }

                // 1) System deps (Ubuntu)
                if do_installs {
                    log::info!("Installing system dependencies...");
                    xshell::cmd!(sh, "sudo apt-get update").run()?;
                    xshell::cmd!(sh, "sudo apt-get install -y build-essential flex bison libssl-dev libelf-dev bc git netcat-openbsd python3 python3-pip python3-venv telnet docker.io unzip").run()?;

                    // Setup Docker group and add current user
                    log::info!("Setting up Docker group...");
                    let username = std::env::var("USER").unwrap_or_else(|_| "vscode".to_string());

                    // Create docker group (ignore error if it already exists)
                    let _ = xshell::cmd!(sh, "sudo groupadd docker").run();

                    // Add user to docker group
                    xshell::cmd!(sh, "sudo usermod -aG docker {username}").run()?;

                    log::warn!("Docker group membership updated. You may need to log out and log back in for docker permissions to take effect.");
                    log::warn!("Alternatively, run: newgrp docker");
                }

                // 2) Download and extract ARM GNU toolchain for Host linux kernel compilation
                let toolchain_dir = shrinkwrap_dir.parent()
                    .ok_or_else(|| anyhow::anyhow!("shrinkwrap_dir has no parent"))?;
                let toolchain_archive = toolchain_dir.join("arm-gnu-toolchain-14.3.rel1-x86_64-aarch64-none-elf.tar.xz");
                let toolchain_extracted_dir = toolchain_dir.join("arm-gnu-toolchain-14.3.rel1-x86_64-aarch64-none-elf");

                // Download toolchain if not present
                if !toolchain_archive.exists() {
                    log::info!("Downloading ARM GNU toolchain to {}", toolchain_archive.display());
                    xshell::cmd!(sh, "wget -O").arg(&toolchain_archive).arg(ARM_GNU_TOOLCHAIN_URL).run()?;
                    log::info!("ARM GNU toolchain downloaded successfully");
                } else {
                    log::info!("ARM GNU toolchain already exists at {}", toolchain_archive.display());
                }

                // Extract toolchain if not already extracted
                if !toolchain_extracted_dir.exists() {
                    log::info!("Extracting ARM GNU toolchain to {}", toolchain_dir.display());
                    sh.change_dir(toolchain_dir);
                    xshell::cmd!(sh, "tar -xvf").arg(&toolchain_archive).run()?;
                    log::info!("ARM GNU toolchain extracted successfully");
                } else {
                    log::info!("ARM GNU toolchain already extracted at {}", toolchain_extracted_dir.display());
                }

                // Document the cross-compilation environment variables needed
                let cross_compile_path = toolchain_extracted_dir.join("bin").join("aarch64-none-elf-");
                log::info!("ARM GNU toolchain bin path: {}", cross_compile_path.display());

                // 3) Clone OHCL Linux Kernel (Host Linux Kernel)
                let host_kernel_dir = toolchain_dir.join("OHCL-Linux-Kernel");
                if !host_kernel_dir.exists() {
                    log::info!("Cloning OHCL Linux Kernel to {}", host_kernel_dir.display());
                    xshell::cmd!(sh, "git clone --branch {OHCL_LINUX_KERNEL_PLANE0_BRANCH} {OHCL_LINUX_KERNEL_REPO}").arg(&host_kernel_dir).run()?;
                    log::info!("OHCL Linux Kernel cloned successfully");
                } else if update_repo {
                    log::info!("Updating OHCL Linux Kernel repo...");
                    sh.change_dir(&host_kernel_dir);
                    xshell::cmd!(sh, "git pull --ff-only").run()?;
                    sh.change_dir(shrinkwrap_dir.parent().unwrap());
                    log::info!("OHCL Linux Kernel updated successfully");
                } else {
                    log::info!("OHCL Linux Kernel already exists at {}", host_kernel_dir.display());
                }

                // 4) Compile OHCL Linux Kernel with ARM GNU toolchain
                let kernel_image = host_kernel_dir.join("arch").join("arm64").join("boot").join("Image");
                if !kernel_image.exists() {
                    log::info!("Compiling OHCL Linux Kernel...");
                    sh.change_dir(&host_kernel_dir);

                    // Set environment variables for cross-compilation
                    let arch = "arm64";
                    let cross_compile = cross_compile_path.to_str()
                        .ok_or_else(|| anyhow::anyhow!("Invalid cross_compile path"))?;

                    // Run make defconfig
                    log::info!("Running make defconfig...");
                    xshell::cmd!(sh, "make ARCH={arch} CROSS_COMPILE={cross_compile} defconfig").run()
                        .map_err(|e| anyhow::anyhow!("Failed to run make defconfig: {}", e))?;

                    // Enable required kernel configs
                    log::info!("Enabling required kernel configurations...");
                    xshell::cmd!(sh, "./scripts/config --file .config --enable CONFIG_VIRT_DRIVERS --enable CONFIG_ARM_CCA_GUEST").run()
                        .map_err(|e| anyhow::anyhow!("Failed to enable CCA configs: {}", e))?;
                    xshell::cmd!(sh, "./scripts/config --file .config --enable CONFIG_NET_9P --enable CONFIG_NET_9P_FD --enable CONFIG_NET_9P_VIRTIO --enable CONFIG_NET_9P_FS").run()
                        .map_err(|e| anyhow::anyhow!("Failed to enable 9P configs: {}", e))?;
                    xshell::cmd!(sh, "./scripts/config --file .config --enable CONFIG_HYPERV --enable CONFIG_HYPERV_MSHV --enable CONFIG_MSHV --enable CONFIG_MSHV_VTL --enable CONFIG_HYPERV_VTL_MODE").run()
                        .map_err(|e| anyhow::anyhow!("Failed to enable Hyper-V configs: {}", e))?;

                    // Run make olddefconfig
                    log::info!("Running make olddefconfig...");
                    xshell::cmd!(sh, "make ARCH={arch} CROSS_COMPILE={cross_compile} olddefconfig").run()
                        .map_err(|e| anyhow::anyhow!("Failed to run make olddefconfig: {}", e))?;

                    // Build kernel Image
                    log::info!("Building kernel Image (this may take several minutes)...");
                    let nproc = std::thread::available_parallelism()
                        .map(|n| n.get().to_string())
                        .unwrap_or_else(|_| "1".to_string());
                    xshell::cmd!(sh, "make ARCH={arch} CROSS_COMPILE={cross_compile} Image -j{nproc}").run()
                        .map_err(|e| anyhow::anyhow!("Failed to build kernel Image: {}", e))?;

                    // Verify kernel Image was created
                    if !kernel_image.exists() {
                        anyhow::bail!("Kernel compilation appeared to succeed but Image file was not created at {}", kernel_image.display());
                    }

                    log::info!("OHCL Linux Kernel compiled successfully");
                    log::info!("Kernel Image at: {}", kernel_image.display());
                } else {
                    log::info!("OHCL Linux Kernel Image already exists at {}", kernel_image.display());
                    log::info!("To rebuild, delete the Image file and run again");
                }

                // 4.5) Clone OpenVMM TMK branch with plane0 support and build TMK components
                let tmk_kernel_dir = toolchain_dir.join("OpenVMM-TMK");
                if !tmk_kernel_dir.exists() {
                    log::info!("Cloning OpenVMM TMK branch to {}", tmk_kernel_dir.display());
                    xshell::cmd!(sh, "git clone --branch {OPENVMM_TMK_BRANCH} {OPENVMM_TMK_REPO}").arg(&tmk_kernel_dir).run()?;
                    log::info!("OpenVMM TMK branch cloned successfully");
                } else if update_repo {
                    log::info!("Updating OpenVMM TMK repo...");
                    sh.change_dir(&tmk_kernel_dir);
                    xshell::cmd!(sh, "git pull --ff-only").run()?;
                    sh.change_dir(shrinkwrap_dir.parent().unwrap());
                    log::info!("OpenVMM TMK repo updated successfully");
                } else {
                    log::info!("OpenVMM TMK already exists at {}", tmk_kernel_dir.display());
                }

                // Install Rust targets and build TMK components if do_installs is true
                if do_installs {
                    log::info!("Installing Rust cross-compilation targets...");
                    xshell::cmd!(sh, "rustup target add aarch64-unknown-linux-gnu").run()?;
                    xshell::cmd!(sh, "rustup target add aarch64-unknown-none").run()?;

                    // Change to the TMK kernel directory (which should be the openvmm repo root)
                    sh.change_dir(&tmk_kernel_dir);

                    // Unset ARCH and CROSS_COMPILE if they were set
                    log::info!("Building TMK components...");

                    // Build simple_tmk
                    let simple_tmk_binary = tmk_kernel_dir.join("target").join("aarch64-minimal_rt-none").join("debug").join("simple_tmk");
                    if !simple_tmk_binary.exists() {
                        log::info!("Building simple_tmk...");
                        xshell::cmd!(sh, "cargo build -p simple_tmk --config openhcl/minimal_rt/aarch64-config.toml")
                            .env("RUSTC_BOOTSTRAP", "1")
                            .env_remove("ARCH")
                            .env_remove("CROSS_COMPILE")
                            .run()
                            .map_err(|e| anyhow::anyhow!("Failed to build simple_tmk: {}", e))?;
                        log::info!("simple_tmk built successfully at: {}", simple_tmk_binary.display());
                    } else {
                        log::info!("simple_tmk binary already exists at {}", simple_tmk_binary.display());
                    }

                    // Build tmk_vmm
                    let tmk_vmm_binary = tmk_kernel_dir.join("target").join("aarch64-unknown-linux-gnu").join("debug").join("tmk_vmm");
                    if !tmk_vmm_binary.exists() {
                        log::info!("Building tmk_vmm...");
                        xshell::cmd!(sh, "cargo build -p tmk_vmm --target aarch64-unknown-linux-gnu")
                            .env("RUSTC_BOOTSTRAP", "1")
                            .env_remove("ARCH")
                            .env_remove("CROSS_COMPILE")
                            .run()
                            .map_err(|e| anyhow::anyhow!("Failed to build tmk_vmm: {}", e))?;
                        log::info!("tmk_vmm built successfully at: {}", tmk_vmm_binary.display());
                    } else {
                        log::info!("tmk_vmm binary already exists at {}", tmk_vmm_binary.display());
                    }

                    // Return to parent directory
                    sh.change_dir(shrinkwrap_dir.parent().unwrap());
                } else {
                    log::info!("Skipping TMK builds (do_installs=false). Run with --install-missing-deps to build.");
                }

                // 5) Clone shrinkwrap repo first (need it for venv location)
                if !shrinkwrap_dir.exists() {
                    log::info!("Cloning Shrinkwrap repo to {}", shrinkwrap_dir.display());
                    xshell::cmd!(sh, "git clone").arg(SHRINKWRAP_REPO).arg(&shrinkwrap_dir).run()?;
                } else if update_repo {
                    log::info!("Updating Shrinkwrap repo...");
                    sh.change_dir(&shrinkwrap_dir);
                    xshell::cmd!(sh, "git pull --ff-only").run()?;
                }

                // 6) Create Python virtual environment and install deps
                let venv_dir = shrinkwrap_dir.join("venv");
                if do_installs {
                    if !venv_dir.exists() {
                        log::info!("Creating Python virtual environment at {}", venv_dir.display());
                        xshell::cmd!(sh, "python3 -m venv").arg(&venv_dir).run()?;
                    }

                    log::info!("Installing Python dependencies in virtual environment...");
                    let pip_bin = venv_dir.join("bin").join("pip");
                    xshell::cmd!(sh, "{pip_bin} install --upgrade pip").run()?;
                    xshell::cmd!(sh, "{pip_bin} install pyyaml termcolor tuxmake").run()?;
                }

                // 7) Validate shrinkwrap entrypoint exists
                let shrinkwrap_bin_dir = shrinkwrap_dir.join("shrinkwrap");
                if !shrinkwrap_bin_dir.exists() {
                    anyhow::bail!(
                        "expected shrinkwrap directory at {}, but it does not exist",
                        shrinkwrap_bin_dir.display()
                    );
                }

                // 8) Print PATH guidance
                log::info!("=== Setup Complete ===");
                log::info!("");
                log::info!("Shrinkwrap repo ready at: {}", shrinkwrap_dir.display());
                log::info!("Virtual environment at: {}", venv_dir.display());
                log::info!("ARM GNU toolchain ready at: {}", toolchain_extracted_dir.display());
                log::info!("OHCL Linux Kernel ready at: {}", host_kernel_dir.display());
                log::info!("Kernel Image at: {}", kernel_image.display());

                // Check if TMK binaries exist and report their status
                let simple_tmk_binary = tmk_kernel_dir.join("target").join("aarch64-minimal_rt-none").join("debug").join("simple_tmk");
                let tmk_vmm_binary = tmk_kernel_dir.join("target").join("aarch64-unknown-linux-gnu").join("debug").join("tmk_vmm");

                if simple_tmk_binary.exists() {
                    log::info!("simple_tmk binary at: {}", simple_tmk_binary.display());
                }
                if tmk_vmm_binary.exists() {
                    log::info!("tmk_vmm binary at: {}", tmk_vmm_binary.display());
                }

                log::info!("");
                log::info!("To use shrinkwrap in your shell:");
                log::info!("  source {}/bin/activate", venv_dir.display());
                log::info!("  export PATH={}:$PATH", shrinkwrap_bin_dir.display());
                log::info!("");
                log::info!("For kernel compilation, set these environment variables:");
                log::info!("  export ARCH=arm64");
                log::info!("  export CROSS_COMPILE={}", cross_compile_path.display());
                log::info!("");
                log::info!("For TMK builds, Rust targets are installed (aarch64-unknown-linux-gnu, aarch64-unknown-none)");
                log::info!("Or the pipeline will invoke it directly using the venv Python.");

                Ok(())
            }
        });

        Ok(())
    }
}
