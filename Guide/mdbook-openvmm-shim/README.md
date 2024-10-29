# mdbook-openvmm-shim

This is a small standalone Rust binary which implements a mdbook preprocessor
"shim", used to give OpenVMM additional control over how `mdbook` finds and
interacts with external preprocessors.

Notably: it allows us to work around the following outstanding mdbook issue:
Preprocessor cannot add folders/files directly to book directory
[#1222](https://github.com/rust-lang/mdBook/issues/1222). Instead, this shim
will dynamically invoke each preprocessor's `install` subcommand on-demand,
avoiding the need to check-in the somewhat large third-party css/js/html blobs
that these plugins depend on.

In addition, this shim has the benefit of keeping the `mdbook` end-user
experience as vanilla as possible, without requiring a project-specific wrapper
script (e.g: `cargo xtask mdbook`).

## Why is this its own workspace?

Since `book.toml` uses `cargo run` to invoke this script, having this crate live
in the main OpenVMM Rust workspace would result in delays of ~0.5s on each
command invocation (due to `cargo run` needing to scan the whole workspace on
each invocation). Having it in its own workspace results in a `cargo run`
overhead of roughly `0.01` seconds (on average), which is far more tolerable.

In the future, when `cargo-script` finally lands (see
<https://github.com/rust-lang/cargo/issues/12207>), this shim should simply be a
standalone file in the `Guide/` directory.
