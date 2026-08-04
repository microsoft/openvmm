# OpenVMM Standalone Source Release Proposal

This page proposes how OpenVMM should identify builds and publish standalone
source releases for Linux distributions.

```admonish important title="Request for consensus"
This is a design proposal, not current release policy. The implementation
should be split into separately reviewed phases only after maintainers agree
on the decisions below.
```

## Goals

The proposal aims to:

- publish a source archive that distributions can build without repository
  metadata or project-specific dependency provisioning;
- validate the exact source archive before it is published;
- give official releases a stable version;
- make development builds distinguishable when useful;
- keep publication manual and reviewable while the process is new;
- avoid release branches, source rewriting, and a two-commit version dance.

The first release phase publishes source only. Prebuilt binaries and a
long-term servicing policy are out of scope.

## Proposed release flow

```text
reviewed version change merges to main
                  |
                  v
maintainer manually starts OpenVMM Release
                  |
                  v
workflow pins one commit and validates release policy
                  |
                  v
assemble source archive and SHA256SUMS once
                  |
                  v
build those exact bytes in the distribution configuration
                  |
                  v
attest and attach those same bytes to a draft GitHub release
                  |
                  v
maintainer reviews the draft and clicks Publish release
                  |
                  v
GitHub creates openvmm-v<VERSION> at the pinned commit
```

Publishing the draft is the irreversible step. A published tag and its assets
would not be moved or replaced. A correction would use a normal reviewed pull
request to select a new patch version, followed by a new release.

## Proposed source artifact

The release would contain:

- `openvmm-<VERSION>-source.tar.gz`;
- `SHA256SUMS`;
- GitHub build provenance attestations for both published files.

The archive would be a deterministic export of the tracked tree at one commit,
rooted at `openvmm-<VERSION>/`. It would not contain `.git`, prebuilt native
dependencies, vendored Rust crates, or pipeline-generated version metadata.

The version would already be present in the root `Cargo.toml`. Release assembly
would not rewrite the tree or inject a second copy of the version.

## Proposed distribution-build gate

The release workflow would assemble the archive once and transfer it through
validation and publication as an internal workflow artifact. The distribution
gate would:

1. consume the exact archive intended for publication;
2. extract it outside the repository checkout;
3. run `cargo build --release --locked -p openvmm` using system dependencies.

The initial gate would answer one question: can a distribution build the source
artifact without relying on the project checkout or project-provisioned native
dependencies?

The standalone GNU/Linux build does not use `openvmm-deps`. CI would install
the distribution's C toolchain, Linux headers, OpenSSL development package,
`pkg-config`, and Protocol Buffers compiler. OpenHCL, test, and firmware assets
from `openvmm-deps` are outside this build.

Checksum verification, an explicit `.git` assertion, binary-version
validation, and direct OpenSSL linkage inspection are possible follow-up
checks. They should be added only when maintainers agree that each check
enforces a release requirement worth owning. In particular, binary-version
validation depends on the unresolved build-identity design and is not part of
the initial gate.

Normal pull-request CI would run the same assembly and distribution-build
logic against the commit under test. It must independently assemble its own
snapshot because no release preparation job exists in ordinary CI.

## Decisions requiring consensus

The statuses below record the proposal author's current direction. **Chosen
direction** records a decision supported by current maintainer feedback.
**Open question** means maintainers are specifically being asked to choose
between alternatives. **Proposed direction** means feedback is still welcome,
but the RFC recommends that choice.

### 1. Canonical product version

**Status: Proposed direction**

**Proposal:** Store a stable `MAJOR.MINOR.PATCH` in
`[workspace.package] version` in the root `Cargo.toml`. Keep the most recently
released version until a reviewed pull request selects the next version.

This makes the version available to Cargo and to downstream builders without
requiring Git metadata.

### 2. Development-build identity

**Status: Chosen direction**

**Proposal:** A normal Git checkout reports
`<VERSION>+g<9-character-commit>`, identified as a development build.

This distinguishes commits made after the latest release even while the
committed product version remains unchanged and makes the source commit obvious
in concise version output.

### 3. Git checkout classification

**Status: Chosen direction**

**Proposal:** Treat every Git checkout as development, including an exact
checkout of an `openvmm-v<VERSION>` release tag.

Only a Git-free source tree reports plain `<VERSION>`. Release tags remain
publication markers and are not build-identity inputs. This avoids special tag
detection and ensures that locally rebuilt checkouts never claim official
release identity.

### 4. Build from an extracted archive

**Status: Proposed direction**

**Proposal:** A build with no applicable Git repository reports plain
`<VERSION>` as a release-shaped build.

The published archive necessarily lacks `.git`, so the committed Cargo version
is the only identity available.

This classification is descriptive, not proof that arbitrary Git-free source
is official. Consumers must verify the source archive's checksum and
provenance attestation.

**Alternative:** Require the release pipeline or packager to set an explicit
official-build variable.

The alternative makes official status explicit but requires mutable build
inputs and makes rebuilding the unmodified published archive behave
differently unless every packager reproduces the release environment.

### 5. Distribution package override

**Status: Proposed direction**

**Proposal:** Do not add a package-version override. The OpenVMM binary reports
the committed product version, while a distribution records its package
revision in its own package metadata.

This is independent of release identity. Builds from the published archive
already recover the committed Cargo version without an environment variable.

**Alternative:** Add an `OPENVMM_PKGVERSION` environment variable that replaces
the displayed identity with builder-supplied text and classifies the result as
a custom build.

The alternative gives downstream packagers another identity surface to manage
and is not required to build an official source archive.

### 6. Identity integration surfaces

**Status: Proposed direction**

**Proposal:** Limit the initial implementation to concise `openvmm -V` and
detailed `openvmm --version` output.

Startup telemetry, saved-state metadata, an extractable binary metadata
section, and Windows VERSIONINFO changes would be separate follow-up proposals.
They should be added only when their consumers and value are clear.

### 7. Manual draft publication

**Status: Proposed direction**

**Proposal:** A manually dispatched workflow creates a draft GitHub release.
A maintainer reviews the ordinary GitHub draft and clicks **Publish release**,
which creates the tag at the workflow's pinned commit.

**Alternative:** Push the tag first and trigger release automation from it.

Publishing the draft last avoids creating an official tag before archive
validation succeeds. The tradeoff is that the release workflow must validate
tag availability and rely on a human for the final action.

## Proposed implementation phases

After consensus, implementation would be divided into independently reviewed
pull requests:

1. establish the canonical Cargo product version;
2. implement only the agreed build-identity behavior and integrations;
3. add deterministic source assembly and the distribution-build CI gate;
4. add generic GitHub release and provenance helpers;
5. add the manual OpenVMM release workflow and maintainer documentation.

Generated workflow files would land with the Flowey source that produces them.
Each phase would remain buildable and testable before the next phase begins.

## Review guidance

Reviewers should focus first on the seven decisions above rather than detailed
implementation. In particular:

- Are there objections to Git-free archive builds using the committed version?
- Is there a demonstrated need for a downstream package-version override?
- Are CLI outputs sufficient for the initial identity implementation?
- Are there objections to manual draft publication as the initial safety
  boundary?

Implementation details should be revised or removed when they do not follow
from an accepted decision.
