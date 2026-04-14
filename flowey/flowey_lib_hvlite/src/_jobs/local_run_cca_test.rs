// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run OpenVMM CCA tests. Now we run them using emulator, code can be tweaked
//! to support running tests on native hardware platform.
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonPlatform;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::env;

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::build_tmks::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { test_root, done } = request;

        let shrinkwrap_dir = test_root.join("shrinkwrap");
        let venv_dir = shrinkwrap_dir.join("venv");
        let shrinkwrap_exe = shrinkwrap_dir.join("shrinkwrap/shrinkwrap");
        if !shrinkwrap_exe.exists() {
            anyhow::bail!(
                "shrinkwrap installation is missing or broken, try --install-emu or --update-emu --rebuild"
            );
        }

        if !venv_dir.exists() {
            anyhow::bail!(
                "can't find shrinkwrap venv, try --install-emu or --update-emu --rebuild"
            );
        }

        let plane0_linux_image = test_root.join("plane0-linux/arch/arm64/boot/Image");
        if !plane0_linux_image.exists() {
            anyhow::bail!(
                "can't find plane0 linux image at {}, try --install-emu or --update-emu --rebuild",
                plane0_linux_image.display()
            );
        }

        let home_dir = env::var("HOME").map(PathBuf::from).expect("HOME not set");
        let firmware_dir = home_dir.join(".shrinkwrap/package/cca-3world");
        let rootfs_file = firmware_dir.join("rootfs.ext2");
        if !rootfs_file.exists() {
            anyhow::bail!(
                "can't find cca emulation rootfs at {}, try --install-emu or --update-emu --rebuild",
                rootfs_file.display()
            );
        }

        let e2fsck_bin =
            home_dir.join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin/e2fsck");
        if !e2fsck_bin.exists() {
            anyhow::bail!(
                "can't find host e2fsck binary at {}, try --install-emu or --update-emu --rebuild",
                e2fsck_bin.display()
            );
        }

        let resize2fs_bin =
            home_dir.join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin/resize2fs");
        if !resize2fs_bin.exists() {
            anyhow::bail!(
                "can't find host resize2fs binary at {}, try --install-emu or --update-emu --rebuild",
                resize2fs_bin.display()
            );
        }

        // Generate request to build tmk_vmm
        let tmk_vmm_output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
            target: CommonTriple::Common {
                arch: CommonArch::Aarch64,
                platform: CommonPlatform::LinuxGnu,
            },
            unstable_whp: false,
            profile: CommonProfile::Debug,
            tmk_vmm: v,
        });

        // Generate request to build simple_tmk
        let simple_tmk_output = ctx.reqv(|v| crate::build_tmks::Request {
            arch: CommonArch::Aarch64,
            profile: CommonProfile::Debug,
            tmks: v,
        });

        ctx.emit_rust_step("running cca tests", |ctx| {
            done.claim(ctx);
            let tmk_vmm_output = tmk_vmm_output.claim(ctx);
            let simple_tmk_output = simple_tmk_output.claim(ctx);
            move |rt| {
                let tmk_vmm_output = rt.read(tmk_vmm_output);
                let crate::build_tmk_vmm::TmkVmmOutput::LinuxBin { bin: tmk_vmm_bin, .. } =
                    tmk_vmm_output
                else {
                    anyhow::bail!("expect Linux tmk_vmm only");
                };

                let simple_tmk_output = rt.read(simple_tmk_output);
                let simple_tmk_bin = simple_tmk_output.bin;

                // fsck has the following exit_code, if we start the FVP and
                // then kill it by force, the rootfs will left in 'dirty' status,
                // but fsck will just clean it and finish with exit code 1, this
                // is not an error.
                //
                //   0  No errors
                //   1  Errors found and corrected (common after journal replay)
                //   (full exit code see https://man7.org/linux/man-pages/man8/e2fsck.8.html)
                let fsck_cmd = format!(
                    r#"
                    {e2fsck_bin} -fp {rootfs_file} || rc=$?
                    [ "${{rc:-0}}" -le 1 ] || exit "$rc"
                    "#,
                    e2fsck_bin = e2fsck_bin.display(),
                    rootfs_file = rootfs_file.display(),
                );
                flowey::shell_cmd!(rt, "bash -c {fsck_cmd}").run()?;
                log::info!("e2fsck finished");

                flowey::shell_cmd!(rt, "{resize2fs_bin} {rootfs_file} 1024M").run()?;
                log::info!("resize rootfs to 1024M finished");

                let guest_disk = firmware_dir.join("guest-disk.img");
                let kvmtool_efi = firmware_dir.join("KVMTOOL_EFI.fd");
                let lkvm = firmware_dir.join("lkvm");

                // Build the mount/inject script
                let mount_script = format!(
                    r#"
                    set -e
                    mkdir -p mnt
                    mount {rootfs_file} mnt
                    mkdir -p mnt/cca
                    cp {simple_tmk_bin} mnt/cca/
                    cp {tmk_vmm_bin} mnt/cca/
                    cp {guest_disk} mnt/cca/
                    cp {plane0_linux_image} mnt/cca/
                    cp {kvmtool_efi} mnt/cca/
                    cp {lkvm} mnt/cca/
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
                    rootfs_file = rootfs_file.display(),
                    simple_tmk_bin = simple_tmk_bin.display(),
                    tmk_vmm_bin = tmk_vmm_bin.display(),
                    guest_disk = guest_disk.display(),
                    plane0_linux_image = plane0_linux_image.display(),
                    kvmtool_efi = kvmtool_efi.display(),
                    lkvm = lkvm.display()
                );

                let mnt_output = flowey::shell_cmd!(rt, "sudo bash -c {mount_script}")
                    .output()
                    .map_err(|e| anyhow::anyhow!("failed to execute mount script: {}", e))?;

                if !mnt_output.status.success() {
                    anyhow::bail!("failed to mount or inject files into guest rootfs: exit status {}", mnt_output.status);
                }

                log::info!("rootfs.ext2 updated successfully with cca firmwares, paravisor, and tests injected");
                log::info!("launching openvmm cca tests...");

                let venv_bin_path = format!("{}:{}", venv_dir.join("bin").display(), env::var("PATH").unwrap_or_default());
                flowey::shell_cmd!(rt, "{shrinkwrap_exe} run cca-3world.yaml --rtvar ROOTFS={rootfs_file}")
                    .env("VIRTUAL_ENV", &venv_dir)
                    .env("PATH", &venv_bin_path)
                    .run()
                    .with_context(|| "failed to launch guest using shrinkwrap}")?;

                log::info!("openvmm cca tests finished");

                Ok(())
            }
        });

        Ok(())
    }
}
