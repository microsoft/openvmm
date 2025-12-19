# Flowey Fundamentals

Before diving into how flowey works, let's establish the key building blocks that form the foundation of flowey's automation model. These concepts are flowey's Rust-based abstractions for common CI/CD workflow primitives.

## The Automation Workflow Model

In traditional CI/CD systems, workflows are defined using YAML with implicit dependencies and global state. Flowey takes a fundamentally different approach: **automation workflows are modeled as a directed acyclic graph (DAG) of typed, composable Rust components**. Each component has explicit inputs and outputs, and dependencies are tracked through the type system.

### Core Building Blocks

Flowey's model consists of a hierarchy of components:

**[Pipelines](https://openvmm.dev/rustdoc/linux/flowey_core/pipeline/trait.IntoPipeline.html)** are the top-level construct that defines a complete automation workflow. A pipeline specifies what work needs to be done and how it should be organized. Pipelines can target different execution backends (local machine, Azure DevOps, GitHub Actions) and generate appropriate configuration for each.

**[Jobs](https://openvmm.dev/rustdoc/linux/flowey_core/pipeline/struct.PipelineJob.html)** represent units of work that run on a specific platform (Windows, Linux, macOS) and architecture (x86_64, Aarch64). Jobs can run in parallel when they don't depend on each other, or sequentially when one job's output is needed by another. Each job is isolated and runs in its own environment.

**[Nodes](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html)** are reusable units of automation logic that perform specific tasks (e.g., "install Rust toolchain", "run cargo build", "publish test results"). Nodes are invoked by jobs and emit one or more steps to accomplish their purpose. Nodes can depend on other nodes, forming a composable ecosystem of automation building blocks.

**Steps** are the individual units of work that execute at runtime. A step might run a shell command, execute Rust code, or interact with the CI backend. Steps are emitted by nodes during the build-time phase and executed in dependency order during runtime.

### Connecting the Pieces

These building blocks are connected through three key mechanisms:

**[Variables (`ReadVar`/`WriteVar`)](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.ReadVar.html)** enable data flow between steps. A `WriteVar<T>` represents a promise to produce a value of type `T` at runtime, while a `ReadVar<T>` represents a dependency on that value. Variables enforce write-once semantics (each value has exactly one producer) and create explicit dependencies in the DAG. For example, a "build" step might write a binary path to a `WriteVar<PathBuf>`, and a "test" step would read from the corresponding `ReadVar<PathBuf>`. This echoes Rust's "shared XOR mutable" ownership rule: a value has either one writer or multiple readers, never both concurrently.

**[Artifacts](https://openvmm.dev/rustdoc/linux/flowey_core/pipeline/trait.Artifact.html)** enable data transfer between jobs. Since jobs may run on different machines or at different times, artifacts package up files (like compiled binaries, test results, or build outputs) for transfer. Flowey automatically handles uploading artifacts at the end of producing jobs and downloading them at the start of consuming jobs, abstracting away backend-specific artifact APIs.

**[Side Effects](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/type.SideEffect.html)** represent dependencies without data. Sometimes step B needs to run after step A, but A doesn't produce any data that B consumes (e.g., "install dependencies" must happen before "run tests", even though the test step doesn't directly use the installation output). Side effects are represented as `ReadVar<SideEffect>` and establish ordering constraints in the DAG without transferring actual values.

### Putting It Together

Here's an example of how these pieces relate:

```txt
Pipeline
  ├─ Job 1 (Linux x86_64)
  │   ├─ Node A (install Rust) 
  │   │   └─ Step: Run rustup install
  │   │       └─ Produces: WriteVar<SideEffect> (installation complete)
  │   └─ Node B (build project)
  │       └─ Step: Run cargo build
  │           └─ Consumes: ReadVar<SideEffect> (installation complete)
  │           └─ Produces: WriteVar<PathBuf> (binary path) → Artifact
  │
  └─ Job 2 (Windows x86_64)
      └─ Node C (run tests)
          └─ Step: Run binary with test inputs
              └─ Consumes: ReadVar<PathBuf> (binary path) ← Artifact
              └─ Produces: WriteVar<PathBuf> (test results)
```

In this example:

- The **Pipeline** defines two jobs that run on different platforms
- **Job 1** installs Rust and builds the project, with step dependencies expressed through variables
- **Job 2** runs tests using the binary from Job 1, with the binary transferred via an artifact
- **Variables** create dependencies within a job (build depends on install)
- **Artifacts** create dependencies between jobs (Job 2 depends on Job 1's output)
- **Side Effects** represent the "Rust is installed" state without carrying data

## Two-Phase Execution Model

Flowey operates in two distinct phases:

1. **Build-Time (Resolution Phase)**: When you run `cargo xflowey regen`, flowey:
   - Reads `.flowey.toml` to determine which pipelines to regenerate
   - Builds the flowey binary (e.g., `flowey-hvlite`) via `cargo build`
   - Runs the flowey binary with `pipeline <backend> --out <file> <cmd>` for each pipeline definition
   - During this invocation, flowey constructs a **directed acyclic graph (DAG)** by:
     - Instantiating all nodes (reusable units of automation logic) defined in the pipeline
     - Processing their requests
     - Resolving dependencies between nodes via variables and artifacts
     - Determining the execution order
     - Performing flowey-specific validations (dependency resolution, type checking, etc.)
   - Generates YAML files for CI systems (ADO, GitHub Actions) at the paths specified in `.flowey.toml`

2. **Runtime (Execution Phase)**: The generated YAML is executed by the CI system (or locally via `cargo xflowey <pipeline>`). Steps (units of work) run in the order determined at build-time:
   - Variables are read and written with actual values
   - Commands are executed
   - Artifacts (data packages passed between jobs) are published/consumed
   - Side effects (dependencies) are resolved

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

### Backend Abstraction

Flowey supports multiple execution backends:

- **Local**: Runs directly on your development machine
- **ADO (Azure DevOps)**: Generates ADO Pipeline YAML
- **GitHub Actions**: Generates GitHub Actions workflow YAML

```admonish warning
Nodes should be written to work across ALL backends whenever possible. Relying on `ctx.backend()` to query the backend or manually emitting backend-specific steps (via `emit_ado_step` or `emit_gh_step`) should be avoided unless absolutely necessary. Most automation logic should be backend-agnostic, using `emit_rust_step` for cross-platform Rust code that works everywhere. Writing cross-platform flowey code enables locally testing pipelines which can be invaluable when iterating over CI changes. 
```

If a node only supports certain backends, it should immediately fast‑fail with a clear error ("`<Node>` not supported on `<backend>`") instead of silently proceeding. That failure signals it's time either to add the missing backend support or introduce a multi‑platform abstraction/meta‑node that delegates to platform‑specific nodes.
