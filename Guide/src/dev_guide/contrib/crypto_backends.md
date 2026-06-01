# Crypto Backends

The `crypto` crate (`support/crypto`) abstracts over several backend
implementations of the cryptographic primitives OpenVMM and OpenHCL need.
Exactly one backend must be selected per binary, chosen via Cargo features:

| Feature    | Backend                                                                          |
| ---------- | -------------------------------------------------------------------------------- |
| `openssl`  | OpenSSL (typical for Linux / OpenHCL).                                           |
| `symcrypt` | [SymCrypt](https://github.com/microsoft/SymCrypt).                               |
| `rust`     | Pure-Rust implementations from [RustCrypto](https://github.com/RustCrypto).      |
| `native`   | Platform default — OpenSSL on Linux, BCrypt/CNG on Windows, Security.framework on macOS. |

Selection happens in [`support/crypto/build.rs`](https://github.com/microsoft/openvmm/blob/main/support/crypto/build.rs),
which emits a `cfg` (`openssl`, `symcrypt`, `rust`, or `native`) based on
the enabled features.

## The "multiple backends enabled" error

Because Cargo unifies features across a workspace, building two binaries
in the same `cargo` invocation that ask for *different* `crypto` backends
will result in the `crypto` crate being compiled with *all* of those
features enabled at once. There is no sensible single backend to pick in
that case.

To keep workspace-wide `cargo check` usable, the build script does not
panic in this situation. Instead it emits a `multi_backend` cfg, and
`crypto`'s `lib.rs` references an undefined extern symbol under that cfg.
The result:

- `cargo check --workspace` — succeeds. No linking happens.
- `cargo build --workspace` (or any actual link step) — fails with:

  ```text
  rust-lld: error: undefined symbol:
    __openvmm_crypto_multiple_backends_enabled__enable_exactly_one__see_support_crypto
  ```

The symbol name *is* the diagnostic: the offending binary has multiple
`crypto` backend features enabled simultaneously and needs to pick one.

### Fixing it

1. Identify which binary you are building and which `crypto` features it
   (transitively) enables. `cargo tree -e features -p <binary> -i crypto`
   is usually the fastest way.
2. Either narrow your `cargo build` invocation (e.g. `-p <binary>`
   instead of `--workspace`), or adjust the offending crate's
   `Cargo.toml` so it stops enabling a backend it shouldn't.
3. Remember that adding `features = ["native"]` (or any other backend)
   to a *library* crate's dependency on `crypto` will force that
   backend on every binary that links the library. Backends should
   normally be selected by binary crates only.
