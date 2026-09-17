# Nodes

At a conceptual level, a Flowey node is analogous to a strongly typed function: you "invoke" it by submitting one or more Request values (its parameters), and it responds by emitting steps that perform work and produce outputs (values written to `WriteVar`s, published artifacts, or side-effect dependencies).

## The Node/Request Pattern

Every node has an associated **Request** type that defines what operations the node can perform. Requests are defined using the [`flowey_request!`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.flowey_request.html) macro and registered with [`new_flow_node!`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.new_flow_node.html) or [`new_simple_flow_node!`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.new_simple_flow_node.html) macros.

For complete examples, see the [`FlowNode` trait documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html).

## FlowNode vs SimpleFlowNode

Flowey provides two node implementation patterns with a fundamental difference in their Request structure and complexity:

[**`SimpleFlowNode`**](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.SimpleFlowNode.html) - for straightforward, function-like operations:

- Uses a **single struct Request** type
- Processes one request at a time independently
- Behaves like a "plain old function" that resolves its single request type
- Each invocation is isolated - no shared state or coordination between requests
- Simpler implementation with less boilerplate
- Ideal for straightforward operations like running a command or transforming data

**Example use case**: A node that runs `cargo build` - each request is independent and just needs to know what to build.

[**`FlowNode`**](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html) - for complex nodes requiring coordination and non-local configuration:

- Often uses an **enum Request** with multiple variants
- Receives all requests as a `Vec<Request>` and processes them together
- Can aggregate, optimize, and consolidate multiple requests into fewer steps
- Enables **non-local configuration** - critical for simplifying complex pipelines

### The Non-Local Configuration Pattern

The key advantage of `FlowNode` is its ability to accept configuration from different parts of the node graph without forcing intermediate nodes to be aware of that configuration. This is the "non-local" aspect.

For nodes that accept **config values** — versions, feature flags, local paths — use `FlowNodeWithConfig` with a typed config struct. Config is declared with the `flowey_config!` macro, set via `ctx.config(...)` by any caller, and automatically merged across all callers before being delivered to `emit()` separately from action requests.

Consider an "install Rust toolchain" node:

```rust,ignore
flowey_config! {
    pub struct Config {
        pub version: Option<String>,
        pub auto_install: Option<bool>,
    }
}

flowey_request! {
    pub enum Request {
        GetToolchain(WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) { /* ... */ }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let version = config.version
            .expect("version must be set by cfg_versions");
        // ... use version to install, then fulfill GetToolchain requests
        Ok(())
    }
}
```

Callers set config and submit requests independently:

```rust,ignore
// A top-level job configuration node sets the version once:
ctx.config(install_rust::Config {
    version: Some("1.75".into()),
    ..Default::default()
});

// Any node that needs the Rust toolchain just requests it:
let toolchain = ctx.reqv(|v| install_rust::Request::GetToolchain(v));
```

```txt
Root Node → config(version: "1.75")
  → Node A
    → Node B
      → Node C → req(GetToolchain)
```

Flowey merges all config partials automatically. If two callers set the same field, the values must agree or the build fails. The intermediate nodes (A, B, C) never need to know about or pass through the version.

This pattern:

- **Eliminates plumbing complexity** in large pipelines
- **Allows global configuration** to be set once at the top level
- **Keeps unrelated nodes decoupled** from configuration they don't need
- **Validates consistency** — conflicting config values are caught at build time
- **Separates concerns** — config (what version?) is distinct from requests (give me the toolchain path)

**Additional Benefits of FlowNode:**

- Optimize and consolidate multiple similar requests into fewer steps (e.g., installing a tool once for many consumers)
- Resolve conflicts or enforce consistency across requests

For detailed comparisons and examples, see the [`FlowNode`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html) and [`SimpleFlowNode`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.SimpleFlowNode.html) documentation.

## Node Registration

Nodes are automatically registered using macros that handle most of the boilerplate:

- [`new_flow_node!(struct Node)`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.new_flow_node.html) - registers a FlowNode
- [`new_flow_node_with_config!(struct Node)`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.new_flow_node_with_config.html) - registers a FlowNodeWithConfig (a FlowNode that also accepts typed config)
- [`new_simple_flow_node!(struct Node)`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.new_simple_flow_node.html) - registers a SimpleFlowNode
- [`flowey_request!`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.flowey_request.html) - defines the Request type and implements [`IntoRequest`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.IntoRequest.html)
- [`flowey_config!`](https://openvmm.dev/rustdoc/linux/flowey_core/macro.flowey_config.html) - defines a Config struct with automatic merge support and implements [`IntoConfig`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.IntoConfig.html)

## The imports() Method

The `imports()` method declares which other nodes this node might depend on. This enables flowey to:

- Validate that all dependencies are available
- Build the complete dependency graph
- Catch missing dependencies at build-time

```admonish warning
Flowey does not catch unused imports today as part of its build-time validation step.
```

**Why declare imports?** Flowey needs to know the full set of potentially-used nodes at compilation time to properly resolve the dependency graph.

For more on node imports, see the [`FlowNode::imports` documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html#tymethod.imports).

## The emit() Method

The [`emit()`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html#tymethod.emit) method is where a node's actual logic lives. For [`FlowNode`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html), it receives all requests together and must:

1. Aggregate and validate requests (ensuring consistency where needed)
2. Emit steps to perform the work
3. Wire up dependencies between steps via variables

For [`SimpleFlowNode`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.SimpleFlowNode.html), the equivalent [`process_request()`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.SimpleFlowNode.html#tymethod.process_request) method processes one request at a time.

For complete implementation examples, see the [`FlowNode::emit` documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.FlowNode.html#tymethod.emit).

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

## Common Patterns

### Node Config vs Request

When designing a `FlowNode`, separate **config** (values that must be consistent across all callers) from **requests** (actions the node performs):

- **Config** (`flowey_config!`): version strings, feature flags, local override paths, auto-install toggles. Use `FlowNodeWithConfig` and `ctx.config(...)`. Config fields are `Option<T>` or `BTreeMap<K, V>` — flowey merges them automatically and errors on conflicts.
- **Requests** (`flowey_request!`): actions that produce outputs, e.g. `GetBinary(WriteVar<PathBuf>)`. Multiple callers can each submit requests, and the node fulfills all of them.

```admonish tip
If a value needs `same_across_all_reqs` validation, it should probably be a config field instead. The `flowey_config!` macro provides this validation automatically during merge.
```

### Request Aggregation and Validation

When a FlowNode receives multiple requests, it may need to aggregate or validate them. Common techniques:

- Iterate through all requests and separate them by variant
- Collect output variables that can have multiple instances
- Validate that required values were provided

For values that must be consistent across all callers (versions, flags), prefer using `flowey_config!` with `FlowNodeWithConfig` instead of manual validation — see [The Non-Local Configuration Pattern](#the-non-local-configuration-pattern) above.

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

For more on backend and platform APIs, see the [`NodeCtx` documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/struct.NodeCtx.html).
