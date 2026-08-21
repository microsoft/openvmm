# Packaging OpenVMM for Linux

This page describes the source inputs and build configuration OpenVMM
provides for Linux distribution packages.

## Release inputs

Releases are tagged `openvmm-v<VERSION>`. GitHub automatically
generates the source archive for every release tag. The `.tar.gz` URL
is:

```text
https://github.com/microsoft/openvmm/archive/refs/tags/openvmm-v<VERSION>.tar.gz
```

OpenVMM also uploads one release asset:
`openvmm-<VERSION>-vendor.tar.gz`.

`<VERSION>` is the canonical `[workspace.package] version` in the root
`Cargo.toml`.

GitHub derives the source archive root from the repository and tag
names. For the release tag above, expect
`openvmm-openvmm-v<VERSION>/`. Confirm the root against the downloaded
archive before finalizing distribution setup macros.

The source archive contains no `.git` directory. The workspace version
is already in `Cargo.toml`, so a binary built from the extracted archive
reports the plain workspace version.

The source archive covers the entire tracked tree, including components
a Linux package does not build. Build `-p openvmm` and package only that
binary.

The vendor archive contains exactly these top-level paths:

```text
cargo_config
vendor/
```

`cargo_config` is the exact stdout of
`cargo vendor --locked --versioned-dirs vendor`. The `vendor/` tree
comes from the same invocation, so it includes both crates.io and locked
git dependencies needed by the workspace.

`cargo vendor` operates on the workspace, so the vendor tree includes
dependencies not compiled by the OpenVMM Linux binary.

```admonish note
GitHub guarantees stable extracted source contents when a tag continues
to name the same commit, but its compression settings may change.
Distribution lookaside caches should retain the exact downloaded source
archive and vendor archive used by the package.
```

## Verifying the release

Confirm that the release tag points to the expected commit:

```bash
git ls-remote https://github.com/microsoft/openvmm.git \
    refs/tags/openvmm-v<VERSION>
```

Download the source archive and vendor archive once when updating the
distribution package, record their digests in the distribution's source
metadata, and retain those exact bytes in the distribution's lookaside
cache.

OpenVMM does not upload a checksum file, an OpenPGP signature, or a
provenance attestation for either archive.

## Build requirements

The distribution build requires:

- the Rust toolchain required by the workspace;
- a C compiler and linker;
- glibc development headers;
- Linux UAPI headers;
- OpenSSL development headers;
- `pkg-config`;
- a Protocol Buffers compiler providing `protoc`.

These map to the following distribution packages, which is what
OpenVMM's release validation job installs:

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

## Offline build configuration

Extract the GitHub source archive, then extract the vendor archive into
that source tree:

```bash
tar -xzf openvmm-v<VERSION>.tar.gz
cd openvmm-openvmm-v<VERSION>
tar -xzf ../openvmm-<VERSION>-vendor.tar.gz
```

Append the shipped `cargo_config` bytes to `.cargo/config.toml`. Do not
overwrite the existing file:

```bash
printf '\n' >> .cargo/config.toml
cat cargo_config >> .cargo/config.toml
```

Then build the host `x86_64-unknown-linux-gnu` target with vendored
dependencies:

```bash
export PROTOC="$(command -v protoc)"
export OPENSSL_NO_VENDOR=1
cargo build --release --locked --offline -p openvmm \
    --target x86_64-unknown-linux-gnu
```

```admonish warning
The shipped `.cargo/config.toml` must keep its existing settings. If
your packaging flow has already added a `[source]` table, merge the
vendored-source configuration manually instead of overwriting the file.
```

The vendor tree naturally preserves license files shipped by upstream
dependencies. OpenVMM does not add an aggregate license inventory to the
vendor archive. Downstream tooling may generate feature-aware manifests
from the extracted vendor tree when distribution policy requires them.

## Distribution integration

Mapping GitHub's generated source archive onto a distribution's
conventions is the packager's job, but the points below are the ones
OpenVMM's layout affects directly.

### RPM

GitHub's generated source archive is expected to unpack into
`openvmm-openvmm-v%{version}` rather than `%{name}-%{version}`. Confirm
the root directory, then pass it explicitly:

```spec
%autosetup -n openvmm-openvmm-v%{version}
```

Fedora's [Rust packaging
guidelines](https://docs.fedoraproject.org/en-US/packaging-guidelines/Rust/)
additionally require, for a package like OpenVMM that ships an
executable with statically linked Rust dependencies:

- `%cargo_prep` in `%prep`, after the source and vendor archives are
  unpacked and the vendored source configuration is appended.
- `%cargo_license_summary` (and `%cargo_license`) in `%build` after the
  build, because the package includes a binary target. Any feature flags
  passed to the build must also be passed to these macros, or the
  recorded licenses will not match what was compiled.
- `%cargo_vendor_manifest` in `%build` when policy requires a generated
  manifest for the vendored sources. It writes `cargo-vendor.txt`, which
  can then be shipped as a `%license` file.
- `%cargo_generate_buildrequires` in `%generate_buildrequires` except
  when building only from vendored dependencies.

### Debian

Configure `debian/watch` for the GitHub-generated source archive and
fetch the matching vendor archive as an additional source. See the
[Debian watch documentation](https://wiki.debian.org/debian/watch) for
the GitHub-specific patterns.

Record the vendored crate licenses in `debian/copyright` for the same
reason Fedora may generate a vendor manifest from the extracted tree.

```admonish note
OpenVMM does not publish an OpenPGP signature or `SHA256SUMS` alongside
either archive, so `uscan` signature verification cannot be enabled.
Confirm the release tag and retain the exact downloaded archives used
for the package.
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

Confirm the exact dependencies for the packaged executable with
`readelf` or the distribution's automatic dependency generator. The
expected shared libraries include glibc, OpenSSL (`libssl` and
`libcrypto`), and `libgcc_s`. SQLite is compiled into the binary and
does not add a shared runtime dependency.
