// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Copy openvmm-deps files (linux test kernel, initrd, petritools erofs, etc.)
//! into the correct "magic directory" set by the project-level `[env]` table
//! in `.cargo/config.toml`.
//!
//! To add a new dep file, add an entry to `dep_files` — no other modules
//! need to change.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

new_flow_node!(struct Node);

fn dir_name(arch: CommonArch) -> &'static str {
    match arch {
        CommonArch::Aarch64 => "aarch64",
        CommonArch::X86_64 => "x64",
    }
}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub done: WriteVar<SideEffect>,
    }
}

/// A single file to copy from openvmm-deps into the magicpath.
struct DepFile {
    /// Which dep file to resolve from openvmm-deps.
    dep: crate::resolve_openvmm_deps::OpenvmmDepFile,
    /// Destination filename (relative to `underhill-deps-private/{arch}/`).
    /// When arch-dependent, use a closure; when fixed, the `_arch` is ignored.
    dest_filename: fn(CommonArch) -> &'static str,
}

/// The table of dep files to copy from the main openvmm-deps archive. To
/// add a new dep, add an entry here.
///
/// The Linux test kernel and matching shared initrd are *not* in this table —
/// the kernel comes from per-(arch, kver) archives via
/// [`crate::resolve_openvmm_test_linux_kernel`], the initrd from the shared
/// [`crate::resolve_openvmm_test_initrd`] node, and both are placed into the
/// magicpath separately below.
fn dep_files() -> Vec<DepFile> {
    use crate::resolve_openvmm_deps::OpenvmmDepFile;
    vec![DepFile {
        dep: OpenvmmDepFile::PetritoolsErofs,
        dest_filename: |_arch| "petritools.erofs",
    }]
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut by_arch: BTreeMap<CommonArch, Vec<WriteVar<SideEffect>>> = BTreeMap::new();
        for Request { arch, done } in requests {
            by_arch.entry(arch).or_default().push(done);
        }

        // -- end of req processing -- //

        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        for (arch, out_vars) in by_arch {
            // Resolve all dep files for this arch.
            let mut resolved: Vec<(ReadVar<PathBuf>, &'static str)> = dep_files()
                .into_iter()
                .map(|dep_file| {
                    let src = ctx
                        .reqv(|v| crate::resolve_openvmm_deps::Request::Get(dep_file.dep, arch, v));
                    let dst_name = (dep_file.dest_filename)(arch);
                    (src, dst_name)
                })
                .collect();

            // Resolve the Linux test kernel for this arch from the per-(arch,
            // kver) `openvmm-test-linux` archive, and the matching guest-
            // userland initrd from the version-independent
            // `openvmm-test-initrd` archive.
            use crate::resolve_openvmm_test_linux_kernel::DEFAULT_LINUX_TEST_KERNEL_VERSION as KVER;
            use crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile;
            let kernel_src = ctx.reqv(|v| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    OpenvmmTestKernelFile::Kernel,
                    arch,
                    KVER,
                    v,
                )
            });
            let kernel_dst_name = OpenvmmTestKernelFile::Kernel.filename(arch);
            resolved.push((kernel_src, kernel_dst_name));
            if OpenvmmTestKernelFile::BzImage.is_available_for(arch) {
                let bzimage_src = ctx.reqv(|v| {
                    crate::resolve_openvmm_test_linux_kernel::Request::Get(
                        OpenvmmTestKernelFile::BzImage,
                        arch,
                        KVER,
                        v,
                    )
                });
                resolved.push((bzimage_src, OpenvmmTestKernelFile::BzImage.filename(arch)));
            }
            let initrd_src =
                ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(arch, v));
            resolved.push((initrd_src, "initrd"));

            ctx.emit_rust_step(
                format!("copy {arch:?} openvmm-deps files to magicpath"),
                |ctx| {
                    let resolved: Vec<_> = resolved
                        .into_iter()
                        .map(|(src, name)| (src.claim(ctx), name))
                        .collect();
                    let openvmm_magicpath = openvmm_magicpath.clone().claim(ctx);
                    out_vars.claim(ctx);

                    move |rt| {
                        let magicpath = rt.read(openvmm_magicpath);
                        let dst_dir = magicpath
                            .join("underhill-deps-private")
                            .join(dir_name(arch));
                        fs_err::create_dir_all(&dst_dir)?;

                        for (src, filename) in resolved {
                            let src = rt.read(src);
                            let dst = dst_dir.join(filename);
                            if src.absolute()? != dst.absolute()? {
                                fs_err::copy(&src, &dst)?;
                            }
                        }

                        Ok(())
                    }
                },
            );
        }

        Ok(())
    }
}
