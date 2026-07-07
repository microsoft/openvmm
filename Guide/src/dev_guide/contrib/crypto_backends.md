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

## The "wrong number of backends" error

Because Cargo unifies features across a workspace, building two binaries
in the same `cargo` invocation that ask for *different* `crypto` backends
will result in the `crypto` crate being compiled with *all* of those
features enabled at once. Conversely, building a library crate that
transitively depends on `crypto` without itself selecting a backend
(e.g. running its unit tests) results in `crypto` being compiled with
*zero* backends enabled. In either case there is no sensible single
backend to pick.

To keep workspace-wide `cargo check` and `cargo test` usable, the build
script does not panic in this situation — it picks a placeholder backend
so the crate still compiles. To still guarantee that a *shipping binary*
has linked exactly one backend, each binary opts into a link-time check
by invoking the `crypto::ensure_single_backend!()` macro (typically gated
on `#[cfg(not(test))]`). The macro emits a `#[used]` reference to a symbol
that is only defined by `crypto` when exactly one backend is selected.
The result:

- `cargo check --workspace` — succeeds. No linking happens.
- `cargo test -p <unrelated_crate>` — succeeds, even when that crate
  transitively depends on `crypto` without selecting a backend.
- `cargo build -p <binary>` for a binary that invokes the macro — fails
  at link time if zero or multiple backends end up enabled, with:

  ```text
  rust-lld: error: undefined symbol:
    __openvmm_crypto_ensure_single_backend__enable_exactly_one__see_support_crypto
  ```

The symbol name *is* the diagnostic: the offending binary has the wrong
number of `crypto` backend features enabled and needs to pick exactly one.

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
