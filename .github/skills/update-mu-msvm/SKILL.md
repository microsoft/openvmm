---
name: update-mu-msvm
description: >
  Update OpenVMM UEFI firmware to the latest published microsoft/mu_msvm
  release. Use when updating mu_msvm, bumping the UEFI dependency, refreshing
  firmware Nix hashes, or preparing the firmware ingestion pull request.
---

# Update mu_msvm

Update the firmware dependency, verify both architectures, and prepare a draft
pull request against `microsoft/openvmm:main`. Follow the repository's current
instructions and honor any limits the user places on committing or publishing.

## Execution policy

- Execute routine steps end to end without asking for approval between steps.
  A branch-name override or "do not create a PR" is an instruction, not a
  reason to reconfirm the workflow.
- Ask only for a real safety conflict or a decision that cannot be inferred:
  conflicting user changes, an existing remote branch, divergent local
  history, or a blocker requiring a change outside this update.
- Complete restore, hash calculation, edits, and nonprivileged checks before
  final Nix verification. Do not use sudo or trial builds to discover hashes.
- Use one visible terminal task and one sudo authentication for final Nix
  verification. The user enters their password there. Do not ask them to
  confirm password entry or command completion in chat; collect the task's
  result directly and retain its output so it does not need to be rerun.

## 1. Prepare a clean branch

1. Inspect the worktree, current branch, remotes, and local commits before
   switching branches. Preserve existing changes. If an earlier update is
   already in progress, establish its state before resuming.
2. Use the user's branch-name override when supplied. Otherwise derive the
   fork owner from `origin` and use `user/<fork-owner>/uefi-update-main`.
   Do not hard-code a maintainer's identity or local machine paths.
3. Check the remote before creating or reusing the branch:

   ```bash
   git ls-remote --heads origin "refs/heads/$branch"
   ```

   A failed remote query is not evidence that the branch is absent. If the
   branch exists remotely, stop and ask how to proceed. Never automatically
   delete it, force-push it, or overwrite an earlier update.
4. Fetch current `main` from `https://github.com/microsoft/openvmm.git`.
   Check out local `main`, then create the update branch from current upstream
   `main`. If local `main` is behind, fast-forward it only when safe. If it has
   extra commits or has diverged, ask before proceeding; offer to create the
   update branch directly from fetched upstream `main`, leaving local `main`
   untouched.
5. Inspect the branch's commits and merge-base diff against upstream `main`.
   Do not carry unrelated local changes or commits into the firmware PR.
   Never amend already-pushed commits or rewrite published history without
   express permission.

## 2. Discover the release and previous ingestion

The relevant files are:

- [Firmware version configuration](../../../flowey/flowey_lib_hvlite/src/_jobs/cfg_versions.rs)
- [Nix firmware derivation](../../../nix/uefi_mu_msvm.nix)
- [Flowey firmware downloader](../../../flowey/flowey_lib_hvlite/src/download_uefi_mu_msvm.rs)
- [Development shell](../../../shell.nix)

1. Record the previous `MU_MSVM` value from upstream `main`, before editing.
   Use this version for the PR changelog, even if the old Nix version differs.
