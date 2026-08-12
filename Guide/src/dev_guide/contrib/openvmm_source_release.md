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

`<VERSION>` is the `[workspace.package] version` in the root
`Cargo.toml`. Nothing is stamped into the archive and the tree is never
rewritten, so the version a release publishes is the version that was
already reviewed and merged.

## Selecting the version

Open an ordinary pull request that sets `[workspace.package] version` to
the version being released, and merge it. That review *is* the decision
to release that version; the release workflow only ever publishes what
the tree already says.

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
3. attests both files and attaches them to a **draft** release.

Validation runs before anything is attached, so a source tree that a
distribution cannot build never reaches a draft.

## Publishing

Review the draft release, write its notes, and click **Publish release**.
GitHub creates the `openvmm-v<VERSION>` tag at the commit the workflow
pinned.

```admonish warning
Publishing is the irreversible step. A published tag and its assets are
not moved or replaced. Correcting a release means merging a pull request
that selects a new patch version and running the workflow again.
```

The workflow fails rather than overwriting an existing release for the
same tag, since that release may already have been reviewed or published.

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
