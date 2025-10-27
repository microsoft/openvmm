# cargo xflowey

To implement various developer workflows (both locally, as well as in CI), the
OpenVMM project relies on `flowey`: a custom, in-house Rust library/framework
for writing maintainable, cross-platform automation.

`cargo xflowey` is a cargo alias that makes it easy for developers to run
`flowey`-based pipelines locally.

Some particularly notable pipelines:

- `cargo xflowey build-igvm` - primarily dev-tool used to build OpenHCL IGVM files locally
- `cargo xflowey restore-packages` - restores external packages needed to compile and run OpenVMM / OpenHCL

> **Note**: While `cargo xflowey` technically has the ability to run CI pipelines 
> locally (e.g., `cargo xflowey ci checkin-gates`), this functionality is 
> currently broken and should not be relied upon. Use CI pipelines in their 
> intended environments (Azure DevOps or GitHub Actions).

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
2. [Pipelines](#pipelines)
3. [Artifacts](#artifacts)
4. [Flowey Nodes](#flowey-nodes)
5. [Variables: ReadVar and WriteVar](#variables-readvar-and-writevar)
6. [Emitting Steps](#emitting-steps)
7. [Runtime Services](#runtime-services)
8. [Node Design Philosophy](#node-design-philosophy)
9. [Common Patterns](#common-patterns)

---

## Core Concepts

### Two-Phase Execution Model

Flowey operates in two distinct phases:

1. **Build-Time (Resolution Phase)**: When you run `cargo xflowey`, flowey
   constructs a directed acyclic graph (DAG) of steps by:
   - Instantiating all nodes
   - Processing their requests
   - Resolving dependencies between nodes via variables
   - Determining the execution order

2. **Runtime (Execution Phase)**: The generated flow is executed, and steps run
   in the computed order. During runtime:
   - Variables are read and written with actual values
   - Commands are executed
   - Artifacts are published/consumed
   - Side effects occur

This separation allows flowey to:
- Validate the entire workflow before execution
- Generate YAML for CI systems (ADO, GitHub Actions)
- Optimize step ordering and parallelization
- Catch dependency errors at build-time

### Backend Abstraction

Flowey supports multiple execution backends:

- **Local**: Runs directly on your development machine via bash or direct
  execution
- **ADO (Azure DevOps)**: Generates ADO Pipeline YAML
- **GitHub Actions**: Generates GitHub Actions workflow YAML

**Important**: Nodes should be written to work across ALL backends whenever 
possible. Relying on `ctx.backend()` to query the backend or manually emitting 
backend-specific steps (via `emit_ado_step` or `emit_gh_step`) should be 
avoided unless absolutely necessary. Most automation logic should be 
backend-agnostic, using `emit_rust_step` for cross-platform Rust code that 
works everywhere.

---

## Pipelines

A **Pipeline** is the top-level construct that defines a complete automation
workflow. Pipelines consist of one or more **Jobs**, each of which runs a set
of **Nodes** to accomplish specific tasks.

### Defining a Pipeline

```rust
fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
    let mut pipeline = Pipeline::new();
    
    // Define a job that runs on Linux x86_64
    let job = pipeline
        .new_job(FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu), FlowArch::X86_64, "build")
        .finish();
    
    Ok(pipeline)
}
```

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

### Backend-Specific Configuration

**ADO-specific:**
- `ado_set_pool()`: Specify agent pool
- `ado_set_pr_triggers()`: Configure PR triggers
- `ado_set_ci_triggers()`: Configure CI triggers
- `ado_add_resources_repository()`: Add repository resources

**GitHub Actions-specific:**
- `gh_set_pool()`: Specify runner (GitHub-hosted or self-hosted)
- `gh_set_pr_triggers()`: Configure PR triggers
- `gh_set_ci_triggers()`: Configure CI triggers
- `gh_grant_permissions()`: Grant GITHUB_TOKEN permissions

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
- `new_parameter_bool()`: Boolean parameters
- `new_parameter_string()`: String parameters with optional validation
- `new_parameter_num()`: Numeric (i64) parameters with optional validation

---

## Artifacts

**Artifacts** are the mechanism for passing data between jobs in a pipeline.
When one job produces output that another job needs, that output is packaged as
an artifact.

### Typed vs Untyped Artifacts

**Typed artifacts (preferred)** provide type-safe artifact handling by defining
a custom type that implements the `Artifact` trait. This type describes the 
structure of files that will be published and consumed.

#### The Artifact Trait

The `Artifact` trait is the foundation of typed artifacts:

The trait works by serializing your type to JSON in a format that reflects a 
directory structure:
- Each JSON key is a file name (use `#[serde(rename = "file.exe")]`)
- Each value is either a string containing the path to the file, or another 
  JSON object representing a subdirectory
- Optional fields allow for conditional file inclusion

#### Example: Defining a Typed Artifact

Here's a real-world example from the codebase:

```rust
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum PipetteOutput {
    LinuxBin {
        #[serde(rename = "pipette")]
        bin: PathBuf,
        #[serde(rename = "pipette.dbg")]
        dbg: PathBuf,
    },
    WindowsBin {
        #[serde(rename = "pipette.exe")]
        exe: PathBuf,
        #[serde(rename = "pipette.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for PipetteOutput {}
```

This enum represents either a Linux build (with `pipette` binary and `pipette.dbg`
debug symbols) or a Windows build (with `pipette.exe` and `pipette.pdb`). The 
`#[serde(rename = "...")]` attributes specify the exact file names that will 
appear in the published artifact.

#### Using Typed Artifacts in Pipelines

```rust
// In pipeline definition - create the artifact handles
let (publish_pipette, use_pipette) = pipeline.new_typed_artifact::<PipetteOutput>("pipette");

// In producer job - write the artifact
let job1 = pipeline.new_job(...)
    .dep_on(|ctx| {
        let pipette = ctx.publish_typed_artifact(publish_pipette);
        // pipette is a WriteVar<PipetteOutput>
        
        // In a node, write the appropriate variant:
        ctx.emit_rust_step("build pipette", |ctx| {
            let pipette = pipette.claim(ctx);
            move |rt| {
                let output = PipetteOutput::WindowsBin {
                    exe: PathBuf::from("path/to/pipette.exe"),
                    pdb: PathBuf::from("path/to/pipette.pdb"),
                };
                rt.write(pipette, &output);
                Ok(())
            }
        });
    })
    .finish();

// In consumer job - read the artifact
let job2 = pipeline.new_job(...)
    .dep_on(|ctx| {
        let pipette = ctx.use_typed_artifact(&use_pipette);
        // pipette is a ReadVar<PipetteOutput>
        
        ctx.emit_rust_step("use pipette", |ctx| {
            let pipette = pipette.claim(ctx);
            move |rt| {
                let output = rt.read(pipette);
                match output {
                    PipetteOutput::WindowsBin { exe, pdb } => {
                        // Use the Windows binaries
                    }
                    PipetteOutput::LinuxBin { bin, dbg } => {
                        // Use the Linux binaries
                    }
                }
                Ok(())
            }
        });
    })
    .finish();
```

#### Untyped Artifacts

**Untyped artifacts** provide simple directory-based artifacts for cases where
you don't need type safety:

```rust
let (publish, use_artifact) = pipeline.new_artifact("my-artifact");

// Producer gets a path to an empty directory to populate
let artifact_dir = ctx.publish_artifact(publish);  // ReadVar<PathBuf>

// Consumer gets a path to the populated directory
let artifact_dir = ctx.use_artifact(&use_artifact);  // ReadVar<PathBuf>
```

Use untyped artifacts when:
- The artifact structure is simple or ad-hoc
- You don't need compile-time guarantees about file names/structure
- The artifact is primarily used by a single node

### How Artifacts Create Dependencies

When you use an artifact in a job, flowey automatically:
1. Creates a dependency from the consuming job to the producing job
2. Ensures the producing job runs first
3. Handles artifact upload/download between jobs (on CI backends)

---

## Flowey Nodes

A **FlowNode** is a reusable unit of automation logic. Nodes process requests,
emit steps, and can depend on other nodes.

### The Node/Request Pattern

Every node has an associated **Request** type that defines what the node can do:

```rust
// Define the node
new_flow_node!(struct Node);

// Define requests using the flowey_request! macro
flowey_request! {
    pub enum Request {
        InstallRust(String),           // Install specific version
        EnsureInstalled(WriteVar<SideEffect>),  // Ensure it's installed
        GetCargoHome(WriteVar<PathBuf>),        // Get CARGO_HOME path
    }
}
```

### FlowNode vs SimpleFlowNode

**Use `FlowNode`** when you need to:
- Aggregate multiple requests and process them together
- Resolve conflicts between requests
- Perform complex request validation

**Use `SimpleFlowNode`** when:
- Each request can be processed independently
- No aggregation logic is needed
- Simpler, less boilerplate

```rust
// FlowNode - processes all requests together
impl FlowNode for Node {
    type Request = Request;
    
    fn imports(ctx: &mut ImportCtx<'_>) {
        // Declare node dependencies
        ctx.import::<other_node::Node>();
    }
    
    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // Process all requests, aggregate common requirements
        // Emit steps to accomplish the work
        Ok(())
    }
}

// SimpleFlowNode - processes one request at a time
impl SimpleFlowNode for Node {
    type Request = Request;
    
    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<other_node::Node>();
    }
    
    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // Process single request
        Ok(())
    }
}
```

### Node Registration

Nodes are automatically registered using macros:
- `new_flow_node!(struct Node)` - registers a FlowNode
- `new_simple_flow_node!(struct Node)` - registers a SimpleFlowNode
- `flowey_request!` - defines the Request type and implements `IntoRequest`

### The imports() Method

The `imports()` method declares which other nodes this node might depend on:

```rust
fn imports(ctx: &mut ImportCtx<'_>) {
    ctx.import::<install_rust::Node>();
    ctx.import::<install_git::Node>();
}
```

This allows flowey to:
- Validate that all dependencies are available
- Build the complete dependency graph
- Catch missing dependencies at build-time

### The emit() Method

The `emit()` method is where the node's actual logic lives:

```rust
fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
    // 1. Aggregate and validate requests
    let mut version = None;
    let mut ensure_installed = Vec::new();
    
    for req in requests {
        match req {
            Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
            Request::EnsureInstalled(var) => ensure_installed.push(var),
        }
    }
    
    // 2. Emit steps to do the work
    ctx.emit_rust_step("install rust", |ctx| {
        let ensure_installed = ensure_installed.claim(ctx);
        move |rt| {
            // Runtime logic here
            Ok(())
        }
    });
    
    Ok(())
}
```

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

```rust
let (read, write) = ctx.new_var::<String>();

// Later, in a step:
rt.write(write, &"hello".to_string());  // write is consumed here
```

### Claiming Variables

Before a step can use a `ReadVar` or `WriteVar`, it must **claim** it:

```rust
ctx.emit_rust_step("my step", |ctx| {
    // Claim variables for this step
    let read_var = some_read_var.claim(ctx);
    let write_var = some_write_var.claim(ctx);
    
    // Return the runtime closure
    move |rt| {
        let value = rt.read(read_var);
        rt.write(write_var, &modified_value);
        Ok(())
    }
});
```

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

### Variable Operations

ReadVar provides several useful operations:

```rust
// Transform a value
let uppercase = lowercase.map(ctx, |s| s.to_uppercase());

// Combine two variables
let combined = var1.zip(ctx, var2);  // ReadVar<(T, U)>

// Discard the value, keep only the dependency
let side_effect = var.into_side_effect();  // ReadVar<SideEffect>

// Create a dependency without using the value
let dependent = var.depending_on(ctx, &other_var);
```

### The SideEffect Type

`SideEffect` is an alias for `()` that represents a dependency without data:

```rust
// This step produces a side effect (e.g., installs a tool)
let installed = ctx.emit_rust_step("install tool", |ctx| {
    let done = done.claim(ctx);
    move |rt| {
        // install the tool
        rt.write(done, &());  // SideEffect is ()
        Ok(())
    }
});

// Other steps can depend on this happening
ctx.emit_rust_step("use tool", |ctx| {
    installed.claim(ctx);  // Ensures install happens first
    move |rt| {
        // use the tool
        Ok(())
    }
});
```

---

## Emitting Steps

Nodes emit **steps** - units of work that will be executed at runtime. Different
step types exist for different purposes.

### Rust Steps

**`emit_rust_step`**: Emits a step that runs Rust code at runtime. This is the
most common step type.

```rust
ctx.emit_rust_step("build the project", |ctx| {
    let source_dir = source_dir.claim(ctx);
    let output = output.claim(ctx);
    
    move |rt| {
        let source = rt.read(source_dir);
        let result = build_project(&source)?;
        rt.write(output, &result);
        Ok(())
    }
});
```

**`emit_minor_rust_step`**: Like `emit_rust_step`, but for steps that:
- Cannot fail (closure returns `T` not `anyhow::Result<T>`)
- Don't need to be visible in CI logs as separate steps

This reduces log clutter for trivial operations like variable transformations.

**`emit_rust_stepv`**: A convenience method that creates a new variable and
returns it:

```rust
// Instead of:
let (read, write) = ctx.new_var();
ctx.emit_rust_step("compute value", |ctx| {
    let write = write.claim(ctx);
    move |rt| {
        rt.write(write, &compute());
        Ok(())
    }
});

// You can write:
let read = ctx.emit_rust_stepv("compute value", |ctx| {
    move |rt| Ok(compute())
});
```

### ADO Steps

**`emit_ado_step`**: Emits an Azure DevOps YAML step.

```rust
ctx.emit_ado_step("checkout code", |ctx| {
    move |rt| {
        r#"
        - checkout: self
          clean: true
        "#.to_string()
    }
});
```

### GitHub Steps

**`emit_gh_step`**: Builds a GitHub Actions step using `GhStepBuilder`.

```rust
ctx.emit_gh_step("Checkout code", "actions/checkout@v4")
    .with("fetch-depth", "0")
    .finish(ctx);
```

### Side Effect Steps

**`emit_side_effect_step`**: Creates a dependency relationship without executing
any code. Useful for resolving multiple side effects into one.

```rust
ctx.emit_side_effect_step(
    vec![dependency1, dependency2],  // use these
    vec![output_side_effect],        // resolve this
);
```

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

Available in Rust steps via the `rt` parameter:

```rust
move |rt: &mut RustRuntimeServices<'_>| {
    // Read variables
    let value = rt.read(some_var);
    
    // Write variables
    rt.write(output_var, &result);
    rt.write_secret(secret_var, &secret);  // Mark as secret
    
    // Query environment
    let backend = rt.backend();    // Local, ADO, or Github
    let platform = rt.platform();  // Windows, Linux, MacOs
    let arch = rt.arch();          // X86_64, Aarch64
    
    Ok(())
}
```

**Important**: If a step reads a secret value, all subsequent writes from that
step are marked as secret by default (to prevent accidental leaks). Use
`write_not_secret()` if you need to override this.

### AdoStepServices

Available in ADO steps for interacting with ADO-specific features:

```rust
move |rt: &mut AdoStepServices<'_>| {
    // Get ADO variable as flowey var
    rt.set_var(flowey_var, AdoRuntimeVar::BUILD__SOURCE_BRANCH);
    
    // Set ADO variable from flowey var
    let ado_var = rt.get_var(flowey_var);
    
    // Resolve repository ID
    let repo = rt.resolve_repository_id(repo_id);
    
    "- task: SomeTask@1".to_string()
}
```

### GhStepBuilder

Builder for GitHub Actions steps:

```rust
ctx.emit_gh_step("Azure Login", "Azure/login@v2")
    .with("client-id", client_id)           // Add parameter
    .with("tenant-id", tenant_id)
    .output("token", token_var)             // Capture output
    .run_after(some_side_effect)            // Add dependency
    .requires_permission(                   // Declare permission needed
        GhPermission::IdToken,
        GhPermissionValue::Write
    )
    .finish(ctx);
```

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

```rust
match ctx.backend() {
    FlowBackend::Local => {
        // Local-specific logic
    }
    FlowBackend::Ado => {
        // ADO-specific logic
    }
    FlowBackend::Github => {
        // GitHub-specific logic
    }
}
```

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

When processing multiple requests, use helper functions to ensure consistency:

```rust
fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
    let mut version = None;
    let mut ensure_installed = Vec::new();
    
    for req in requests {
        match req {
            Request::Version(v) => {
                // Ensure all requests agree on the version
                same_across_all_reqs("Version", &mut version, v)?;
            }
            Request::EnsureInstalled(v) => {
                ensure_installed.push(v);
            }
        }
    }
    
    let version = version.ok_or(anyhow::anyhow!("Missing required request: Version"))?;
    
    // ... emit steps using aggregated requests
}
```

### Conditional Execution Based on Backend/Platform

```rust
// Only emit this step on Windows
if ctx.platform() == FlowPlatform::Windows {
    ctx.emit_rust_step("windows-specific step", |ctx| {
        move |rt| {
            // Windows-specific logic
            Ok(())
        }
    });
}

// Different behavior per backend
match ctx.backend() {
    FlowBackend::Local => {
        // Check if tool is already installed
    }
    FlowBackend::Ado | FlowBackend::Github => {
        // Always install the tool
    }
}
```

### Working with Persistent Directories

Some nodes need to persist data between runs (e.g., caches). Use
`ctx.persistent_dir()`:

```rust
if let Some(cache_dir) = ctx.persistent_dir() {
    // Have a persistent directory, can cache things
    ctx.emit_rust_step("restore from cache", |ctx| {
        let cache_dir = cache_dir.claim(ctx);
        move |rt| {
            let dir = rt.read(cache_dir);
            // Restore from cache
            Ok(())
        }
    });
} else {
    // No persistent storage available, skip caching
}
```

### Using the flowey_request! Macro

The `flowey_request!` macro supports several formats:

```rust
// Enum with separate struct per variant (recommended for complex requests)
flowey_request! {
    pub enum_struct Request {
        Install { version: String, components: Vec<String> },
        Check(pub WriteVar<bool>),
        GetPath(pub WriteVar<PathBuf>),
    }
}
// This generates Request::Install(req::Install), Request::Check(req::Check), etc.

// Simple enum (for simple requests)
flowey_request! {
    pub enum Request {
        Install { version: String },
        Check(WriteVar<bool>),
    }
}

// Struct (for nodes with a single request type)
flowey_request! {
    pub struct Request {
        pub input: ReadVar<String>,
        pub output: WriteVar<String>,
    }
}
```

---

## Additional Resources

- **Example nodes**: See `flowey/flowey_lib_common/src/` for many real-world examples
- **Pipeline examples**: See `flowey/flowey_hvlite/src/pipelines/` for complete pipelines
- **Core types**: Defined in `flowey/flowey_core/src/`