2. Query the latest published, non-prerelease release from
   [microsoft/mu_msvm](https://github.com/microsoft/mu_msvm), for example:

   ```bash
   gh api repos/microsoft/mu_msvm/releases/latest \
     --jq '{tag_name, published_at, assets: [.assets[].name]}'
   ```

   The public GitHub REST endpoint can also be queried with `curl` if `gh`
   is unavailable. Strip the leading `v` for version strings in code.
   Never reuse a release number from an earlier invocation without checking.
3. Inspect the release assets and downloader before choosing URLs. Keep the
   existing firmware flavor: legacy C core with `CLANGPDB` for X64 and AARCH64,
   not Patina or VS2022. Do not assume historical asset names still exist.
4. Check both `MU_MSVM` and the Nix derivation before deciding there is no
   update. Report a no-op only when both versions match the latest release
   and the Nix asset URLs and both unpacked hashes are verified current using
   section 4. If Rust is current but Nix is stale, continue with the Nix
   refresh; likewise update Rust if only it is stale. An unchanged `MU_MSVM`
   alone is never a reason to skip a requested Nix refresh.

## 3. Update and restore

1. Set `MU_MSVM` to the new version.
2. Run:

   ```bash
   cargo xflowey restore-packages
   ```

   Check that restore resolves the intended firmware version. Cache hits are
   normal, but success alone does not establish which version was consumed.
   The extraction cache records each archive's version under
   `flowey-persist/flowey_lib_hvlite__download_uefi_mu_msvm/.flowey_info/`.
   Confirm the marker matches `<version>-<asset-name>` before reusing a tree.
3. Set the Nix derivation's `version` to the same version. Calculate both new
   hashes as described below and replace them together; there is no need for
   an intermediate edit that clears the hashes.
4. Match the Nix URL to the actual release assets. Recent releases use
   `firmware-RELEASE-${archToolchain}.tar.gz`; older releases used
   `RELEASE-${archToolchain}-artifacts.tar.gz`. If Flowey also needs an asset
   naming change, update it consistently rather than leaving restore broken.

## 4. Compute and verify both Nix hashes

These are recursive hashes of the unpacked source tree used by `fetchzip`,
not SHA-256 hashes of the compressed downloads. Preserve `stripRoot = false`.

1. Reuse the verified restored trees under
   `flowey-persist/flowey_lib_hvlite__download_uefi_mu_msvm/extracted/<asset-name>/`.
   Hash the archive's extracted contents, not the parent directory or its
   sibling `.flowey_info` metadata. Restore may only fetch the host
   architecture. If either tree is absent or has a different version marker,
   download that architecture's release asset without sudo and extract it to
   a fresh temporary directory, without stripping its root.
2. Compute both hashes locally, without accessing the Nix store:

   ```bash
   nix-hash --type sha256 --base64 "$unpacked_tree"
   ```

   Prefix each result with `sha256-` and fill in both architecture entries.
   Do not use `nix-prefetch-url` when store permissions would require sudo.
3. After the nonprivileged validation in section 5, open one visible terminal
   task for final verification. If sudo is necessary, group all privileged
   commands in one invocation so the user only authenticates once. Never
   request their password in chat, capture it, or bypass authentication.
4. In that task, first verify both UEFI derivations using the development
   shell's pinned nixpkgs, explicitly selecting both architectures. This
   confirms the computed hashes against real `fetchzip` behavior even if
   another shell dependency is broken. The expression has this shape, with
   `PINNED_NIXPKGS_URL` replaced by the current shell's pin:

   ```bash
   nix-build --no-out-link --expr '
     let
       pkgs = import (builtins.fetchTarball "PINNED_NIXPKGS_URL") {};
     in {
       x64 = pkgs.callPackage ./nix/uefi_mu_msvm.nix {
         targetArch = "x86_64";
       };
       aarch64 = pkgs.callPackage ./nix/uefi_mu_msvm.nix {
         targetArch = "aarch64";
       };
     }'
   ```

   Verify both builds succeed and install nonempty `MSVM.fd` files matching
   their release archives. Then invoke `nix-shell` exactly once in the same
   task. Its `--run` command should verify that `NIX_UEFI_X64` and
   `NIX_UEFI_AARCH64` point to the expected, nonempty firmware files.
5. If the full shell fails on an unrelated dependency, report that blocker
   and leave unrelated hashes unchanged. Do not rerun the shell or request
   another authentication to rediscover the same failure. Preserve the
   independent UEFI build results and clearly distinguish their success
   from full-shell success. A UEFI hash mismatch is a real update failure:
   correct it and revalidate rather than committing an unverified hash.

## 5. Validate before committing

Run the repository's required checks for each modified package. For the usual
version-only Rust change, the package is `flowey_lib_hvlite`:

```bash
cargo check -p flowey_lib_hvlite
cargo clippy --all-targets -p flowey_lib_hvlite
cargo doc --no-deps -p flowey_lib_hvlite
cargo nextest run --profile agent -p flowey_lib_hvlite
cargo xtask fmt --fix
```

Use `cargo test -p flowey_lib_hvlite` only if nextest is unavailable. Do not
filter test output. Run formatting last; resolve failures and rerun until it
succeeds. Check that formatting did not introduce unrelated changes.
Then perform the single final Nix verification task described in section 4.

## 6. Commit, push, and stage the PR

1. Stage only intended files. Normally these are the version configuration
   and Nix derivation; include downloader changes only when required.
2. Commit with the exact subject, substituting the new version:

   ```text
   deps: update mu_msvm to v<new-version> release
   ```

   Include the repository-required co-author trailer.
3. Push the branch to `origin`, without force.
4. Create a **draft** PR targeting `microsoft/openvmm:main`, using the commit
   subject as the title. Derive the head owner from the actual origin fork.
   The body must contain only:

   ```markdown
   Changelog: [microsoft/mu_msvm@v<previous-version>...v<new-version>](https://github.com/microsoft/mu_msvm/compare/v<previous-version>...v<new-version>)
   ```

   Use the previous Rust ingestion recorded in step 2, not a guessed release
   or the previous Nix version.
5. Verify the published PR's base, head, draft status, description, commits,
   and changed files. Confirm there are no unrelated commits or changes and
   the local commit matches the remote branch.
6. Remove temporary downloads and verification-only configuration that you
   created, without deleting user files. Report the PR link, validation
   results, and any remaining environment blockers.
