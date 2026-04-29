// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install CCA emulation environment. Now we only support using ARM's Fixed
//! Virtual Platform (FVP) as the emulator. The environment also contains a
//! few essential firmwares in CCA stack, for example TF-A and TF-RMM. ARM has
//! published an python based tool, 'shrinkwrap', to simply the deployment
//! process, this installation use it as well.
use flowey::node::prelude::RustRuntimeServices;
use flowey::node::prelude::*;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::thread;

const SHRINKWRAP_REPO: &str = "https://git.gitlab.arm.com/tooling/shrinkwrap.git";
// The guest Linux kernel (with cca/plane driver) hasn't been upstreamed yet, fetch it from our private repo
const PLANE0_LINUX_REPO: &str = "https://github.com/weiding-msft/OHCL-Linux-Kernel.git";
const PLANE0_LINUX_BRANCH: &str = "with-arm-rebased-planes";
// A few config information needed when building Linux kernel
const CCA_CONFIGS: &[&str] = &["CONFIG_VIRT_DRIVERS", "CONFIG_ARM_CCA_GUEST"];
const NINEP_CONFIGS: &[&str] = &[
    "CONFIG_NET_9P",
    "CONFIG_NET_9P_FD",
    "CONFIG_NET_9P_VIRTIO",
    "CONFIG_NET_9P_FS",
];
const HYPERV_CONFIGS: &[&str] = &[
    "CONFIG_HYPERV",
    "CONFIG_HYPERV_MSHV",
    "CONFIG_MSHV",
    "CONFIG_MSHV_VTL",
    "CONFIG_HYPERV_VTL_MODE",
];

