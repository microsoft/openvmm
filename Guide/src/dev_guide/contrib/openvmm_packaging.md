# Packaging OpenVMM for a Linux Distribution

This page describes building the `openvmm` binary as a Linux distribution
package, from public source only. It is aimed at downstream packagers (for
example, an RPM or `.deb` maintainer) who build from the official OpenVMM source
release rather than from the in-repo `cargo xflowey` provisioning flow.

The examples target [Azure Linux](https://github.com/microsoft/azurelinux), but
the requirements generalize to any glibc distribution.

```admonish note title="See also"
[OpenVMM Release Model and Support](./openvmm_release.md) describes the source
release archive and version identity that a distribution package builds from.
[Crypto Backends](./crypto_backends.md) explains the backend selection that
determines the native dependencies below.
```

## What a distribution package builds

A distribution package builds the host `x86_64-unknown-linux-gnu` target:
dynamically linked against the system glibc and OpenSSL. This differs from the
official prebuilt Linux release archives, which are statically linked `musl`
binaries built through the repository's own provisioning tooling.

The gnu build deliberately avoids the repository's `.packages/` provisioning
(`cargo xflowey restore-packages`), which fetches prebuilt native libraries a
distribution build cannot consume. A packager instead supplies every native
dependency from distribution packages and overrides the few build settings that
otherwise point into `.packages/`.

```admonish tip title="Only two overrides are required"
Building the `openvmm` binary for the gnu target needs no source patches. It
needs the distribution build packages below, plus two environment overrides
(`PROTOC` and `OPENSSL_NO_VENDOR`).
```

## Toolchain

The workspace declares a minimum supported Rust version (MSRV) in the root
`Cargo.toml` (`rust-version`). Building requires a Rust toolchain at least that
new. The MSRV advances over time, so confirm the current value in the source
you are packaging and require at least that Rust version in the package.

```admonish warning title="Distribution Rust may lag the MSRV"
If the distribution's packaged `rust` is older than the workspace MSRV, `cargo`
fails during resolution — for example:

    error: package `x86emu@0.0.0` cannot be built because it requires
    rustc 1.95 or newer, while the currently active rustc version is 1.90.0

Target a distribution whose packaged Rust meets the MSRV, or arrange a newer
toolchain in the package build environment.
```

## Native build dependencies

The `openvmm` gnu binary compiles a small amount of C through build scripts and
shells out to `protoc`. The native build dependencies are:

- a C toolchain (`gcc`, the glibc development headers, and `binutils`);
- the Linux UAPI headers (`kernel-headers`), for the bundled SQLite compiled by
  `libsqlite3-sys`;
- the OpenSSL development headers, for `openssl-sys`;
- a Protocol Buffers compiler providing `protoc`, for `prost` / `pbjson`.

```admonish note title="SymCrypt is not needed for the gnu build"
The `crypto` crate selects the OpenSSL backend on `target_os = "linux"` for
non-`musl` targets, and SymCrypt only on `musl`. The `openvmm` gnu binary
therefore does not depend on SymCrypt, `ms-tpm-20-ref`, or `mimalloc` — none of
them appear in `cargo tree -p openvmm` for the host target. Only `musl`
(OpenHCL) builds link SymCrypt. See [Crypto Backends](./crypto_backends.md).
```

### Azure Linux package names

Package names differ between Azure Linux versions:

| Need | Azure Linux 3.0 | Azure Linux 4.0 |
| --- | --- | --- |
| C toolchain | `gcc`, `glibc-devel`, `binutils` | same |
| Linux UAPI headers | `kernel-headers` | `kernel-headers` |
| OpenSSL headers | `openssl-devel` | `openssl-devel` |
| `protoc` | `protobuf` | `protobuf-compiler`, `protobuf-devel` |

Azure Linux 3.0 ships a newer `protoc` that embeds the well-known types, so its
`protobuf` package is sufficient. Azure Linux 4.0 ships an older `protoc`, so
`protobuf-devel` is also required to supply `google/protobuf/*.proto` on disk.

## Environment overrides

The in-repo `.cargo/config.toml` sets `PROTOC` to a path under `.packages/`
unconditionally. A distribution build must point it at the system `protoc`.
Setting `OPENSSL_NO_VENDOR` makes `openssl-sys` link the system OpenSSL rather
than building a vendored copy:

```bash
export PROTOC="$(command -v protoc)"
export OPENSSL_NO_VENDOR=1
```

## Offline vendored build

A package build should be reproducible and offline. Vendor all dependencies —
including the Git dependencies and the `[patch.crates-io]` pins — into a tarball
alongside the source archive:

```bash
cargo vendor vendor/ > vendor-config.toml
```

`cargo vendor` captures the Git dependencies and their Git submodule C sources
(for example the `ms-tpm-20-ref` and SymCrypt submodules), so the vendored tree
is self-contained. Append the generated `[source]` redirection to
`.cargo/config.toml`, then build without network access:

```bash
cargo build --release -p openvmm --offline
```

## Runtime dependencies

The resulting binary links a small, stable set of shared libraries. Confirm the
exact set for your build with `ldd`:

- glibc (`libc`, `libm`);
- OpenSSL (`libssl`, `libcrypto`);
- `libgcc_s`.

Depending on the OpenSSL build, `libz` may also appear transitively. The bundled
SQLite is linked statically and adds no runtime dependency. RPM automatic
dependency generation derives these from the ELF `NEEDED` entries; the
corresponding packages are typically `glibc`, `openssl-libs`, and `zlib`.

## Worked example: Azure Linux RPM

This mirrors the pattern used by the
[`cloud-hypervisor`](https://github.com/microsoft/azurelinux/tree/3.0/SPECS/cloud-hypervisor)
package: a source tarball, a `cargo vendor` tarball, and an offline build. The
distribution package version is independent from the OpenVMM product version
(see [Independent version spaces](./openvmm_release.md#independent-version-spaces)).

Use `openvmm-<VERSION>-source.tar.gz` from the release as `Source0` so the build
retains its release identity through `.openvmm-release.json`. Do not use
GitHub's automatic archive links, which drop that metadata.

Declare the build and runtime dependencies:

```spec
BuildRequires:  rust >= 1.95
BuildRequires:  cargo >= 1.95
BuildRequires:  gcc glibc-devel binutils kernel-headers
BuildRequires:  openssl-devel
BuildRequires:  protobuf-compiler protobuf-devel

Requires:       glibc
Requires:       openssl-libs
Requires:       zlib
```

Build offline and install the single binary:

```spec
%build
export PROTOC="$(command -v protoc)"
export OPENSSL_NO_VENDOR=1
cargo build --release --offline -p openvmm \
    --target x86_64-unknown-linux-gnu

%install
install -D -m0755 \
    target/x86_64-unknown-linux-gnu/release/openvmm \
    %{buildroot}%{_bindir}/openvmm
```

On Azure Linux 3.0, replace `protobuf-compiler protobuf-devel` with `protobuf`,
and confirm the packaged `rust` meets the workspace MSRV before adopting a base
version.
