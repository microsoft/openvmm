# Packaging OpenVMM for Linux

This page describes the source archive and build configuration OpenVMM provides for Linux distribution packages.

## Source archive

The Flowey source-archive node exports the tracked repository tree at `HEAD` under an `openvmm-<VERSION>/` prefix. `<VERSION>` is the canonical `[workspace.package] version` in the root `Cargo.toml`.

Archive assembly uses `git archive` with a fixed mode mask and `gzip -n`, so repeated assembly at the same commit produces the same `openvmm-<VERSION>-source.tar.gz` bytes. The assembly also generates `SHA256SUMS`.

The archive contains no `.git` directory and does not stamp a second version into the source. Consequently, a binary built from the extracted archive reports the plain workspace version.

## Build requirements

The distribution build requires:

- the Rust toolchain required by the workspace;
- a C compiler and linker;
- glibc development headers;
- Linux UAPI headers;
- OpenSSL development headers;
- `pkg-config`;
- a Protocol Buffers compiler providing `protoc`.

Do not use `cargo xflowey restore-packages` when building a distribution package. That command restores prebuilt native dependencies intended for repository development.

## Build configuration

Build the host `x86_64-unknown-linux-gnu` target dynamically linked against the distribution's glibc and OpenSSL:

```bash
export PROTOC="$(command -v protoc)"
export OPENSSL_NO_VENDOR=1
cargo build --release --locked -p openvmm \
    --target x86_64-unknown-linux-gnu
```

OpenVMM CI assembles the source archive, extracts it outside the repository checkout, and runs this command with distribution-provided native dependencies. This gate prevents repository-only `.packages/` dependencies from becoming accidental packaging requirements.

## Offline builds

The source archive contains project source, not a vendored Cargo dependency tree. A distribution that requires an offline build should vendor dependencies separately and cover that vendor archive with its own integrity metadata.

Create the vendor tree:

```bash
cargo vendor vendor/ > vendor-config.toml
```

Append the generated source replacement configuration to `.cargo/config.toml`, then build offline:

```bash
cargo build --release --locked --offline -p openvmm \
    --target x86_64-unknown-linux-gnu
```

`cargo vendor` operates on the workspace, so the vendor tree includes dependencies not compiled by the OpenVMM Linux binary.

## Package identity

The OpenVMM binary reports the upstream product version committed in the source tree. Record a distribution-specific package revision in the distribution package metadata rather than replacing the binary version.

## Runtime dependencies

Confirm the exact dependencies for the packaged executable with `readelf` or the distribution's automatic dependency generator. The expected shared libraries include glibc, OpenSSL (`libssl` and `libcrypto`), and `libgcc_s`. SQLite is compiled into the binary and does not add a shared runtime dependency.