flowey_request! {
    pub struct Params {
        /// The CCA test root directory, defaults to target/cca-tests.
        pub test_root: PathBuf,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

fn clone_repo(
    rt: &RustRuntimeServices<'_>,
    repo_url: &str,
    target_dir: &Path,
    branch: Option<&str>,
    repo_name: &str,
) -> anyhow::Result<()> {
    if target_dir.exists() {
        log::info!(
            "{} has been installed at {}",
            repo_name,
            target_dir.display()
        );
        return Ok(());
    }

    log::info!("Cloning {} to {}", repo_name, target_dir.display());

    if let Some(b) = branch {
        flowey::shell_cmd!(rt, "git clone --branch {b} {repo_url} {target_dir}").run()?;
    } else {
        flowey::shell_cmd!(rt, "git clone {repo_url} {target_dir}").run()?;
    }

    log::info!("{} has been cloned successfully", repo_name);

    Ok(())
}

fn enable_kernel_configs(
    rt: &RustRuntimeServices<'_>,
    group: &str,
    configs: &[&str],
) -> anyhow::Result<()> {
    // Enable each config one at a time to avoid shell argument parsing issues
    for config in configs {
        flowey::shell_cmd!(rt, "./scripts/config --file .config --enable {config}")
            .run()
            .with_context(|| format!("Failed to enable {} kernel config {}", group, config))?;
    }

    Ok(())
}

fn make_target(
    rt: &RustRuntimeServices<'_>,
    arch: &str,
    target: &str,
    jobs: &str,
) -> anyhow::Result<()> {
    flowey::shell_cmd!(
        rt,
        "make ARCH={arch} CROSS_COMPILE=aarch64-linux-gnu- {target} -j{jobs}"
    )
    .run()
    .with_context(|| format!("Failed to run `make {}`", target))?;
    Ok(())
}

fn build_plane0_linux(
    rt: &RustRuntimeServices<'_>,
    plane0_linux: &Path,
    plane0_image: &Path,
) -> anyhow::Result<()> {
    log::info!("Compiling Plane0 Linux kernel...");
    rt.sh.change_dir(plane0_linux);

    const ARCH: &str = "arm64";
    const SINGLE_JOB: &str = "1";

    log::info!("Running make defconfig...");
    make_target(rt, ARCH, "defconfig", SINGLE_JOB)?;

    log::info!("Enabling required kernel configurations...");
    for (name, configs) in [
        ("CCA", CCA_CONFIGS),
        ("9P", NINEP_CONFIGS),
        ("Hyper-V", HYPERV_CONFIGS),
    ] {
        enable_kernel_configs(rt, name, configs)?;
    }

    log::info!("Running make olddefconfig...");
    make_target(rt, ARCH, "olddefconfig", SINGLE_JOB)?;

    let jobs =
        thread::available_parallelism().map_or_else(|_| "1".to_string(), |n| n.get().to_string());

    log::info!("Building plane0 kernel image...");
    make_target(rt, ARCH, "Image", &jobs)?;

    anyhow::ensure!(
        plane0_image.exists(),
        "Plane0 kernel compilation appeared to succeed but image file was not found at {}",
        plane0_image.display()
    );

    log::info!("Plane0 Linux kernel compiled successfully");
    log::info!("Kernel image at: {}", plane0_image.display());
    Ok(())
}

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { test_root, done } = request;

        let openvmm_root = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("install cca emulation environment", |ctx| {
            done.claim(ctx);
            let openvmm_root = openvmm_root.claim(ctx);
            move |rt| {
                // emulation environment is under 'test_root'
                fs_err::create_dir_all(&test_root)?;

                // 'shrinkwrap' only build host Linux kernel, plane0 Linux kernel
                // needs to be downloaded and built separately.
                let plane0_linux = test_root.join("plane0-linux");
                let plane0_image = plane0_linux
                    .join("arch")
                    .join("arm64")
                    .join("boot")
                    .join("Image");
                if plane0_linux.exists() {
                    log::info!(
                        "plane0 Linux source tree is already installed at: {}",
                        plane0_linux.display()
                    );
                } else {
                    clone_repo(
                        &rt,
                        PLANE0_LINUX_REPO,
                        &plane0_linux,
                        Some(PLANE0_LINUX_BRANCH),
                        "plane0 Linux",
                    )?;
                }

                // Now check if image has been built
                if plane0_image.exists() {
                    log::info!(
                        "plane0 Linux image also has been built and found at: {}",
                        plane0_image.display()
                    );
                } else {
                    build_plane0_linux(rt, &plane0_linux, &plane0_image)?;
                }

                // Install the remaining emulation environment components
                // using 'shrinkwrap', which leverages YAML to define all required
                // components. This significantly reduces manual effort and the risk of errors.
                let shrinkwrap_dir = test_root.join("shrinkwrap");
                let venv_dir = shrinkwrap_dir.join("venv");
                if shrinkwrap_dir.exists() {
                    log::info!(
                        "'shrinkwrap' source tree is already installed at: {}",
                        shrinkwrap_dir.display()
                    );
                } else {
                    clone_repo(&rt, SHRINKWRAP_REPO, &shrinkwrap_dir, None, "shrinkwrap")?;

                    // Create venv
                    if !venv_dir.exists() {
                        log::info!(
                            "Creating Python virtual environment at {}",
                            venv_dir.display()
                        );
                        flowey::shell_cmd!(rt, "python3 -m venv")
                            .arg(&venv_dir)
                            .run()?;
                    }

                    // Install packages
                    log::info!("Installing Python dependencies...");
                    let pip = venv_dir.join("bin/pip");

                    flowey::shell_cmd!(rt, "{pip} install --upgrade pip").run()?;
                    flowey::shell_cmd!(rt, "{pip} install pyyaml termcolor tuxmake").run()?;
                }

                // A few cleanups after we checkout 'shrinkwrap' source code
                //   - copy over cca plane configuration
                //   - create python venv and install all packages needed when using 'shrinkwrap'
                //   - sync local shrinkwrap overlays into the checked-out tree on every run.
                let openvmm_root = rt.read(openvmm_root);
                let planes_yaml_src =
                    openvmm_root.join("vmm_tests/vmm_tests/test_data/cca_planes.yaml");
                let planes_yaml_dest = shrinkwrap_dir.join("config/cca_planes.yaml");
                let realm_overlay_yaml_src =
                    openvmm_root.join("vmm_tests/vmm_tests/test_data/cca_realm_overlay.yaml");
                let realm_overlay_yaml_dest = shrinkwrap_dir.join("config/cca_realm_overlay.yaml");
                let realm_overlay_dir_src =
                    openvmm_root.join("vmm_tests/vmm_tests/test_data/cca-overlay");
                let realm_overlay_dir_dest = shrinkwrap_dir.join("config/cca-overlay");
                fs_err::create_dir_all(planes_yaml_dest.parent().unwrap())?;

                log::info!(
                    "Copying planes.yaml from {} to {}",
                    planes_yaml_src.display(),
                    planes_yaml_dest.display()
                );
                fs_err::copy(&planes_yaml_src, &planes_yaml_dest)?;

                log::info!(
                    "Copying realm overlay config from {} to {}",
                    realm_overlay_yaml_src.display(),
                    realm_overlay_yaml_dest.display()
                );
                fs_err::copy(&realm_overlay_yaml_src, &realm_overlay_yaml_dest)?;

                if realm_overlay_dir_dest.exists() {
                    fs_err::remove_dir_all(&realm_overlay_dir_dest)?;
                }
                log::info!(
                    "Copying realm overlay directory from {} to {}",
                    realm_overlay_dir_src.display(),
                    realm_overlay_dir_dest.display()
                );
                flowey::shell_cmd!(rt, "cp -a {realm_overlay_dir_src} {realm_overlay_dir_dest}")
                    .run()?;

                let home_dir = env::var("HOME").map(PathBuf::from).expect("HOME not set");
                let rootfs_file = home_dir.join(".shrinkwrap/package/cca-3world/rootfs.ext2");
                if rootfs_file.exists() {
                    log::info!(
                        "cca emulation rootfs is already generated at: {}",
                        rootfs_file.display()
                    );
                } else {
                    // Now, we are all good to use 'shrinkwrap' to build all
                    // components needed by OpenVMM CCA tests
                    let log_dir = test_root.join("logs");
                    fs_err::create_dir_all(&log_dir)?;
                    let log_file = log_dir.join("shrinkwrap.build.log");

                    // Build the command line and go
                    let shrinkwrap_exe = shrinkwrap_dir.join("shrinkwrap/shrinkwrap");
                    let path = format!(
                        "{}:{}",
                        venv_dir.join("bin").display(),
                        env::var("PATH").unwrap_or_default()
                    );

                    let rootfs = "${artifact:BUILDROOT}";
                    let tfa_revision = "8dae0862c502e08568a61a1050091fa9357f1240";
                    let cmd = format!(
                        "{} build cca-3world.yaml \
                        --overlay buildroot.yaml \
                        --overlay cca_realm_overlay.yaml \
                        --overlay cca_planes.yaml \
                        --btvar GUEST_ROOTFS={rootfs} \
                        --btvar TFA_REVISION={tfa_revision} \
                        2>&1 | tee {}",
                        shrinkwrap_exe.display(),
                        log_file.display()
                    );

                    flowey::shell_cmd!(rt, "bash -c {cmd}")
                        .env("VIRTUAL_ENV", &venv_dir)
                        .env("PATH", path)
                        .run()
                        .with_context(|| "failed to do shrinkwrap build")?;

                    log::info!("shrinkwrap build finished, emulation env have been setup");
                }

                Ok(())
            }
        });

        Ok(())
    }
}
