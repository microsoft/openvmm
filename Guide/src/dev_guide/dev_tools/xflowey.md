# cargo xflowey

To implement various developer workflows (both locally, as well as in CI), the
OpenVMM project relies on `flowey`: a custom, in-house Rust library/framework
for writing maintainable, cross-platform automation.

`cargo xflowey` is a cargo alias that makes it easy for developers to run
`flowey`-based pipelines locally.

Some particularly notable pipelines:

- `cargo xflowey build-igvm` - primarily dev-tool used to build OpenHCL IGVM files locally
- `cargo xflowey restore-packages` - restores external packages needed to compile and run OpenVMM / OpenHCL

```admonish warning
While `cargo xflowey` technically has the ability to run CI pipelines locally (e.g., `cargo xflowey ci checkin-gates`), this functionality is currently broken and should not be relied upon. Use CI pipelines in their intended environments (Azure DevOps or GitHub Actions).
```

## `xflowey` vs `xtask`

In a nutshell:

- `cargo xtask`: implements novel, standalone tools/utilities
- `cargo xflowey`: orchestrates invoking a sequence of tools/utilities, without
  doing any non-trivial data processing itself

---

# Flowey Developer Guide

This guide explains the core concepts and architecture of flowey for developers
working on OpenVMM automation.

## Table of Contents

