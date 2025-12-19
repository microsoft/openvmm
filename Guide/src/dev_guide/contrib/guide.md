# Updating this Guide

We gladly welcome PRs that improve the quality of the OpenVMM guide!

The OpenVMM Guide is written in Markdown, and rendered to HTML using
[mdbook](https://github.com/rust-lang/mdBook). You can find the source-code of
this Guide in the main OpenVMM GitHub repo, in the
[`Guide/`](https://github.com/microsoft/openvmm/tree/main/Guide) folder.

## Guide conventions

* Line Length: Wrap lines at 80 characters.

## Editing the Guide

### Small Changes

For small changes, you can simply click the "Suggest an Edit" button in the
top-right corner of any page to automatically open up a GitHub Edit page.

### Medium Changes

For medium changes, we suggest cloning the repo locally, and previewing changes
to Markdown in your editor (Visual Studio Code has good support for this).

### Large Changes

For large changes, we suggest cloning the repo locally, and building a fully
rendered copy of the Guide using `mdbook`.

This is very useful when making changes that leverage mdbook preprocessors, such
as using mermaid diagrams, or previewing admonishments.

```admonish info
For example, the `mdbook-admonish` preprocessor is what powers this nice looking
"Info" box!
```

Building the Guide locally is quite straightforward:

1. Install `mdbook` and the additional preprocessors we use locally:

```bash
cargo install mdbook
cargo install mdbook-admonish
cargo install mdbook-mermaid
```

2. Navigate into the `Guide/` directory, and run `mdbook`:

```bash
cd Guide/
# must be run inside the `Guide/` directory!
mdbook serve
```

3. Navigate to the localhost URL in your web browser (typically
`http://127.0.0.1:3000/`)

### Troubleshooting

#### Running `mdbook serve` outside the `Guide/` directory

**Error:**

```text
2024-10-29 16:26:22 [INFO] (mdbook::book): Book building has started
error: manifest path `./mdbook-openvmm-shim/Cargo.toml` does not exist
```

**Solution:**

Ensure you have changed your working-directory to the `Guide/` folder (e.g: via
`cd Guide/`), and then run `mdbook serve`.

#### Rust is not installed

**Error:**

```text
2024-10-29 16:35:49 [INFO] (mdbook::book): Book building has started
2024-10-29 16:35:49 [WARN] (mdbook::preprocess::cmd): The command wasn't found, is the "admonish" preprocessor installed?
2024-10-29 16:35:49 [WARN] (mdbook::preprocess::cmd):   Command: cargo run --quiet --manifest-path ./mdbook-openvmm-shim/Cargo.toml mdbook-admonish
```

**Solution:**

The OpenVMM Guide hooks into a custom Rust utility called `mdbook-openvmm-shim`,
which must be compiled in order for `mdbook` to successfully build the OpenVMM
guide.

Please ensure you have [installed Rust](../getting_started/linux.md#installing-rust).
