# Flowey

Flowey is an in-house, Rust library for writing maintainable, cross-platform automation. It enables developers to define CI/CD pipelines and local workflows as type-safe Rust code that can generate backend-specific YAML (Azure DevOps, GitHub Actions) or execute directly on a local machine. Rather than writing automation logic in YAML with implicit dependencies, flowey treats automation as first-class Rust code with explicit, typed dependencies tracked through a directed acyclic graph (DAG).

## Why Flowey?

Traditional CI/CD pipelines using YAML-based configuration (e.g., Azure DevOps Pipelines, GitHub Actions workflows) have several fundamental limitations that become increasingly problematic as projects grow in complexity:

### The Problems with Traditional YAML Pipelines

#### Non-Local Reasoning and Global State

- YAML pipelines heavily rely on global state and implicit dependencies (environment variables, file system state, installed tools)
- Understanding what a step does often requires mentally tracking state mutations across the entire pipeline
- Debugging requires reasoning about the entire pipeline context rather than isolated units of work
- Changes in one part of the pipeline can have unexpected effects in distant, seemingly unrelated parts

#### Maintainability Challenges

- YAML lacks type safety, making it easy to introduce subtle bugs (typos in variable names, incorrect data types, etc.)
- No compile-time validation means errors only surface at runtime
- Refactoring is risky and error-prone without automated tools to catch breaking changes
- Code duplication is common because YAML lacks good abstraction mechanisms
- Testing pipeline logic requires actually running the pipeline, making iteration slow and expensive

#### Platform Lock-In

- Pipelines are tightly coupled to their specific CI backend (ADO, GitHub Actions, etc.)
- Multi-platform support means maintaining multiple, divergent YAML files

#### Local Development Gaps

- Developers can't easily test pipeline changes before pushing to CI
- Reproducing CI failures locally is difficult or impossible
- The feedback loop is slow: push → wait for CI → debug → repeat

### Flowey's Solution

Flowey addresses these issues by treating automation as **first-class Rust code**:

- **Type Safety**: Rust's type system catches errors at compile-time rather than runtime
- **Local Reasoning**: Dependencies are explicit through typed variables, not implicit through global state
- **Portability**: Write once, generate YAML for any backend (ADO, GitHub Actions, or run locally)
- **Reusability**: Nodes are composable building blocks that can be shared across pipelines

## Flowey's Directory Structure

Flowey is architected as a standalone tool with a layered crate structure that separates project-agnostic core functionality from project-specific implementations:

- **`flowey_core`**: Provides the core types and traits shared between user-facing and internal Flowey code, such as the essential abstractions for nodes and pipelines.
- **`flowey`**: Thin wrapper around `flowey_core` that exposes the public API for defining nodes and pipelines.
- **`flowey_cli`**: Command-line interface for running flowey - handles YAML generation, local execution, and pipeline orchestration.
- **`schema_ado_yaml`**: Rust types for Azure DevOps YAML schemas used during pipeline generation.
- **`flowey_lib_common`**: Ecosystem-wide reusable nodes (installing Rust, running Cargo, downloading tools, etc.) that could be useful across projects outside of OpenVMM.
- **`flowey_lib_hvlite`**: OpenVMM-specific nodes and workflows that build on the common library primitives.
- **`flowey_hvlite`**: The OpenVMM pipeline definitions that compose nodes from the libraries above into complete CI/CD workflows.
