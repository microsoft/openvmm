# OpenVMM Repository

## Project Overview
OpenVMM is a modular, cross-platform Virtual Machine Monitor (VMM) written in Rust. This repository is home to both OpenVMM and OpenHCL (a paravisor). The project focuses on creating secure, high-performance virtualization infrastructure.

## Technology Stack
- **Language**: Rust (using Cargo build system)
- **Build Tool**: Cargo with custom xtask automation
- **Package Management**: Cargo + custom flowey pipeline tools
- **Testing Framework**: Rust unit tests + cargo-nextest (recommended)
- **Documentation**: mdBook (in `Guide/` folder)

## Project Structure
- `openvmm/` - Core OpenVMM VMM implementation
- `openhcl/` - OpenHCL paravisor implementation
- `vmm_tests/` - Integration tests using the petri framework
- `support/` - Shared support libraries and utilities
- `vm/` - VM components (devices, chipset, etc.)
- `Guide/` - Documentation source (published at https://openvmm.dev)
- `xtask/` - Custom build and automation tasks
- `flowey/` - Pipeline and build automation framework

## Build Commands

### Initial Setup
Before building for the first time, restore required dependencies:
```bash
cargo xflowey restore-packages
```

### Building
Build the project using standard Cargo:
```bash
cargo build
```

For release builds:
```bash
cargo build --release
```

### Cross-compilation
The project supports cross-compilation for `x86_64` and `aarch64` architectures. Note:
- Some components (like OpenHCL) can only be built from Linux (WSL2 counts as Linux)
- For cross-compilation from WSL2 to Windows, see `Guide/src/dev_guide/getting_started/cross_compile.md` and source `. ./build_support/setup_windows_cross.sh`

## Testing

### Unit Tests
Use cargo-nextest (recommended) or cargo test:
```bash
# Recommended - install with: cargo install cargo-nextest --locked
# Run tests in specific packages you are modifying (default won't run anything)
cargo nextest run -p <package-name>

# Or use standard cargo test
cargo test -p <package-name>
```

Configure test runs using `.config/nextest.toml` for resource management and timeouts.

### Test Types
- **Unit tests**: Spread throughout crates, marked by `#[cfg(test)]` blocks
- **VMM tests**: Integration tests in `vmm_tests/` using the petri framework for Hyper-V and OpenVMM VMs (requires additional setup)
- **Fuzz tests**: Nondeterministic tests ensuring no panics across trust boundaries

## Linting and Formatting

### Required Before Each Commit
Always run formatting and documentation checks before committing:
```bash
cargo xtask fmt --fix
cargo doc
```

This ensures:
- All source code follows rustfmt standards
- Generated pipeline files maintain consistent style
- Code follows project-specific "house rules" (copyright headers, naming conventions, etc.)
- No errors in rustdoc comments

### Available Checks
Run specific formatting passes:
```bash
cargo xtask fmt --help  # See all available passes
cargo xtask fmt --pass rustfmt
cargo xtask fmt --pass house-rules
```

## Code Standards

### Key Guidelines
1. Follow Rust best practices and idiomatic patterns
2. Maintain existing code structure and organization
3. Write unit tests for new functionality
4. Document public APIs and complex logic
5. Update documentation in `Guide/` folder when adding features or changing behavior

### Domain-specific Guidelines
Both OpenVMM and OpenHCL process data from untrusted sources. OpenHCL runs in a constrained environment.

**Trust Boundaries** (critical for security):
- **OpenVMM does not trust the VTL0 guest** - code must not panic on any guest input
- **OpenHCL does not trust the root** - code must not panic on any root input
- **OpenHCL does not trust the VTL0 guest** - less critical than OpenVMM, but the attack surface is subtle and needs human review

When possible:
1. Avoid `unsafe` code
2. Avoid taking new external dependencies, especially those that significantly increase binary size
3. Ensure code doesn't panic across trust boundaries

## Testing Best Practices
- Thoroughly test code with unit tests whenever possible
- Add VMM test cases for interesting integration points
- Unit tests should be fast, isolated, and not require root/administrator access
- Mark tests requiring special setup with `#[ignore]` for manual testing
