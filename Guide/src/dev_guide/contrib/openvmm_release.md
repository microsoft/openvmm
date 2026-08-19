# Cutting an OpenVMM release

This page explains how maintainers create an OpenVMM release on GitHub.

For the packager's view of the shipped inputs, see
[Packaging OpenVMM for Linux](./openvmm_packaging.md).

## What a release is

An OpenVMM release is a GitHub release tagged `openvmm-v<VERSION>`.

The release workflow produces:

- GitHub-generated source archives for the release tag;
- `openvmm-<VERSION>-vendor.tar.gz` for offline Cargo builds;
- `openvmm-x86_64-unknown-linux-musl`;
- `openvmm-aarch64-unknown-linux-musl`;
- unsigned x64 and ARM64 Windows workflow artifacts.

That vendor archive contains the vendored Cargo dependency tree and the
exact `cargo vendor` source-replacement snippet needed for offline
builds. No custom source archive is assembled or uploaded.

The Linux executables are raw musl binaries. They are not wrapped in an
archive, and Linux `.dbg` files are not published.

The Windows artifacts are named
`openvmm-x86_64-pc-windows-msvc-unsigned` and
`openvmm-aarch64-pc-windows-msvc-unsigned`. Each contains `openvmm.exe`
and its PDB when the build naturally produces one.

```admonish warning
Windows signing is not integrated yet. The unsigned Windows builds remain
GitHub Actions workflow artifacts and are not uploaded to the GitHub release.
```

`<VERSION>` is the `version` field under `[workspace.package]` in the
OpenVMM repository's root `Cargo.toml`. Nothing is stamped into the
source archive, and the tree is never rewritten, so the version a
release publishes is the version that was already reviewed and merged.

```admonish note
The published binaries are built from a Git checkout, so `openvmm -V`
reports `<VERSION>+g<COMMIT>` rather than plain `<VERSION>`. A locally
rebuilt checkout of the release commit reports the same identity. See the
[CLI reference](../../reference/openvmm/management/cli.md) for the full
`--version` behavior.
```

## Selecting the version

Open an ordinary pull request that sets that field to the version being
released, and merge it. That review *is* the decision to release that
version; the release workflow only ever publishes what the tree already
says.

If the committed version has never been released and is already the
intended value, the pull request may instead state explicitly that it
selects that existing version. A no-op edit to `Cargo.toml` is not
required.

```admonish note
The version stays at the released value after publication. Commits made
afterwards report `<VERSION>+g<COMMIT>` and are identifiable by the
appended commit, so there is no second commit to "reopen" the
version.
```

## Running the workflow

Dispatch the **OpenVMM Release** workflow against the commit to release. It
has no automatic triggers; it only runs when someone asks for it.

The workflow:

1. reads `[workspace.package] version` and the commit from the checkout;
2. assembles `openvmm-<VERSION>-vendor.tar.gz` once with
   `cargo vendor --locked --versioned-dirs`;
3. checks out the same revision, appends the generated `cargo_config`
   to `.cargo/config.toml`, and builds `openvmm` with
   `--locked --offline`;
4. builds release-mode OpenVMM binaries for x64 and ARM64 Linux musl and
   Windows MSVC targets;
5. uploads both unsigned Windows builds as architecture-specific workflow
   artifacts;
6. creates or verifies `openvmm-v<VERSION>` at that commit, after
   confirming no release already exists for it;
7. creates one **draft** release for that tag, relies on GitHub's automatic
   source archives, and uploads the vendor archive and both Linux binaries.

The workflow does not upload `SHA256SUMS` or a provenance attestation.
It does not upload unsigned Windows binaries to the release.

## Publishing

Before publishing, confirm the draft still targets the commit the
workflow pinned. The workflow logs that commit when it creates
`openvmm-v<VERSION>`, and
`git ls-remote origin refs/tags/openvmm-v<VERSION>` reports where the
tag points now. They must match.

That check is the one thing publication cannot do for you. A draft
release tracks a tag *name*, not a commit, and GitHub enforces
immutability only after publication. Creating the tag up front means
publication cannot invent a tag at some other commit, but nothing in the
workflow prevents someone from moving an existing tag while the draft
sits in review.

Then review the draft, write its notes, and click **Publish release**.

```admonish warning
The tag is publicly visible while the release is still a draft. Do not
move or delete it during review. Publishing is the irreversible step: a
published tag is not moved or deleted. Correcting a release means
merging a pull request that selects a new patch version and running the
workflow again.
```

The workflow fails rather than overwriting an existing release for the
same tag, since that release may already have been reviewed or
published. It checks for that release before creating the tag, so a
refused run does not leave a tag behind. If a run must be retried before
publication, it reuses the tag only when the tag still names the exact
pinned commit. A maintainer may delete the draft and keep the matching
tag. Deleting the tag is reserved for exceptional pre-publication
cleanup.

## Limitations

The workflow validates one Linux `openvmm` distribution build with the
uploaded vendor archive. It does not add binary smoke tests or additional
release validation. Distribution-specific packaging steps still belong to
the downstream package.

```admonish note
OpenVMM does not publish an OpenPGP signature, `SHA256SUMS`, or a provenance
attestation for the generated source archives, vendor archive, or Linux
binaries. Consumers can confirm that the release tag names the expected
commit and retain the exact downloaded bytes they use.
```
