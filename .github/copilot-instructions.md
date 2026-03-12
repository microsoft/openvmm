# OpenVMM Repository

## Project Overview
OpenVMM is a modular, cross-platform Virtual Machine Monitor (VMM) written in Rust. This repository is home to both OpenVMM and OpenHCL (a paravisor). The project focuses on creating secure, high-performance virtualization infrastructure.

## Technology Stack
- **Language**: Rust (using Cargo build system)
- **Build Tool**: Cargo with custom xtask automation
- **Package Management**: Cargo + custom flowey pipeline tools
- **Testing Framework**: Rust unit tests + cargo-nextest (recommended)
- **Documentation**: mdBook (in `Guide/` folder, published at https://openvmm.dev)

## Project Structure
- `openvmm/` - Core OpenVMM VMM implementation
- `openhcl/` - OpenHCL paravisor implementation
- `vmm_tests/` - Integration tests using the petri framework
- `support/` - Shared support libraries and utilities
- `vm/` - VM components (devices, chipset, etc.)
- `Guide/` - Documentation source (published at https://openvmm.dev)
- `xtask/` - Custom build and automation tasks
- `flowey/` - Pipeline and build automation framework

## Build and Test

```bash
cargo xflowey restore-packages   # First-time dependency setup
cargo build                       # Debug build
cargo build --release             # Release build
cargo nextest run -p <package>    # Test a specific crate (recommended; do NOT use `cargo test` directly)
cargo xtask fmt --fix             # Format and apply house rules
cargo doc                         # Check rustdoc
```

- Cross-compilation for `x86_64` and `aarch64` is supported
- OpenHCL can only be built from Linux (WSL2 counts)
- See `Guide/src/dev_guide/getting_started/cross_compile.md` for cross-compilation setup
- Test config: `.config/nextest.toml`

### Test Types
- **Unit tests**: `#[cfg(test)]` blocks throughout crates
- **VMM tests**: Integration tests in `vmm_tests/` using the petri framework (requires setup)
- **Fuzzers**: Help identify edge cases and security issues, not tests per se

## Line Endings

LF only on all platforms (see `.gitattributes`). Never use CRLF.

## Code Standards

1. Follow Rust best practices and idiomatic patterns
2. Write unit tests for new functionality
3. Document public APIs and complex logic
4. Update `Guide/` docs when adding features or changing behavior
   (see `.github/instructions/doc-code-sync.instructions.md` for the mapping)

### Trust Boundaries (critical for security)
- **OpenVMM does not trust the VTL0 guest** — code must not panic on any guest input
- **OpenHCL does not trust the root** — code must not panic on any root input
- **OpenHCL does not trust the VTL0 guest** — attack surface is subtle, needs human review

### General Rules
- Avoid `unsafe` code when possible
- Avoid new external dependencies, especially those increasing binary size
- Never panic across trust boundaries
- Unit tests: fast, isolated, no root/admin access required
- Mark tests needing special setup with `#[ignore]`
