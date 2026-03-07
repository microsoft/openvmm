# Steps

**Steps** are units of work that will be executed at runtime. Different
step types exist for different purposes.

## Types of Steps

### Rust Steps

Rust steps execute Rust code at runtime and are the most common step type in flowey.

**`emit_rust_step`**: The primary method for emitting steps that run Rust code. Steps can claim variables, read inputs, perform work, and write outputs. Returns an optional `ReadVar<SideEffect>` that other steps can use as a dependency.

**`emit_minor_rust_step`**: Similar to `emit_rust_step` but for steps that cannot fail (no `Result` return) and don't need visibility in CI logs. Used for simple transformations and glue logic. Using minor steps also improve performance, since there is a slight cost to starting and ending a 'step' in GitHub and ADO. During the build stage, minor steps that are adjacent to each other will get merged into one giant CI step.

**`emit_rust_stepv`**: Convenience method that combines creating a new variable and emitting a step in one call. The step's return value is automatically written to the new variable.

For detailed examples of Rust steps, see the [`NodeCtx` emit methods documentation](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.NodeCtx.html).

### ADO Steps

**`emit_ado_step`**: Emits a step that generates Azure DevOps Pipeline YAML. Takes a closure that returns a YAML string snippet which is interpolated into the generated pipeline.

For ADO step examples, see the [`NodeCtx::emit_ado_step` documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/struct.NodeCtx.html#method.emit_ado_step).

### GitHub Steps

**`emit_gh_step`**: Creates a GitHub Actions step using the fluent `GhStepBuilder` API. Supports specifying the action, parameters, outputs, dependencies, and permissions. Returns a builder that must be finalized with `.finish(ctx)`.

For GitHub step examples, see the [`GhStepBuilder` documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/steps/github/struct.GhStepBuilder.html).

### Side Effect Steps

**`emit_side_effect_step`**: Creates a dependency relationship without executing code. Useful for aggregating multiple side effect dependencies into a single side effect. More efficient than emitting an empty Rust step.

For side effect step examples, see the [`NodeCtx::emit_side_effect_step` documentation](https://openvmm.dev/rustdoc/linux/flowey_core/node/struct.NodeCtx.html#method.emit_side_effect_step).

### Isolated Working Directories and Path Immutability

```admonish warning title="Critical Constraint"
**Each step gets its own fresh local working directory.** This avoids the "single global working directory dumping ground" common in bash + YAML systems.

However, while flowey variables enforce sharing XOR mutability at the type-system level, **developers must manually enforce this at the filesystem level**:

**Steps must NEVER modify the contents of paths referenced by `ReadVar<PathBuf>`.**
```

When you write a path to `WriteVar<PathBuf>`, you're creating an immutable contract. Other steps reading that path must treat it as read-only. If you need to modify files from a `ReadVar<PathBuf>`, copy them to your step's working directory.

## Runtime Services

Runtime services provide the API available during step execution (inside the
closures passed to `emit_rust_step`, etc.).

### RustRuntimeServices

[`RustRuntimeServices`](https://openvmm.dev/rustdoc/linux/flowey_core/node/steps/rust/struct.RustRuntimeServices.html) is the primary runtime service available in Rust steps. It provides:

#### Variable Operations

- Reading and writing flowey variables
- Secret handling (automatic secret propagation for safety)
- Support for reading values of any type that implements [`ReadVarValue`](https://openvmm.dev/rustdoc/linux/flowey_core/node/trait.ReadVarValue.html)

#### Environment Queries

- Backend identification (Local, ADO, or GitHub)
- Platform detection (Windows, Linux, macOS)
- Architecture information (x86_64, Aarch64)

### AdoStepServices

[`AdoStepServices`](https://openvmm.dev/rustdoc/linux/flowey_core/node/steps/ado/struct.AdoStepServices.html) provides integration with Azure DevOps-specific features when emitting ADO YAML steps:

**ADO Variable Bridge:**

- Convert ADO runtime variables (like `BUILD.SOURCEBRANCH`) into flowey vars
- Convert flowey vars back into ADO variables for use in YAML
- Handle secret variables appropriately

**Repository Resources:**

- Resolve repository IDs declared as pipeline resources
- Access repository information in ADO-specific steps

### GhStepBuilder

[`GhStepBuilder`](https://openvmm.dev/rustdoc/linux/flowey_core/node/steps/github/struct.GhStepBuilder.html) is a fluent builder for constructing GitHub Actions steps with:

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

## Secret Variables and CI Backend Integration

Flowey provides built-in support for handling sensitive data like API keys, tokens, and credentials through **secret variables**. Secret variables are treated specially to prevent accidental exposure in logs and CI outputs.

### How Secret Handling Works

When a variable is marked as secret, flowey ensures:

- The value is not logged or printed in step output
- CI backends (ADO, GitHub Actions) are instructed to mask the value in their logs
- Secret status is automatically propagated to prevent leaks

### Automatic Secret Propagation

To prevent accidental leaks, flowey uses conservative automatic secret propagation:

```admonish warning
If a step reads a secret value, **all subsequent writes from that step are automatically marked as secret** by default. This prevents accidentally leaking secrets through derived values.
```

For example:

```rust,ignore
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

```rust,ignore
rt.write_not_secret(output_var, &"done".to_string());
```

### Best Practices for Secrets

1. **Never use `ReadVar::from_static()` for secrets** - static values are encoded in plain text in the generated YAML
2. **Always use `write_secret()`** when writing sensitive data like tokens, passwords, or keys
3. **Minimize secret lifetime** - read secrets as late as possible and don't pass them through more variables than necessary
