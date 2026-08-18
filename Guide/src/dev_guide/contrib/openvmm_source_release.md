# Cutting an OpenVMM source release

This page explains how maintainers create an OpenVMM source release on GitHub.

For the packager's view of the shipped inputs, see
[Packaging OpenVMM for Linux](./openvmm_packaging.md).

## What a release is

An OpenVMM source release is a GitHub release tagged
`openvmm-v<VERSION>`.

GitHub automatically provides the source archive for that tag. OpenVMM
uploads exactly one additional asset:
`openvmm-<VERSION>-vendor.tar.gz`.

That vendor archive contains the vendored Cargo dependency tree and the
exact `cargo vendor` source-replacement snippet needed for offline
builds. No custom source archive is assembled or uploaded.

A release publishes source. Prebuilt binaries, and any commitment to
service a published version, are outside this process.

`<VERSION>` is the `version` field under `[workspace.package]` in the
OpenVMM repository's root `Cargo.toml`. Nothing is stamped into the
source archive, and the tree is never rewritten, so the version a
release publishes is the version that was already reviewed and merged.

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
afterwards report `<VERSION>+g<COMMIT>` and are identifiable as
development builds, so there is no second commit to "reopen" the
version.
```

## Running the workflow

Dispatch the **OpenVMM Source Release** workflow against the commit to
release. It has no automatic triggers; it only runs when someone asks
for it.

The workflow:

1. reads `[workspace.package] version` and the commit from the checkout;
2. assembles `openvmm-<VERSION>-vendor.tar.gz` once with
   `cargo vendor --locked --versioned-dirs`;
3. checks out the same revision, appends the generated `cargo_config`
   to `.cargo/config.toml`, and builds `openvmm` with
   `--locked --offline`;
4. creates or verifies `openvmm-v<VERSION>` at that commit, after
   confirming no release already exists for it;
5. creates a **draft** release for that tag, relies on GitHub's
   automatic source archive, and uploads only the vendor archive.

The workflow does not upload `SHA256SUMS` or a provenance attestation.

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

The workflow validates one Linux `openvmm` build with the uploaded
vendor archive. Distribution-specific packaging steps still belong to
the downstream package.

```admonish note
OpenVMM does not publish an OpenPGP signature, `SHA256SUMS`, or a
provenance attestation for either the GitHub-generated source archive or
the uploaded vendor archive. Consumers can confirm that the release tag
names the expected commit and retain the exact downloaded bytes they
package.
```
