This is a Rust based repository. Please follow these guidelines when contributing:

## Code Standards

### Required Before Each Commit
- Run `cargo xtask fmt --fix` before committing any changes to ensure proper code formatting.
- This will ensure all source code and generated pipeline files maintain consistent style and content.
- Cross-compile by targeting `x86_64` and `aarch64` processors, and Windows and Linux environments.

## Key Guidelines
1. Follow Rust best practices and idiomatic patterns.
2. Maintain existing code structure and organization.
3. Write unit tests for new functionality.
4. Document public APIs and complex logic. Suggest changes to the `Guide/` folder when appropriate.

## Domain-specific Guidelines
Both OpenVMM and OpenHCL processes data from untrusted sources. OpenHCL runs in a constrained environment.

When possible:
1. Avoid `unsafe` code.
2. Avoid taking new external dependencies, or those that can significantly increase binary size.

## Testing
The OpenVMM project contains several types of tests.
- **Unit tests** are spread throughout crates, and are marked by a `#[cfg(test)]` code block.
- **VMM tests** are integration tests. These are found in the `vmm_tests` folder, and use the code in `petri` as a framework to help create Hyper-V and OpenVMM based VMs.
- **Fuzz tests** are nondeterminstic, and are used to ensure that the code does not panic across trust boundaries.

Whenever possible, thoroughly test the code with unit tests. Add a test case to the VMM tests if there is an interesting integration point.
