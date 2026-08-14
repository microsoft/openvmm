# Packaging OpenVMM for Linux

This page describes the source archive and build configuration OpenVMM
provides for Linux distribution packages.

## Source archive

The Flowey source-archive node exports the tracked repository tree at
`HEAD` under an `openvmm-<VERSION>/` prefix. `<VERSION>` is the canonical
`[workspace.package] version` in the root `Cargo.toml`.

The archive is named `openvmm-<VERSION>.tar.gz` and unpacks into
`openvmm-<VERSION>/`. The filename and the root directory deliberately
match `%{name}-%{version}`, so RPM's `%autosetup` and Fedora's forge
macros work without a `-n` override or a renamed source.

Archive assembly uses `git archive` with a fixed mode mask and `gzip -n`,
so repeated assembly at the same commit produces the same
`openvmm-<VERSION>.tar.gz` bytes. The assembly also generates
`SHA256SUMS`.

The archive contains no `.git` directory and does not stamp a second
version into the source. Consequently, a binary built from the extracted
archive reports the plain workspace version.

The export covers the entire tracked tree, including components a Linux
package does not build — OpenHCL, firmware, and test infrastructure
among them. The root `Cargo.toml` defines a workspace spanning those
paths, so narrowing the archive means restructuring the workspace rather
than filtering files out of the export. Build `-p openvmm` and package
only that binary.

```admonish note
Archive bytes are a function of the commit, not of when the archive was
built. `git archive` takes file timestamps from the commit, and `gzip -n`
omits the compression timestamp. Setting `SOURCE_DATE_EPOCH` during
assembly is therefore unnecessary; it would not change the output.
```

## Verifying the archive

Releases are published on GitHub under the tag `openvmm-v<VERSION>`. Each
one carries the archive, `SHA256SUMS`, and a GitHub build provenance
attestation for both files.

`SHA256SUMS` covers the archive under the archive's own name:

```bash
sha256sum --check SHA256SUMS
```

The attestation ties those bytes back to the workflow run that produced
them, and can be checked independently of the checksum:

```bash
gh attestation verify openvmm-<VERSION>.tar.gz --repo microsoft/openvmm
```

Pin that digest in the distribution package rather than re-downloading
the archive at build time. Do not substitute GitHub's automatically
generated source links: those are produced on demand and are not
guaranteed to stay byte-identical, so their digests are unsuitable for
pinning.

A build with no `.git` reports the plain product version, because the
committed Cargo version is the only identity available to it. That
describes the build's inputs; it is not evidence that the source is
official, since any Git-free copy of the tree reports the same version.
The checksum and the attestation are what establish provenance.

## Build requirements

The distribution build requires:

- the Rust toolchain required by the workspace;
- a C compiler and linker;
- glibc development headers;
- Linux UAPI headers;
- OpenSSL development headers;
- `pkg-config`;
- a Protocol Buffers compiler providing `protoc`.

These map to the following distribution packages, which is what OpenVMM's
own distribution-build gate installs:

| Requirement | Debian / Ubuntu | Fedora |
| --- | --- | --- |
| C compiler and linker | `build-essential` | `gcc`, `binutils` |
| Linux UAPI headers | `linux-libc-dev` | `kernel-headers` |
| OpenSSL development headers | `libssl-dev` | `openssl-devel` |
| `pkg-config` | `pkg-config` | `pkgconf-pkg-config` |
| `protoc` | `protobuf-compiler` | `protobuf-compiler` |

Do not use `cargo xflowey restore-packages` when building a distribution
package. That command restores prebuilt native dependencies intended for
repository development.

## Build configuration

Build the host `x86_64-unknown-linux-gnu` target dynamically linked
against the distribution's glibc and OpenSSL:

```bash
export PROTOC="$(command -v protoc)"
export OPENSSL_NO_VENDOR=1
cargo build --release --locked -p openvmm \
    --target x86_64-unknown-linux-gnu
```

OpenVMM CI assembles the source archive, extracts it outside the
repository checkout, and runs this command with distribution-provided
native dependencies. This gate prevents repository-only `.packages/`
dependencies from becoming accidental packaging requirements.

## Offline builds

The source archive contains project source, not a vendored Cargo
dependency tree. A distribution that requires an offline build should
vendor dependencies separately and cover that vendor archive with its own
integrity metadata.

Create the vendor tree:

```bash
cargo vendor vendor/ > vendor-config.toml
```

Append the generated source replacement configuration to
`.cargo/config.toml`, then build offline:

```bash
cargo build --release --locked --offline -p openvmm \
    --target x86_64-unknown-linux-gnu
```

`cargo vendor` operates on the workspace, so the vendor tree includes
dependencies not compiled by the OpenVMM Linux binary.

```admonish warning
Distributions treat a vendored Rust build as statically linked
third-party source, and require the licenses of the vendored crates to be
recorded in the binary package. Generating the vendor tree is not
sufficient on its own; see the distribution-specific requirements below.
```

## Distribution integration

OpenVMM publishes an upstream source archive and its checksum. Mapping
those onto a distribution's own conventions is the packager's job, but
the points below are the ones OpenVMM's layout affects directly.

### RPM

Because the archive name and root directory both match
`%{name}-%{version}`, `%autosetup` needs no arguments beyond the
defaults.

Fedora's [Rust packaging
guidelines](https://docs.fedoraproject.org/en-US/packaging-guidelines/Rust/)
additionally require, for a package like OpenVMM that ships an executable
with statically linked Rust dependencies:

- `%cargo_prep` in `%prep`, after the source is unpacked.
- `%cargo_license_summary` (and `%cargo_license`) in `%build` after the
  build, because the package includes a binary target. Any feature flags
  passed to the build must also be passed to these macros, or the
  recorded licenses will not match what was compiled.
- `%cargo_vendor_manifest` in `%build` when building with vendored
  dependencies. It writes `cargo-vendor.txt`, which must be shipped as a
  `%license` file in the package containing the executable.
- `%cargo_generate_buildrequires` in `%generate_buildrequires` — except
  when building with vendored dependencies, which is the expected
  configuration for OpenVMM.

### Debian

`debian/watch` must match the published asset rather than GitHub's
generated source links, so that `uscan` retrieves the same bytes covered
by `SHA256SUMS`. See the [Debian watch
documentation](https://wiki.debian.org/debian/watch) for the
GitHub-specific patterns.

Record the vendored crate licenses in `debian/copyright` for the same
reason Fedora requires a vendor manifest.

```admonish note
OpenVMM does not currently publish an OpenPGP signature alongside the
archive, so `uscan` signature verification cannot be enabled. Verify the
archive against `SHA256SUMS` instead.
```

## Package identity

The OpenVMM binary reports the upstream product version committed in the
source tree. Record a distribution-specific package revision in the
distribution package metadata rather than replacing the binary version.

OpenVMM deliberately offers no environment variable or build flag that
overrides the reported version. The version is chosen by a reviewed
commit to the root `Cargo.toml`, which keeps a binary's self-reported
identity tied to the source it was built from.

## Runtime dependencies

Confirm the exact dependencies for the packaged executable with `readelf`
or the distribution's automatic dependency generator. The expected shared
libraries include glibc, OpenSSL (`libssl` and `libcrypto`), and
`libgcc_s`. SQLite is compiled into the binary and does not add a shared
runtime dependency.
