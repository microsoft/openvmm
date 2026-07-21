# OpenVMM Release Model and Support

This page describes standalone OpenVMM releases, support policy, source-build
identity, and the maintainer release runbook. OpenVMM versions are independent
from OpenHCL servicing versions.

## One repository, two products

OpenVMM and OpenHCL share a repository and substantial code, but they are
separate products with different release and support policies.

| Product | Release and support model |
| --- | --- |
| OpenVMM | Cross-platform Virtual Machine Monitor. Continuous development on `main`, with standalone releases identified by OpenVMM-specific tags. |
| OpenHCL | Paravisor built on OpenVMM. Releases and in-market servicing use selected long-lived release branches. |

```admonish note title="See also"
[OpenHCL Releases & Code Flow](./release.md) describes the OpenHCL release
branch and backport process.
```

## Pre-1.0 cadence and support

Before `1.0`, maintainers target approximately one OpenVMM release per month
from a healthy `main` commit. A release may be delayed or skipped when the
candidate does not meet the quality bar. Calendar months are not encoded into
the product version.

Normal releases advance the minor version:

```text
0.1.0
0.2.0
0.3.0
```

Patch releases advance the patch version:

```text
0.2.1
0.2.2
```

Only the newest OpenVMM release is supported initially. A supported release
receives a patch only for a security issue or release-blocking regression.
Once a newer normal release ships, the previous line leaves normal support.

A `0.x.y` version does not promise backward compatibility. APIs, command-line
behavior, device models, snapshot formats, and other interfaces may change
between releases.

## Release tags

Git tags are the only source of public OpenVMM semantic versions. There is no
checked-in OpenVMM product-version file.

Release tags use:

```text
openvmm-vMAJOR.MINOR.PATCH
```

Examples:

```text
openvmm-v0.1.0
openvmm-v0.1.1
openvmm-v0.2.0
```

Tags must contain exactly three canonical unsigned numeric components.
Prerelease suffixes, build metadata, leading zeroes, and additional components
are rejected.

Normal `MAJOR.MINOR.0` tags must point to commits reachable from `main`. A patch
tag must descend from its immediate predecessor. For example,
`openvmm-v0.2.2` must descend from `openvmm-v0.2.1`.

More than one `openvmm-v*` tag on the same commit is an error. Pushed release
tags are immutable.

## Build identity

OpenVMM resolves its displayed version from source identity:

| Source state | Displayed version |
| --- | --- |
| Clean exact `openvmm-v0.2.0` tag | `0.2.0` |
| Dirty exact tag | `0.2.0+dirty` |
| Clean untagged Git checkout | `0.0.0-dev+g012345678` |
| Dirty untagged Git checkout | `0.0.0-dev+g012345678.dirty` |
| No usable Git or generated release metadata | `0.0.0-dev` |

More than one OpenVMM release tag on `HEAD` fails the build rather than
selecting an arbitrary version.

`openvmm --version` prints the concise displayed version. The full source
revision remains available as separate embedded build information.

A source tree with neither Git metadata nor generated release metadata warns
that `0.0.0-dev` is not an official release identity.

Windows numeric version resources use `MAJOR.MINOR.PATCH.0` for an official
release and `0.0.0.0` for a development build.

### Independent version spaces

The OpenVMM product version is independent from:

- internal Cargo crate versions;
- OpenHCL release and servicing versions;
- distribution package versions;
- CI build counters;
- source revision identifiers.

Build and distribution systems may record package versions, build IDs, and
source revisions as additional metadata. They must not present those values as
the OpenVMM product version.

## Release assets

The initial release contains:

- `openvmm-<VERSION>-windows-x64.zip`;
- `openvmm-<VERSION>-windows-x64-symbols.zip`;
- `openvmm-<VERSION>-windows-arm64.zip`;
- `openvmm-<VERSION>-windows-arm64-symbols.zip`;
- `openvmm-<VERSION>-linux-x64-musl.tar.gz`;
- `openvmm-<VERSION>-linux-x64-musl-symbols.tar.gz`;
- `openvmm-<VERSION>-linux-arm64-musl.tar.gz`;
- `openvmm-<VERSION>-linux-arm64-musl-symbols.tar.gz`;
- `openvmm-<VERSION>-source.tar.gz`;
- `SHA256SUMS`.

Runtime archives contain the runnable binary and `LICENSE`. Symbol archives
contain the matching debug symbols and `LICENSE`. Linux runtime binaries retain
executable permissions.

`SHA256SUMS` covers all nine archives. Every archive and `SHA256SUMS` receives a
public GitHub build provenance attestation before the release is published.

```admonish warning title="Windows signing is not implemented"
The public release workflow does not Authenticode-sign Windows artifacts. Do
not describe downloaded Windows executables as signed.
```

## Building a release from source

A real Git checkout with the exact release tag available derives its version
directly from Git:

