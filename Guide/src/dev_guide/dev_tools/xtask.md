# cargo xtask

`cargo xtask` is OpenVMM's "swiss army knife" Rust binary that houses various
bits of project specific tooling.

For more info on how `xtask` is different from `xflowey`, see [`xflowey` vs
`xtask`](./xflowey.md#xflowey-vs-xtask).

Some examples of tools that you can find under `xtask`:

- `cargo xtask clean` cleans build system artifacts (flowey, packages, test results)
- `cargo xtask fmt` implements various OpenVMM-specific style / linting rules
- `cargo xtask fuzz` implements various OpenVMM-specific `cargo fuzz` extensions
- `cargo xtask install-git-hooks` sets up git hooks for developers

This list is not exhaustive. Running `cargo xtask` will list what tools are
available, along with brief descriptions of what they do / how to use them.

For more information of the `xtask` pattern, see <https://github.com/matklad/cargo-xtask>
