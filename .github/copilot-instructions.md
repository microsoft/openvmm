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
The project supports cross-compilation for `x86_64` and `aarch64` architectures on both Windows and Linux. See `cargo xflowey restore-packages --help` for cross-compilation package options.

## Testing

### Unit Tests
Use cargo-nextest (recommended) or cargo test:
```bash
# Recommended - install with: cargo install cargo-nextest --locked
cargo nextest run

# Or use standard cargo test
cargo test
```

### Test Types
- **Unit tests**: Spread throughout crates, marked by `#[cfg(test)]` blocks
- **VMM tests**: Integration tests in `vmm_tests/` using the petri framework for Hyper-V and OpenVMM VMs
- **Fuzz tests**: Nondeterministic tests ensuring no panics across trust boundaries

## Linting and Formatting

### Required Before Each Commit
Always run formatting before committing:
```bash
cargo xtask fmt --fix
```

This ensures:
- All source code follows rustfmt standards
- Generated pipeline files maintain consistent style
- Code follows project-specific "house rules" (copyright headers, naming conventions, etc.)

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

When possible:
1. Avoid `unsafe` code
2. Avoid taking new external dependencies, especially those that significantly increase binary size
3. Ensure code doesn't panic across trust boundaries (critical for security)

## Testing Best Practices
- Thoroughly test code with unit tests whenever possible
- Add VMM test cases for interesting integration points
- Unit tests should be fast, isolated, and not require root/administrator access
- Mark tests requiring special setup with `#[ignore]` for manual testing
