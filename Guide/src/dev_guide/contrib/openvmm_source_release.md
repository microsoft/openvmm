# Cutting an OpenVMM source release

This page is for maintainers publishing an OpenVMM source release. For
the packager's view of what a release contains, see [Packaging OpenVMM
for Linux](./openvmm_packaging.md).

## What a release is

A release is a GitHub release tagged `openvmm-v<VERSION>` carrying two
files:

- `openvmm-<VERSION>.tar.gz`, a deterministic export of the tracked tree
  at one commit;
- `SHA256SUMS`, covering that archive.

Both files also get a GitHub build provenance attestation.

A release publishes source. Prebuilt binaries, and any commitment to
service a published version, are outside this process.

`<VERSION>` is the `version` field under `[workspace.package]` in the
OpenVMM repository's root `Cargo.toml`. Nothing is stamped into the archive
and the tree is never rewritten, so the version a release publishes is the
version that was already reviewed and merged.

## Selecting the version

Open an ordinary pull request that sets that field to the version being
released, and merge it. That review *is* the decision to release that version;
the release workflow only ever publishes what the tree already says.

If the committed version has never been released and is already the
intended value, the pull request may instead state explicitly that it
selects that existing version. A no-op edit to `Cargo.toml` is not
required.

```admonish note
The version stays at the released value after publication. Commits made
afterwards report `<VERSION>+g<COMMIT>` and are identifiable as
development builds, so there is no second commit to "reopen" the version.
```

## Running the workflow

Dispatch the **OpenVMM Source Release** workflow against the commit to
release. It has no automatic triggers; it only runs when someone asks for
it.

The workflow:

1. assembles the archive and `SHA256SUMS` once, pinning the commit;
2. builds that exact archive the way a Linux distribution would, using
   system dependencies rather than the repository's `.packages/`
   provisioning;
3. creates or verifies `openvmm-v<VERSION>` at the pinned commit, after
   confirming no release already exists for it;
4. attests both files and attaches them to a **draft** release that requires
   that existing tag.

Validation runs before anything is attached, so a source tree that a
distribution cannot build never reaches a draft.

## Publishing

Before publishing, confirm the draft still targets the commit the workflow
pinned. The workflow logs that commit when it creates `openvmm-v<VERSION>`, and
`git ls-remote origin refs/tags/openvmm-v<VERSION>` reports where the tag points
now. They must match.

That check is the one thing publication cannot do for you. A draft release
tracks a tag *name*, not a commit, and GitHub enforces immutability only after
publication. Creating the tag up front means publication cannot invent a tag at
some other commit, but nothing in the workflow prevents someone from moving an
existing tag while the draft sits in review.

Then review the draft, write its notes, and click **Publish release**.

```admonish warning
The tag is publicly visible while the release is still a draft. Do not move or
delete it during review. Publishing is the irreversible step: a published tag
and its assets are not moved or replaced. Correcting a release means merging a
pull request that selects a new patch version and running the workflow again.
```

The workflow fails rather than overwriting an existing release for the
same tag, since that release may already have been reviewed or published. It
checks for that release before creating the tag, so a refused run does not
leave a tag behind.
If a run must be retried before publication, it reuses the tag only when the
tag still names the exact pinned commit. A maintainer may delete the draft and
keep the matching tag. Deleting the tag is reserved for exceptional
pre-publication cleanup.

## Limitations

The distribution gate answers one question: can a distribution build the
published archive without the repository's own dependency provisioning?
It does not re-verify `SHA256SUMS`, assert that the extracted tree has
no `.git`, check the version the built binary reports, or inspect how
that binary links OpenSSL. Each is a reasonable check to add, but each
should be added only when maintainers agree it enforces a release
requirement worth owning.

```admonish note
OpenVMM does not yet publish an OpenPGP signature alongside the archive,
so Debian's `uscan` signature verification is unavailable. Consumers can
verify the checksum and the provenance attestation in the meantime.
```
