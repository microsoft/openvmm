# Dev Tools / Utilities

While most tasks in the OpenVMM repo can be accomplished directly via standard
Rust tooling (i.e: `cargo run`, `cargo build`), there are many dev tasks that
don't neatly fall under the `cargo` umbrella. e.g:

- running code formatters / linters
- orchestrating multi-stage, multi-component OpenHCL builds
- running different kinds of test suites
- building/downloading test images for VMM testing
- setting up git hooks
- etc...

The following chapter discusses some of the various dev-facing tools / utilities
you may encounter and/or find useful when working on OpenVMM.

## Rust-based Tooling

As with many projects, OpenVMM initially took the simple approach of spinning up
ad-hoc Bash/Python scripts, and hand-written YAML workflow automation.

This worked for a while... but as the project continued to grow, our once small
and focused set of scripts evolved into a mass of interconnected dependencies,
magic strings, and global variables!

To pay down mounting tech debt, and to foster a culture where all devs are
empowered to contribute and maintain OpenVMM's project tooling, we have adopted
a policy of migrating as much core tooling away from loosely-typed languages
(like Bash, Python, and hand-written Workflow YAML), and towards new
strongly-typed Rust-based tooling.
