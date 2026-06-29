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
// Pinned shrinkwrap revision to keep local CCA emulation builds stable across upstream updates.
const SHRINKWRAP_REV: &str = "206ea90cb7aa907168a4e7beca94771e2fb2b301";
// The guest Linux kernel (with cca/plane driver) hasn't been upstreamed yet, fetch it from our private repo
const PLANE0_LINUX_REPO: &str = "https://github.com/jiong-microsoft/OHCL-Linux-Kernel.git";
const PLANE0_LINUX_BRANCH: &str = "cca-dev";
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
        /// The CCA test root directory, defaults to target/cca-test.
        pub test_root: PathBuf,
        pub openvmm_root: PathBuf,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

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

pub(crate) fn build_plane0_linux(
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

fn checkout_shrinkwrap_revision(
    rt: &RustRuntimeServices<'_>,
    shrinkwrap_dir: &Path,
) -> anyhow::Result<()> {
    if flowey::shell_cmd!(rt, "git -C {shrinkwrap_dir} cat-file -e {SHRINKWRAP_REV}")
        .run()
        .is_err()
    {
        flowey::shell_cmd!(rt, "git -C {shrinkwrap_dir} fetch origin")
            .run()
            .with_context(|| {
                format!("failed to fetch shrinkwrap repo before checking out rev {SHRINKWRAP_REV}")
            })?;
    }

    flowey::shell_cmd!(rt, "git -C {shrinkwrap_dir} checkout {SHRINKWRAP_REV}")
        .run()
        .with_context(|| format!("failed to checkout shrinkwrap rev {SHRINKWRAP_REV}"))?;

    Ok(())
}

/// Syncs OpenVMM-owned CCA shrinkwrap overlay assets into the shrinkwrap checkout.
///
/// Call this before invoking shrinkwrap builds that reference the CCA overlay
/// assets. The helper ensures `config/` exists under `shrinkwrap_dir` and copies
/// the repo versions of the assets there, replacing existing files only when
/// their contents differ.
pub(crate) fn sync_shrinkwrap_overlay_assets(
    openvmm_root: &Path,
    shrinkwrap_dir: &Path,
) -> anyhow::Result<()> {
    let overlay_assets = [
        (
            openvmm_root.join("vmm_tests/vmm_tests/test_data/cca_planes.yaml"),
            shrinkwrap_dir.join("config/cca_planes.yaml"),
            "planes.yaml",
        ),
        (
            openvmm_root.join("vmm_tests/vmm_tests/test_data/cca_realm_overlay.yaml"),
            shrinkwrap_dir.join("config/cca_realm_overlay.yaml"),
            "realm overlay config",
        ),
        (
            openvmm_root.join("vmm_tests/vmm_tests/test_data/cca_start_tmk.sh"),
            shrinkwrap_dir.join("config/cca_start_tmk.sh"),
            "Plane0 TMK launcher",
        ),
    ];

    fs_err::create_dir_all(shrinkwrap_dir.join("config"))?;

    for (src, dest, label) in overlay_assets {
        if dest.is_file() {
            if fs_err::read(&src)? == fs_err::read(&dest)? {
                log::info!(
                    "{label} already exists at {} and matches source",
                    dest.display()
                );
                continue;
            }

            log::info!(
                "{label} already exists at {} but differs from source; replacing it",
                dest.display()
            );
        }

        log::info!(
            "Copying {label} from {} to {}",
            src.display(),
            dest.display()
        );

        fs_err::copy(&src, &dest)?;
    }

    Ok(())
}

pub(crate) fn build_cca_rootfs(
    rt: &RustRuntimeServices<'_>,
    test_root: &Path,
    shrinkwrap_dir: &Path,
    venv_dir: &Path,
) -> anyhow::Result<()> {
    let shrinkwrap_exe = shrinkwrap_dir.join("shrinkwrap/shrinkwrap");
    anyhow::ensure!(
        shrinkwrap_exe.exists(),
        "shrinkwrap installation is missing or broken at {}, try --install-emu first",
        shrinkwrap_exe.display()
    );
    anyhow::ensure!(
        venv_dir.exists(),
        "shrinkwrap venv is missing at {}, try --install-emu first",
        venv_dir.display()
    );

    let log_dir = test_root.join("logs");
    fs_err::create_dir_all(&log_dir)?;
    let log_file = log_dir.join("shrinkwrap.build.log");

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
        .env("VIRTUAL_ENV", venv_dir)
        .env("PATH", path)
        .run()
        .with_context(|| "failed to do shrinkwrap build")?;

    log::info!("shrinkwrap build finished, emulation env have been setup");
    Ok(())
}

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::git_checkout::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            test_root,
            openvmm_root,
            done,
        } = request;
        let plane0_linux = test_root.join("plane0-linux");
        let shrinkwrap_dir = test_root.join("shrinkwrap");

        let plane0_linux = if plane0_linux.exists() {
            ReadVar::from_static(plane0_linux)
        } else {
            ctx.req(flowey_lib_common::git_checkout::Request::RegisterRepo {
                repo_id: "cca-plane0-linux".into(),
                repo_src: flowey_lib_common::git_checkout::RepoSource::LocalOnlyNewClone {
                    url: PLANE0_LINUX_REPO.into(),
                    path: plane0_linux,
                    ignore_existing_clone: false,
                },
                allow_persist_credentials: false,
                depth: None,
                pre_run_deps: Vec::new(),
            });
            ctx.reqv(|v| flowey_lib_common::git_checkout::Request::CheckoutRepo {
                repo_id: ReadVar::from_static("cca-plane0-linux".into()),
                repo_path: v,
                persist_credentials: false,
            })
        };

        let shrinkwrap_dir = if shrinkwrap_dir.exists() {
            ReadVar::from_static(shrinkwrap_dir)
        } else {
            ctx.req(flowey_lib_common::git_checkout::Request::RegisterRepo {
                repo_id: "shrinkwrap".into(),
                repo_src: flowey_lib_common::git_checkout::RepoSource::LocalOnlyNewClone {
                    url: SHRINKWRAP_REPO.into(),
                    path: shrinkwrap_dir,
                    ignore_existing_clone: false,
                },
                allow_persist_credentials: false,
                depth: None,
                pre_run_deps: Vec::new(),
            });
            ctx.reqv(|v| flowey_lib_common::git_checkout::Request::CheckoutRepo {
                repo_id: ReadVar::from_static("shrinkwrap".into()),
                repo_path: v,
                persist_credentials: false,
            })
        };

        ctx.emit_rust_step("install cca emulation environment", |ctx| {
            done.claim(ctx);
            let plane0_linux = plane0_linux.claim(ctx);
            let shrinkwrap_dir = shrinkwrap_dir.claim(ctx);
            move |rt| {
                // emulation environment is under 'test_root'
                fs_err::create_dir_all(&test_root)?;

                // 'shrinkwrap' only build host Linux kernel, plane0 Linux kernel
                // needs to be downloaded and built separately.
                let plane0_linux = rt.read(plane0_linux);
                let plane0_image = plane0_linux
                    .join("arch")
                    .join("arm64")
                    .join("boot")
                    .join("Image");
                rt.sh.change_dir(&plane0_linux);
                flowey::shell_cmd!(rt, "git checkout {PLANE0_LINUX_BRANCH}").run()?;

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
                let shrinkwrap_dir = rt.read(shrinkwrap_dir);
                let venv_dir = shrinkwrap_dir.join("venv");
                checkout_shrinkwrap_revision(rt, &shrinkwrap_dir)?;

                if !venv_dir.exists() {
                    log::info!(
                        "Creating Python virtual environment at {}",
                        venv_dir.display()
                    );
                    flowey::shell_cmd!(rt, "python3 -m venv")
                        .arg(&venv_dir)
                        .run()?;

                    log::info!("Installing Python dependencies...");
                    let pip = venv_dir.join("bin/pip");

                    flowey::shell_cmd!(rt, "{pip} install --upgrade pip").run()?;
                    flowey::shell_cmd!(rt, "{pip} install pyyaml termcolor tuxmake").run()?;
                }

                sync_shrinkwrap_overlay_assets(&openvmm_root, &shrinkwrap_dir)?;

                let home_dir = env::var("HOME").map(PathBuf::from).expect("HOME not set");
                let rootfs_file = home_dir.join(".shrinkwrap/package/cca-3world/rootfs.ext2");
                if rootfs_file.exists() {
                    log::info!(
                        "cca emulation rootfs is already generated at: {}",
                        rootfs_file.display()
                    );
                } else {
                    build_cca_rootfs(rt, &test_root, &shrinkwrap_dir, &venv_dir)?;
                }

                Ok(())
            }
        });

        Ok(())
    }
}