```bash
git clone https://github.com/microsoft/openvmm.git
cd openvmm
git checkout openvmm-v0.2.0
```

Once the release automation lands, each release will also publish an official
attested source archive. The archive will contain `.openvmm-release.json` at
its root, recording the metadata schema, release version, release tag, and full
source revision. Builds from this archive retain the exact release identity
without a `.git` directory.

```admonish warning title="Use the official source archive"
GitHub's automatic "Source code (zip)" and "Source code (tar.gz)" links omit Git
metadata and do not preserve the OpenVMM release identity. They are convenience
snapshots, not supported version-preserving build inputs. Use a real checkout of
the release tag or `openvmm-<VERSION>-source.tar.gz`.
```

## Normal release runbook

```admonish note
The tag-triggered release automation described below is under development. Do
not create a public OpenVMM release tag until that implementation has landed.
```

Tag creation is the release decision. A successful workflow publishes
automatically after building, packaging, checksumming, and attesting every
asset. There is no separate manual approval or draft-review phase.

### 1. Select the release

Choose a healthy commit already on `main`. Confirm that:

- required changes are merged;
- required CI checks pass;
- the next normal version follows the release sequence;
- the version has not already been released;
- the commit is suitable for public release.

Do not create the tag while required changes or checks are outstanding.

### 2. Create and push the tag

Update local `main`, confirm the working tree is clean, and create an annotated
tag. For example:

```bash
git switch main
git pull --ff-only
git status --short
version="0.2.0"
tag="openvmm-v${version}"
git tag -a "${tag}" -m "OpenVMM ${version}"
git push origin "${tag}"
```

Do not move, delete, or recreate a pushed release tag.

### 3. Monitor automatic publication

The tag starts the OpenVMM release workflow. The workflow:

1. validates the tag and commit topology;
2. builds Windows x64, Windows ARM64, Linux musl x64, and Linux musl ARM64;
3. creates separate runtime and symbol archives;
4. creates the official source archive;
5. creates `SHA256SUMS`;
6. attests every archive and the checksum file;
7. publishes a non-draft GitHub Release with generated notes.

If a build, packaging, checksum, or attestation step fails, no public release
is created. The same immutable tag may be rerun after a pipeline or
infrastructure failure when no release was published.

If a public release already exists, the workflow refuses to replace its assets.
Correct a bad published release with a new version rather than mutating it.

### 4. Confirm the release

After the workflow succeeds, confirm that:

- the release page points to the intended tag and revision;
- all four targets have separate runtime and symbol archives;
- the official source archive is present;
- every runtime and symbol archive contains `LICENSE`;
- `SHA256SUMS` covers all nine archives;
- every archive and `SHA256SUMS` has a provenance attestation;
- generated notes cover the intended pull requests;
- Windows assets are not described as signed;
- the release is presented as the newest supported OpenVMM release.

After downloading all assets into one directory, verify the checksums:

```bash
sha256sum -c SHA256SUMS
```

Verify an asset's provenance with the GitHub CLI:

```bash
gh attestation verify path/to/<ASSET> --repo microsoft/openvmm
```

Smoke-test runnable archives on compatible hosts where practical. At minimum,
confirm that the executable starts and reports the expected version.

## Patch release runbook

OpenVMM does not create a release branch for every normal release. Create a
patch branch only when the currently supported release requires a security or
release-blocking fix.

1. Implement and merge the fix into `main`.
2. Create a temporary patch branch from the currently supported release tag.
3. Cherry-pick the fix onto that branch.
4. Validate the branch and the affected release artifacts.
5. Tag the corrected commit with the next patch version.
6. Push the tag and monitor the normal automatic release workflow.

For example:

```bash
git switch main
git pull --ff-only
git switch -c patch/openvmm-0.2.x openvmm-v0.2.0
git cherry-pick <FIX_COMMIT>
git tag -a openvmm-v0.2.1 -m "OpenVMM 0.2.1"
git push origin openvmm-v0.2.1
```

Further patches must branch from or include their immediate predecessor. For
example, `openvmm-v0.2.2` must descend from `openvmm-v0.2.1`.

The patch branch is not a new general development branch. New work continues
on `main`, and the older line leaves support when the next normal release
ships.

## How fixes flow

Shared fixes always land in `main` first.

For OpenHCL, maintainers cherry-pick a fix to each live OpenHCL release branch
that requires it.

For OpenVMM, the fix ships in the next normal release unless the currently
supported release qualifies for an on-demand patch.

## Future policy

The project may later designate selected releases for longer support, but no
LTS policy exists yet. Before supporting multiple OpenVMM lines or publishing
`1.0.0`, maintainers must document the compatibility, deprecation,
support-window, and servicing commitments.

Public nightly releases are deferred until a concrete consumer requires them.
Ordinary CI artifacts remain available for engineering use.
