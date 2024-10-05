# Scripts

Whereas other projects might reach for bash or python to implement various bits
of repo automation / pipelines / tooling - OpenVMM uses Rust!

- `cargo xtask`: one-off _tools/scripts_
- `cargo xflowey`: project-critical dev-loop and CI-facing _build automation_

Both `cargo xtask` and `cargo xflowey` are _project-specific binaries_, unique
to the OpenVMM project. They are _not_ built into Cargo and/or standardized
across the Rust ecosystem.

If you open `.cargo/config.toml`, you'll find that both `cargo xtask` and `cargo
xflowey` are just convenient `[alias]`es for project-local `cargo run`
invocations.

## `cargo xtask`

`cargo xtask` is a OpenVMM's "swiss army knife" Rust binary that houses various
bits of project specific tooling.

Some examples of tools that you can find under `xtask`:

- `cargo xtask fmt` implements various OpenVMM-specific style / linting rules
- `cargo xtask fuzz` implements various OpenVMM-specific `cargo fuzz` extensions
- `cargo xtask install-git-hooks` sets up git hooks for developers

This list is not exhaustive. Running `cargo xtask` will list what tools are
available, along with brief descriptions of what they do / how to use them.

For more information of the `xtask` pattern, see <https://github.com/matklad/cargo-xtask>

## `cargo xflowey`

To implement various developer workflows (both locally, as well as in CI), the
OpenVMM project relies on `flowey` - a custom, in-house Rust library/framework
for writing maintainable, cross-platform automation.

`cargo xflowey` is a cargo alias that makes it easy for developers to run
`flowey`-based pipelines locally.

Some particularly notable pipelines:

- `cargo xflowey build-igvm` - primarily dev-tool used to build OpenHCL IGVM files locally
- `cargo xflowey ci checkin-gates` - runs the entire PR checkin suite locally
- `cargo xflowey restore-packages` - restores external packages needed to compile and run OpenVMM / OpenHCL