1. [Core Concepts](#core-concepts)
2. [Emitting Steps](#emitting-steps)
3. [Runtime Services](#runtime-services)
4. [Flowey Nodes](#flowey-nodes)
5. [Variables: ReadVar and WriteVar](#variables-readvar-and-writevar)
6. [Node Design Philosophy](#node-design-philosophy)
7. [Common Patterns](#common-patterns)
8. [Artifacts](#artifacts)
9. [Pipelines](#pipelines)
10. [Additional Resources](#additional-resources)

---

## Core Concepts

### Two-Phase Execution Model

Flowey operates in two distinct phases:

1. **Build-Time (Resolution Phase)**: When you run `cargo xflowey regen`, flowey:
   - Reads `.flowey.toml` to determine which pipelines to regenerate
   - Builds the flowey binary (e.g., `flowey-hvlite`) via `cargo build`
   - Runs the flowey binary with `pipeline <backend> --out <file> <cmd>` for each pipeline definition
   - During this invocation, flowey constructs a directed acyclic graph (DAG) by:
     - Instantiating all nodes defined in the pipeline
     - Processing their requests
     - Resolving dependencies between nodes via variables and artifacts
     - Determining the execution order
     - Performing flowey-specific validations (dependency resolution, type checking, etc.)
   - Generates YAML files for CI systems (ADO, GitHub Actions) at the paths specified in `.flowey.toml`

2. **Runtime (Execution Phase)**: The generated YAML is executed by the CI system (or locally via `cargo xflowey <pipeline>`). Steps run in the order determined at build-time:
   - Variables are read and written with actual values
   - Commands are executed
   - Artifacts are published/consumed
   - Side effects occur

```admonish note
**Understanding the Workflow:**

The `.flowey.toml` file at the repo root defines which pipelines to generate and where. For example:
```toml
[[pipeline.flowey_hvlite.github]]
file = ".github/workflows/openvmm-pr.yaml"
cmd = ["ci", "checkin-gates", "--config=pr"]
```

When you run `cargo xflowey regen`:
1. It reads `.flowey.toml` 
2. Builds the `flowey-hvlite` binary
3. Runs `flowey-hvlite pipeline github --out .github/workflows/openvmm-pr.yaml ci checkin-gates --config=pr`
4. This generates/updates the YAML file with the resolved pipeline

**Key Distinction:**
- `cargo build -p flowey-hvlite` - Only compiles the flowey code to verify it builds successfully. **Does not** construct the DAG or generate YAML files.
- `cargo xflowey regen` - Compiles the code **and** runs the full build-time resolution to construct the DAG, validate the pipeline, and regenerate all YAML files defined in `.flowey.toml`.

Always run `cargo xflowey regen` after modifying pipeline definitions to ensure the generated YAML files reflect your changes.
```

This separation allows flowey to:
- Validate the entire workflow before execution
- Generate static YAML for CI systems (ADO, GitHub Actions)
- Catch dependency errors at build-time rather than runtime

### Backend Abstraction

Flowey supports multiple execution backends:

- **Local**: Runs directly on your development machine 
- **ADO (Azure DevOps)**: Generates ADO Pipeline YAML
- **GitHub Actions**: Generates GitHub Actions workflow YAML

```admonish warning: 
Nodes should be written to work across ALL backends whenever possible. Relying on `ctx.backend()` to query the backend or manually emitting 
backend-specific steps (via `emit_ado_step` or `emit_gh_step`) should be 
avoided unless absolutely necessary. Most automation logic should be 
backend-agnostic, using `emit_rust_step` for cross-platform Rust code that 
works everywhere. 
```
---

## Emitting Steps

Nodes emit **steps** - units of work that will be executed at runtime. Different
step types exist for different purposes.

### Rust Steps

Rust steps execute Rust code at runtime and are the most common step type in flowey.

**`emit_rust_step`**: The primary method for emitting steps that run Rust code. Steps can claim variables, read inputs, perform work, and write outputs. Returns an optional `ReadVar<SideEffect>` that other steps can use as a dependency.

**`emit_minor_rust_step`**: Similar to `emit_rust_step` but for steps that cannot fail (no `Result` return) and don't need visibility in CI logs. Used for simple transformations and glue logic. Using minor steps also improve performance, since there is a slight cost to starting and ending a 'step' in GitHub and ADO. During the build stage, minor steps that are adjacent to each other will get merged into one giant CI step.

**`emit_rust_stepv`**: Convenience method that combines creating a new variable and emitting a step in one call. The step's return value is automatically written to the new variable.

For detailed examples of Rust steps, see the [`NodeCtx` emit methods documentation](https://docs.rs/flowey_core/latest/flowey_core/node/struct.NodeCtx.html).

### ADO Steps

**`emit_ado_step`**: Emits a step that generates Azure DevOps Pipeline YAML. Takes a closure that returns a YAML string snippet which is interpolated into the generated pipeline.

For ADO step examples, see the [`NodeCtx::emit_ado_step` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/struct.NodeCtx.html#method.emit_ado_step).

### GitHub Steps

**`emit_gh_step`**: Creates a GitHub Actions step using the fluent `GhStepBuilder` API. Supports specifying the action, parameters, outputs, dependencies, and permissions. Returns a builder that must be finalized with `.finish(ctx)`.

For GitHub step examples, see the [`GhStepBuilder` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/steps/github/struct.GhStepBuilder.html).

### Side Effect Steps

**`emit_side_effect_step`**: Creates a dependency relationship without executing code. Useful for aggregating multiple side effect dependencies into a single side effect. More efficient than emitting an empty Rust step.

For side effect step examples, see the [`NodeCtx::emit_side_effect_step` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/struct.NodeCtx.html#method.emit_side_effect_step).

### StepCtx vs NodeCtx

- **`NodeCtx`**: Used when emitting steps. Provides `emit_*` methods, `new_var()`,
  `req()`, etc.
  
- **`StepCtx`**: Used inside step closures. Provides access to `claim()` for
  variables, and basic environment info (`backend()`, `platform()`).

---

## Runtime Services

Runtime services provide the API available during step execution (inside the
closures passed to `emit_rust_step`, etc.).

### RustRuntimeServices

`RustRuntimeServices` is the primary runtime service available in Rust steps. It provides:

**Variable Operations:**
- Reading and writing flowey variables
- Secret handling (automatic secret propagation for safety)
- Support for reading values of any type that implements `ReadVarValue`

**Environment Queries:**
- Backend identification (Local, ADO, or GitHub)
- Platform detection (Windows, Linux, macOS)
- Architecture information (x86_64, Aarch64)

#### Secret Variables and CI Backend Integration

Flowey provides built-in support for handling sensitive data like API keys, tokens, and credentials through **secret variables**. Secret variables are treated specially to prevent accidental exposure in logs and CI outputs.

**How Secret Handling Works**

When a variable is marked as secret, flowey ensures:
- The value is not logged or printed in step output
- CI backends (ADO, GitHub Actions) are instructed to mask the value in their logs
- Secret status is automatically propagated to prevent leaks

**Automatic Secret Propagation**

To prevent accidental leaks, flowey uses conservative automatic secret propagation:

```admonish warning 
If a step reads a secret value, **all subsequent writes from that step are automatically marked as secret** by default. This prevents accidentally leaking secrets through derived values.
```

For example:

```rust
ctx.emit_rust_step("process token", |ctx| {
    let secret_token = secret_token.claim(ctx);
    let output_var = output_var.claim(ctx);
    |rt| {
        let token = rt.read(secret_token);  // Reading a secret
        
        // This write is AUTOMATICALLY marked as secret
        // (even though we're just writing "done")
        rt.write(output_var, &"done".to_string());
        
        Ok(())
    }
});
```

If you need to write non-secret data after reading a secret, use `write_not_secret()`:

```rust
rt.write_not_secret(output_var, &"done".to_string());
```

**Best Practices for Secrets**

1. **Never use `ReadVar::from_static()` for secrets** - static values are encoded in plain text in the generated YAML
2. **Always use `write_secret()`** when writing sensitive data like tokens, passwords, or keys
5. **Minimize secret lifetime** - read secrets as late as possible and don't pass them through more variables than necessary

### AdoStepServices

`AdoStepServices` provides integration with Azure DevOps-specific features when emitting ADO YAML steps:

**ADO Variable Bridge:**
- Convert ADO runtime variables (like `BUILD.SOURCEBRANCH`) into flowey vars
- Convert flowey vars back into ADO variables for use in YAML
- Handle secret variables appropriately

**Repository Resources:**
- Resolve repository IDs declared as pipeline resources
- Access repository information in ADO-specific steps

### GhStepBuilder

`GhStepBuilder` is a fluent builder for constructing GitHub Actions steps with:

**Step Configuration:**
- Specifying the action to use (e.g., `actions/checkout@v4`)
- Adding input parameters via `.with()`
- Capturing step outputs into flowey variables
- Setting conditional execution based on variables

**Dependency Management:**
- Declaring side-effect dependencies via `.run_after()`
- Ensuring steps run in the correct order

**Permissions:**
- Declaring required GITHUB_TOKEN permissions
- Automatic permission aggregation at the job level

---

## Flowey Nodes

A **FlowNode** is a reusable unit of automation logic. Nodes process requests,
emit steps, and can depend on other nodes.

### The Node/Request Pattern

Every node has an associated **Request** type that defines what operations the node can perform. Requests are defined using the `flowey_request!` macro and registered with `new_flow_node!` or `new_simple_flow_node!` macros.

**Key concepts:**
- Each node is a struct registered with `new_flow_node!` or `new_simple_flow_node!`
- Request types define the node's API using `flowey_request!` macro
- Requests often include `WriteVar` parameters for outputs

For complete examples, see the [`FlowNode` trait documentation](https://docs.rs/flowey_core/latest/flowey_core/node/trait.FlowNode.html).

### FlowNode vs SimpleFlowNode

Flowey provides two node implementation patterns:

**FlowNode** - for nodes that need to process multiple requests together:
- Receives all requests as a `Vec<Request>`
- Can aggregate common requirements across requests and consolidate them into a single step to reduce repeated work
- Can resolve conflicts between requests

**SimpleFlowNode** - for nodes where each request is independent:
- Processes one request at a time
- Simpler implementation, less boilerplate
- Ideal for straightforward operations

For detailed comparisons and examples, see the [`FlowNode`](https://docs.rs/flowey_core/latest/flowey_core/node/trait.FlowNode.html) and [`SimpleFlowNode`](https://docs.rs/flowey_core/latest/flowey_core/node/trait.SimpleFlowNode.html) documentation.

### Node Registration

Nodes are automatically registered using macros that handle most of the boilerplate:
- `new_flow_node!(struct Node)` - registers a FlowNode
- `new_simple_flow_node!(struct Node)` - registers a SimpleFlowNode
- `flowey_request!` - defines the Request type and implements `IntoRequest`

### The imports() Method

The `imports()` method declares which other nodes this node might depend on. This enables flowey to:
- Validate that all dependencies are available
- Build the complete dependency graph
- Catch missing dependencies at build-time

```admonish warning
Flowey does not catch unused imports today as part of its build-time validation step.
```

**Why declare imports?** Flowey needs to know the full set of potentially-used nodes at compilation time to properly resolve the dependency graph.

For more on node imports, see the [`FlowNode::imports` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/trait.FlowNode.html#tymethod.imports).

### The emit() Method

The `emit()` method is where a node's actual logic lives. For `FlowNode`, it receives all requests together and must:
1. Aggregate and validate requests (ensuring consistency where needed)
2. Emit steps to perform the work
3. Wire up dependencies between steps via variables

For `SimpleFlowNode`, the equivalent `process_request()` method processes one request at a time.

For complete implementation examples, see the [`FlowNode::emit` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/trait.FlowNode.html#tymethod.emit).

---

## Variables: ReadVar and WriteVar

**ReadVar** and **WriteVar** are flowey's solution to the problem of declaring
variables at build-time that will hold values produced during pipeline runtime.

### The Problem They Solve

When constructing the pipeline graph, we don't yet know the values that will be
produced during execution (e.g., paths to built binaries, git commit hashes,
etc.). We need a way to:
1. Declare "this step will produce a value"
2. Declare "this step will consume that value"
3. Let flowey infer the execution order from these dependencies

### Write-Once Semantics

`WriteVar<T>` can only be written to **once**. This is fundamental to flowey's
execution model:

- Writing to a `WriteVar` consumes it (the type is not `Clone`)
- This ensures there's exactly one producer for each variable
- Flowey can use this to build a valid DAG (no cycles, no conflicts)

### Claiming Variables

Before a step can use a `ReadVar` or `WriteVar`, it must **claim** it:

Claiming serves several purposes:
1. Registers that this step depends on (or produces) this variable
2. Converts `ReadVar<T, VarNotClaimed>` to `ReadVar<T, VarClaimed>`
3. Allows flowey to track variable usage for graph construction

### ClaimedReadVar and ClaimedWriteVar

These are type aliases for claimed variables:
- `ClaimedReadVar<T> = ReadVar<T, VarClaimed>`
- `ClaimedWriteVar<T> = WriteVar<T, VarClaimed>`

Only claimed variables can be read/written at runtime.

### Static Values vs Runtime Values

Sometimes you know a value at build-time:

```rust
// Create a ReadVar with a static value
let version = ReadVar::from_static("1.2.3".to_string());

// This is encoded directly in the pipeline, not computed at runtime
// WARNING: Never use this for secrets!
```

This can be used as an escape hatch when you have Request (that expects a value to be determined at runtime), but in a given instance you know the value is known at build-time. 

### Variable Operations

`ReadVar` provides several useful operations for transforming and combining variables:

**Transform operations:**
- **`map()`**: Apply a function to transform a `ReadVar<T>` into a `ReadVar<U>`. Useful for deriving new values from existing variables (e.g., extracting a filename from a path, converting to uppercase).

**Combining operations:**
- **`zip()`**: Combine two ReadVars into a single `ReadVar<(T, U)>`. Useful when a step needs access to multiple values simultaneously.

**Dependency operations:**
- **`into_side_effect()`**: Discard the value but keep the dependency. Converts `ReadVar<T>` to `ReadVar<SideEffect>`, useful when you only care that a step ran, not what it produced.
- **`depending_on()`**: Create a new ReadVar that has an explicit dependency on another variable. Ensures ordering without actually using the dependent value.

For detailed examples of each operation, see the [`ReadVar` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/struct.ReadVar.html).

### The SideEffect Type

`SideEffect` is an alias for `()` that represents a dependency without data. It's used when you need to express that one step must run before another, but the first step doesn't produce any value that the second step needs to consume.

**Key concepts:**
- Represents "something happened" without carrying data
- Enables explicit dependency ordering between steps
- Commonly used for installation, initialization, or cleanup steps

For examples of using SideEffect, see the [`SideEffect` type documentation](https://docs.rs/flowey_core/latest/flowey_core/node/type.SideEffect.html).

---

## Node Design Philosophy

Flowey nodes are designed around several key principles:

### 1. Composability

Nodes should be reusable building blocks that can be combined to build complex
workflows. Each node should have a single, well-defined responsibility.

❌ **Bad**: A node that "builds and tests the project"  
✅ **Good**: Separate nodes for "build project" and "run tests"

### 2. Explicit Dependencies

Dependencies between steps should be explicit through variables, not implicit
through side effects.

❌ **Bad**: Assuming a tool is already installed  
✅ **Good**: Taking a `ReadVar<SideEffect>` that proves installation happened

### 3. Backend Abstraction

Nodes should work across all backends when possible. Backend-specific behavior
should be isolated and documented.

### 4. Separation of Concerns

Keep node definition (request types, dependencies) separate from step
implementation (runtime logic):

- **Node definition**: What the node does, what it depends on
- **Step implementation**: How it does it

### 5. Type Safety

Use Rust's type system to prevent errors at build-time:

- Typed artifacts ensure type-safe data passing
- `WriteVar` can only be written once (enforced by the type system)
- `ClaimVar` ensures variables are claimed before use
- Request validation happens during `emit()`, not at runtime

---

## Common Patterns

### Request Aggregation and Validation

When a FlowNode receives multiple requests, it often needs to ensure certain values are consistent across all requests while collecting others. The `same_across_all_reqs` helper function simplifies this pattern by validating that a value is identical across all requests.

**Key concepts:**
- Iterate through all requests and separate them by type
- Use `same_across_all_reqs` to validate values that must be consistent
- Collect values that can have multiple instances (like output variables)
- Validate that required values were provided

For a complete example, see the [`same_across_all_reqs` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/user_facing/fn.same_across_all_reqs.html).

### Conditional Execution Based on Backend/Platform

Nodes can query the current backend and platform to emit platform-specific or backend-specific steps. This allows nodes to adapt their behavior based on the execution environment.

**Key concepts:**
- Use `ctx.backend()` to check if running locally, on ADO, or on GitHub Actions
- Use `ctx.platform()` to check the operating system (Windows, Linux, macOS)
- Use `ctx.arch()` to check the architecture (x86_64, Aarch64)
- Emit different steps or use different tool configurations based on these values

**When to use:**
- Installing platform-specific tools or dependencies
- Using different commands on Windows vs Unix systems
- Optimizing for local development vs CI environments

For more on backend and platform APIs, see the [`NodeCtx` documentation](https://docs.rs/flowey_core/latest/flowey_core/node/struct.NodeCtx.html).

### Using the flowey_request! Macro

The `flowey_request!` macro generates the Request type and associated boilerplate for a node. It supports three main formats to accommodate different node complexity levels.

**Format options:**
- **`enum_struct`**: Recommended for complex requests. Creates an enum where each variant is a separate struct in a `req` module, providing better organization
- **`enum`**: Simple enum for straightforward request types
- **`struct`**: Single request type for nodes that only do one thing

The macro automatically derives `Serialize`, `Deserialize`, and implements the `IntoRequest` trait.

For complete syntax and examples, see the [`flowey_request!` macro documentation](https://docs.rs/flowey_core/latest/flowey_core/macro.flowey_request.html).

---

## Artifacts

**Artifacts** are the mechanism for passing data between jobs in a pipeline.
When one job produces output that another job needs, that output is packaged as
an artifact.

### Typed vs Untyped Artifacts

**Typed artifacts (preferred)** provide type-safe artifact handling by defining
a custom type that implements the `Artifact` trait. **Untyped artifacts** provide
simple directory-based artifacts for simpler cases.

For detailed examples of defining and using artifacts, see the [Artifact trait documentation](https://docs.rs/flowey_core/latest/flowey_core/pipeline/trait.Artifact.html).

Key concepts:
- The `Artifact` trait works by serializing your type to JSON in a format that reflects a directory structure
- Use `#[serde(rename = "file.exe")]` to specify exact file names
- Typed artifacts ensure compile-time type safety when passing data between jobs
- Untyped artifacts are simpler but don't provide type guarantees

### How Artifacts Create Dependencies

When you use an artifact in a job, flowey automatically:
1. Creates a dependency from the consuming job to the producing job
2. Ensures the producing job runs first
3. Handles artifact upload/download between jobs (on CI backends)

---

## Pipelines

A **Pipeline** is the top-level construct that defines a complete automation
workflow. Pipelines consist of one or more **Jobs**, each of which runs a set
of **Nodes** to accomplish specific tasks.

For detailed examples of defining pipelines, see the [IntoPipeline trait documentation](https://docs.rs/flowey_core/latest/flowey_core/pipeline/trait.IntoPipeline.html).

### Pipeline Jobs

Each `PipelineJob` represents a unit of work that:
- Runs on a specific platform and architecture
- Can depend on artifacts from other jobs
- Can be conditionally executed based on parameters
- Emits a sequence of steps that accomplish the job's goals

Jobs are configured using a builder pattern:

```rust
let job = pipeline
    .new_job(platform, arch, "my-job")
    .with_timeout_in_minutes(60)
    .with_condition(some_param)
    .ado_set_pool("my-pool")
    .gh_set_pool(GhRunner::UbuntuLatest)
    .dep_on(|ctx| {
        // Define what nodes this job depends on
        some_node::Request { /* ... */ }
    })
    .finish();
```

### Pipeline Parameters

Parameters allow runtime configuration of pipelines:

```rust
// Define a boolean parameter
let use_cache = pipeline.new_parameter_bool(
    "use_cache",
    "Whether to use caching",
    ParameterKind::Stable,
    Some(true) // default value
);

// Use the parameter in a job
let job = pipeline.new_job(...)
    .dep_on(|ctx| {
        let use_cache = ctx.use_parameter(use_cache);
        // use_cache is now a ReadVar<bool>
    })
    .finish();
```

Parameter types:
- Boolean parameters
- String parameters with optional validation
- Numeric (i64) parameters with optional validation

#### Stable vs Unstable Parameters

Every parameter in flowey must be declared as either **Stable** or **Unstable** using `ParameterKind`. This classification determines the parameter's visibility and API stability:

**Stable Parameters (`ParameterKind::Stable`)**

Stable parameters represent a **public, stable API** for the pipeline:

- **External Visibility**: The parameter name is exposed as-is in the generated CI YAML, making it callable by external pipelines and users.
- **API Contract**: Once a parameter is marked stable, its name and behavior should be maintained for backward compatibility. Removing or renaming a stable parameter is a breaking change.
- **Use Cases**: 
  - Parameters that control major pipeline behavior (e.g., `enable_tests`, `build_configuration`)
  - Parameters intended for use by other teams or external automation
  - Parameters documented as part of the pipeline's public interface

**Unstable Parameters (`ParameterKind::Unstable`)**

Unstable parameters are for **internal use** and experimentation:

- **Internal Only**: The parameter name is prefixed with `__unstable_` in the generated YAML (e.g., `__unstable_debug_mode`), signaling that it's not part of the stable API.
- **No Stability Guarantee**: Unstable parameters can be renamed, removed, or have their behavior changed without notice. External consumers should not depend on them.
- **Use Cases**:
  - Experimental features or debugging flags
  - Internal pipeline configuration that may change frequently
  - Parameters for development/testing that shouldn't be used in production

## Additional Resources

- **Example nodes**: See `flowey/flowey_lib_common/src/` for many real-world examples
- **Pipeline examples**: See `flowey/flowey_hvlite/src/pipelines/` for complete pipelines
- **Core types**: Defined in `flowey/flowey_core/src/`
