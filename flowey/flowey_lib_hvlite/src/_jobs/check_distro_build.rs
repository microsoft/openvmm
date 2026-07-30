// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure `openvmm` still builds the way a Linux distribution package builds
//! it.
//!
//! OpenVMM publishes a source release that distributions build and package
//! themselves, so this configuration is a shipping interface. It differs from
//! every other build in CI in one important way: it does not use the
//! repository's `.packages/` provisioning, because a distribution build cannot
//! consume prebuilt native libraries. Every native dependency comes from a
//! distribution package instead, and the two environment overrides a packager
//! must set are set here as well.
//!
//! Without this job, a change that only resolves through `.packages/` breaks
//! downstream packagers silently, and we would not find out until someone
//! tried to build a release.
//!
//! The build runs against the release assets themselves -- assembled by the
//! same node the release publishes from, then verified and unpacked the way a
//! packager would. Building the checkout instead would let this pass on a tree
//! a packager cannot reproduce, since a packager has no `.git` directory and no
//! untracked files.

use crate::assemble_openvmm_source_release::CHECKSUM_FILE;
use crate::assemble_openvmm_source_release::IdentitySource;
use crate::assemble_openvmm_source_release::METADATA_FILE;
use crate::assemble_openvmm_source_release::expected_metadata;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Which identity to assemble and build under.
        ///
        /// A release passes [`IdentitySource::ReleaseTag`], so the job builds
        /// the archive that is about to be published rather than a lookalike.
        pub identity: IdentitySource,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::assemble_openvmm_source_release::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { identity, done } = request;

        let target = target_lexicon::triple!("x86_64-unknown-linux-gnu");

        // This job deliberately does not depend on
        // `install_openvmm_rust_build_essential`. That node provisions `protoc`
        // out of `.packages/`, which is the dependency this job exists to prove
        // we do not need. It also skips the `-Dwarnings` cargo config, which a
        // packager does not build with either; the clippy jobs cover warnings.
        let mut deps = vec![ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled)];

        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            deps.push(ctx.reqv(|v| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![
                        // a C toolchain and a working linker
                        "build-essential".into(),
                        // Linux UAPI headers, for the SQLite bundled by
                        // `libsqlite3-sys`. `build-essential` pulls this in
                        // transitively; it is named here so the reason it is
                        // needed is written down.
                        "linux-libc-dev".into(),
                        // for `openssl-sys`
                        "libssl-dev".into(),
                        "pkg-config".into(),
                        // for `prost` / `pbjson`
                        "protobuf-compiler".into(),
                    ],
                    done: v,
                }
            }));
        }

        ctx.req(flowey_lib_common::install_rust::Request::InstallTargetTriple(target.clone()));

        // A release resolves the tag it is publishing, so this job builds the
        // archive that is about to ship. A commit under test has no release
        // version, so it is assembled under a snapshot identity instead.
        // Everything else about the assembly and the build is identical.
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let resolved = ctx.emit_rust_stepv("resolve source release identity", |ctx| {
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let output_dir = std::env::current_dir()?.join("openvmm-source-release");
                let path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(path);

                Ok((identity.resolve(rt)?, output_dir))
            }
        });
        let identity = resolved.clone().map(ctx, |(identity, _)| identity);
        let output_dir = resolved.map(ctx, |(_, output_dir)| output_dir);

        let assembled = ctx.reqv(|done| crate::assemble_openvmm_source_release::Request {
            identity: identity.clone(),
            output_dir: output_dir.clone(),
            done,
        });
        let output_dir = output_dir.depending_on(ctx, &assembled);

        ctx.emit_rust_step("build openvmm in a distribution configuration", |ctx| {
            done.claim(ctx);
            deps.claim(ctx);
            let identity = identity.claim(ctx);
            let output_dir = output_dir.claim(ctx);
            move |rt| {
                let identity = rt.read(identity);
                let output_dir = rt.read(output_dir);

                // A packager starts by checking the archive against the
                // checksums we published, so start there too.
                rt.sh.change_dir(&output_dir);
                flowey::shell_cmd!(rt, "sha256sum --check --strict {CHECKSUM_FILE}").run()?;

                // Unpack the archive exactly as a packager would, into a
                // directory outside the repository so nothing can reach back
                // into the checkout.
                let build_root = std::env::current_dir()?.join("distro-build");
                if build_root.exists() {
                    fs_err::remove_dir_all(&build_root)?;
                }
                fs_err::create_dir_all(&build_root)?;
                let archive = output_dir.join(identity.archive_name());
                flowey::shell_cmd!(rt, "tar -xf {archive} -C {build_root}").run()?;

                let source_dir = build_root.join(identity.source_root());
                if source_dir.join(".git").exists() {
                    anyhow::bail!("the source archive must not contain a .git directory");
                }

                // The archive carries its own identity, which is how a packager
                // recovers a version without a `.git` directory. If that were
                // missing or wrong, the build would still succeed and the
                // package would be labelled incorrectly. Compare the whole
                // document, not just the version: `tag` in particular is the
                // field distinguishing a release from a snapshot, and this is
                // the only place the metadata contract is exercised end to end.
                let metadata_path = source_dir.join(METADATA_FILE);
                let metadata: serde_json::Value =
                    serde_json::from_slice(&fs_err::read(&metadata_path)?)
                        .with_context(|| format!("failed to parse {}", metadata_path.display()))?;
                let expected = expected_metadata(&identity);
                if metadata != expected {
                    anyhow::bail!(
                        "the source archive declares {metadata}, but it was assembled as {expected}"
                    );
                }

                rt.sh.change_dir(&source_dir);

                // `.cargo/config.toml` points `PROTOC` into `.packages/`. It
                // does not set `force`, so an inherited `PROTOC` takes
                // precedence, which is what lets a packager redirect it at the
                // system compiler.
                let protoc = flowey::shell_cmd!(rt, "which protoc").read()?;
                let protoc = protoc.trim();

                let target = target.to_string();
                // Build the way a packager does. A spec file builds the release
                // profile, so building anything else here would leave
                // release-only code -- `#[cfg(not(debug_assertions))]` blocks
                // in particular -- never compiled by this gate.
                flowey::shell_cmd!(
                    rt,
                    "cargo build --release --locked -p openvmm --target {target}"
                )
                .env("PROTOC", protoc)
                // Link the system OpenSSL rather than building a vendored
                // copy. Nothing in `openvmm`'s tree enables `openssl-sys`'s
                // `vendored` feature today, so this is currently inert --
                // it is set because a packager sets it, and because it
                // becomes load-bearing the moment something turns that
                // feature on.
                .env("OPENSSL_NO_VENDOR", "1")
                // The workspace's release profile carries debug info, which
                // is the binding constraint on runner disk. Nothing debugs
                // this artifact.
                .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
                .env("CARGO_INCREMENTAL", "0")
                .run()?;

                // Building is not enough. If `openssl-sys` were to start
                // building a vendored copy, or if some dependency were to
                // acquire a static native library, the build would still
                // succeed but the packaged binary would no longer be one the
                // distribution can service. Assert the shape of the result.
                //
                // Read the binary's own `NEEDED` entries rather than `ldd`'s
                // output: `ldd` reports the whole transitive closure, so it
                // would still be satisfied if `openvmm` linked a static
                // OpenSSL while some unrelated shared library pulled in the
                // system one.
                let binary = format!("target/{target}/release/openvmm");
                let linkage = flowey::shell_cmd!(rt, "readelf -d {binary}").read()?;
                for lib in ["libssl.so", "libcrypto.so"] {
                    let needed = linkage
                        .lines()
                        .filter(|line| line.contains("(NEEDED)"))
                        .any(|line| line.contains(lib));

                    if !needed {
                        anyhow::bail!(
                            "openvmm did not link the system {lib}; \
                             a distribution build must use the distribution's OpenSSL.\n\
                             readelf -d output:\n{linkage}"
                        );
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
