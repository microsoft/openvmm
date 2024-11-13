# Suggested Dev Environment

**Prerequisites:**

- One of:
  - [Getting started on Windows](./windows.md)
  - [Getting started on Linux / WSL2](./linux.md).
- One of:
  - [Building OpenVMM](./build_openvmm.md)
  - [Building OpenHCL](./build_openhcl.md)

* * *

This page is for those interested in actively iterating on OpenVMM or OpenHCL.

## Setting up VSCode

These instructions assume you're using [VSCode](https://code.visualstudio.com/).

If you're using a different development environment, we nonetheless suggest
reading through this section, so you can enable similar settings in whatever
editor / IDE you happen to be using.

~~~admonish tip
Just want the recommended editor settings? Put this in `openvmm/.vscode/settings.json`:
```json
{
    "rust-analyzer.linkedProjects": [
        "Cargo.toml",
    ],
    "rust-analyzer.cargo.targetDir": true,
    "rust-analyzer.imports.granularity.group": "item",
    "rust-analyzer.imports.group.enable": false,
    "[rust]": {
        "editor.formatOnSave": true
    },
}
```
~~~

### \[WSL2] Connecting to WSL using VSCode

When using Visual Studio Code with WSL, be sure to use the
[WSL extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-wsl)
instead of accessing your files using the `\\wsl.localhost` share (the repo
should be cloned in the WSL filesystem, as mentioned in the WSL getting started
guide). This will ensure that all VSCode extensions and features to work properly.

Once the
extension is installed, click the blue arrows in the bottom left corner and
select "Connect to WSL". Then open the folder you cloned the repository into.
More information is available
[here](https://learn.microsoft.com/en-us/windows/wsl/tutorials/wsl-vscode).

### Configuring `rust-analyzer`

[rust-analyzer](https://marketplace.visualstudio.com/items?itemName=matklad.rust-analyzer)
provides IDE-like functionality when writing Rust code (e.g: autocomplete, jump
to definition, refactoring, etc...). It is a massive productivity multiplier
when working with Rust code, and it would be a _very bad idea_ to work in the
OpenVMM repo without having it set up correctly.

Check out the [rust-analyzer manual](https://rust-analyzer.github.io/manual.html)
for a comprehensive overview of rust-analyzer's features.

Once installed, we suggest you specify the following additional configuration
options in the OpenVMM workspace's `.vscode/settings.json` file:

```json
{
    "rust-analyzer.linkedProjects": [
        "Cargo.toml",
    ]
}
```

#### (Strongly Suggested) Avoiding cache invalidation

To avoid unnecessary re-builds or lock-contention in the build directory between
rust-analyzer and manual builds, set the following configuration option to give
rust-analyzer a separate target directory:

```json
{
    "rust-analyzer.cargo.targetDir": true,
}
```

#### (Strongly Suggested) Disable nested imports

When auto-importing deps, rust-analyzer defaults to nesting imports, which isn't
the OpenVMM convention.

This can be changed to one-dep-per-line by specifying the following settings:

```json
{
    "rust-analyzer.imports.granularity.group": "item",
    "rust-analyzer.imports.group.enable": false,
}
```

#### Enabling `clippy`

CI will fail if the code is not clippy-clean.
[Clippy](https://doc.rust-lang.org/stable/clippy/) is a linter that helps catch
common mistakes and improves the quality of our Rust code.

By default, rust-analyzer will use `cargo check` to lint code, but it can be
configured to use `cargo clippy` instead:

```json
{
  "rust-analyzer.check.command": "clippy",
}
```

#### Enabling Format on Save

CI will fail if code is not formatted with `rustfmt`.

You can enable the "format on save" option in VSCode to automatically run
`rustfmt` whenever you save a file:

```json
{
    "[rust]": {
        "editor.formatOnSave": true
    },
}
```

#### Enhanced "Enter"

`rust-analyzer` can override the "Enter" key to make it smarter:

- "Enter" inside triple-slash comments automatically inserts `///`
- "Enter" in the middle or after a trailing space in `//` inserts `//`
- "Enter" inside `//!` doc comments automatically inserts `//!`
- "Enter" after `{` indents contents and closing `}` of single-line block

This action needs to be assigned to shortcut explicitly, which can be done by
adding the following line to `keybindings.json`:

```json
// must be put into keybindings.json, NOT .vscode/settings.json!
{
    "key": "Enter",
    "command": "rust-analyzer.onEnter",
    "when": "editorTextFocus && !suggestWidgetVisible && editorLangId == rust"
}
```

### Running `cargo xtask fmt house-rules` on-save

The OpenVMM project includes a handful of custom "house rule" lints that are
external to `rustfmt`. These are things like checking for the presence of
copyright headers, enforcing single-trailing newlines, etc...

These lints are enfoced using `cargo xtask fmt house-rules`, and can be
automatically fixed by passing the `--fix` flag.

We recommend installing the
[RunOnSave](https://marketplace.visualstudio.com/items?itemName=emeraldwalk.RunOnSave)
extension, and configuring it to run these lints as part of your regular
development flow.

Set the following configuration in your `.vscode/settings.json`

```json
{
    "emeraldwalk.runonsave": {
        "commands": [
            {
                "match": ".*",
                "cmd": "cd ${workspaceFolder}"
            },
            {
                "match": ".*",
                "isAsync": true,
                "cmd": "$(cat ./target/xtask-path) fmt house-rules --fix ${file}"
            }
        ]
    },
}
```

### GitHub Pull Request Integration

As the repo is hosted on GitHub, you might find convenient to use the
[GitHub Pull Request](https://marketplace.visualstudio.com/items?itemName=GitHub.vscode-pull-request-github)
VSCode extension. That allows working through the PR feedback and
issues without leaving the comfort of VSCode.

## Setting up pre-commit and pre-push hooks

It's never fun having CI reject your changes due to some minor formatting issue,
especially when it's super quick to run those formatting checks locally. Running
`cargo xtask fmt` before pushing up your code is quick and easy, and will save
you the annoyance of wrestling with formatting check-in gates!

Of course, it's very easy to forget to run `cargo xtask fmt` after making code
changes, but thankfully, you can set up some [git hooks](https://git-scm.com/book/en/v2/Customizing-Git-Git-Hooks)
that will do this for you automatically!

You can run `cargo xtask install-git-hooks --help` for more details on what
hooks are available and their various configuration options, but for most users,
we suggest the following config:

```bash
cargo xtask install-git-hooks --pre-push --with-fmt=yes
```

And you'll be all set!

```admonish success
If you're worried about time, the `pre-push` hook should only take ~5
seconds to run locally. That's far better than waiting ~20+ minutes only
for CI to fail on your pull request.
```

# \[WSL2] Cross Compiling from WSL2 to Windows

Setting up cross compilation is very useful, as it allows using the same repo
cloned in WSL2 to both develop OpenHCL, as well as launch it via OpenVMM via the
WHP backend.

## Required Dependencies

Note that this requires some additional dependencies, described below.

### Windows deps

Visual Studio build tools must be installed, along with the Windows SDK. This is
the same as what's required to build OpenVMM on windows.

### WSL deps

The msvc target `x86_64-pc-windows-msvc` must be installed for the toolchain
being used in WSL. This can be added by doing the following:

```bash
rustup target add x86_64-pc-windows-msvc
```

Note that today this is only supported with the external, public toolchain, not
msrustup.

Additional build tools must be installed as well. If your distro has LLVM 14
available (Ubuntu 22.04 or newer):
```bash
sudo apt install clang-tools-14 lld-14
```

Otherwise, follow the steps at https://apt.llvm.org/ to install a specific
version, by adding the correct apt repos. Note that you must install
`clang-tools-14` as default `clang-14` uses gcc style arguments, where
`clang-cl-14` uses msvc style arguments. You can use their helper script as
well:
```bash
wget https://apt.llvm.org/llvm.sh
chmod +x llvm.sh
sudo ./llvm.sh 14
sudo apt install clang-tools-14
```

## Setting up the terminal environment

Source the `build_support/setup_windows_cross.sh` script from your terminal
instance. For example, the following script will do this along with setting a
default cargo build target:

```bash
#!/bin/bash

# Setup environment and windows cross tooling.

export CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu
cd path/to/openvmm || exit
. build_support/setup_windows_cross.sh
exec "$SHELL"
```

For developers using shells other than bash, you may need to run the
`setup_windows_cross.sh` script in bash then launch your shell in order to get
the correct environment variables.

## Editing with vscode

You can have rust-analyzer target Windows, which will allow you to use the same
repo for OpenHCL, Linux, and Windows changes, but the vscode remote server
must be launched from the terminal window that sourced the setup script. You can
do this by closing all vscode windows then opening your workspace with
`code <path to workspace>` in your terminal.

Add the following to your workspace settings for a vscode workspace
dedicated to Windows:

```json
"settings": {
    "rust-analyzer.cargo.target": "x86_64-pc-windows-msvc"
}
```

## Running Windows OpenVMM from within WSL

You can build and run the windows version of OpenVMM by overriding the target
field of cargo commands, via `--target x86_64-pc-windows-msvc`. For example, the
following command will run OpenVMM with WHP:

```bash
cargo run --target x86_64-pc-windows-msvc
```

You can optionally set cargo aliases for this so that you don't have to type out
the full target every time. Add the following to your `~/.cargo/config.toml`:

```toml
[alias]
winbuild = "build --target x86_64-pc-windows-msvc"
wincheck = "check --target x86_64-pc-windows-msvc"
winclippy = "clippy --target x86_64-pc-windows-msvc"
windoc = "doc --target x86_64-pc-windows-msvc"
winrun = "run --target x86_64-pc-windows-msvc"
wintest = "test --target x86_64-pc-windows-msvc"
```

You can then run the windows version of OpenVMM by running:

```bash
cargo winrun
```

### Speeding up Windows OpenVMM launch

Due to filesystem limitations on WSL, launching OpenVMM directly will be somewhat
slow. Instead, you can copy the built binaries to a location on in the Windows
filesystem and then launch them via WSL.

Quite a few folks working on the OpenVMM project have hacked together personal
helper scripts to automate this process.

TODO: include a sample of such a script here
