# Variables

Variables are flowey's mechanism for creating typed data dependencies between steps. When a node emits steps, it uses `ReadVar<T>` and `WriteVar<T>` to declare what data each step consumes and produces. This creates explicit edges in the dependency graph: if step B reads from a variable that step A writes to, flowey ensures step A executes before step B.

## Claiming Variables

Before a step can use a [`ReadVar`](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.ReadVar.html) or [`WriteVar`](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.WriteVar.html), it must **claim** it. Claiming serves several purposes:

1. Registers that this step depends on (or produces) this variable
2. Converts `ReadVar<T, VarNotClaimed>` to `ReadVar<T, VarClaimed>`
3. Allows flowey to track variable usage for graph construction

Variables can only be claimed inside step closures using the `claim()` method.

**Nested closure pattern and related contexts:**

```rust
// Inside a SimpleFlowNode's process_request() method
fn process_request(&self, request: Self::Request, ctx: &mut NodeCtx<'_>) {
    // Assume a single Request provided an input ReadVar and output WriteVar
    let input_var: ReadVar<String> = /* from one of the requests */;
    let output_var: WriteVar<i32> = /* from one of the requests */;

    // Declare a step (still build-time). This adds a node to the DAG.
    ctx.emit_rust_step("compute length", |step| {
        // step : StepCtx  (outer closure, build-time)
        // Claim dependencies so the graph knows: this step READS input_var, WRITES output_var.
        let input_var = input_var.claim(step);
        let output_var = output_var.claim(step);

        // Return the runtime closure.
        move |rt| {
            // rt : RustRuntimeServices (runtime phase)
            let input = rt.read(input_var);      // consume value
            let len = input.len() as i32;
            rt.write(output_var, &len);          // fulfill promise
            Ok(())
        }
    });
}
```

**Why the nested closure dance?**

The nested closure pattern is fundamental to flowey's two-phase execution model:

1. **Build-Time (Outer Closure)**: When flowey constructs the DAG, the outer closure runs to:
   - Claim variables, which registers dependencies in the graph
   - Determine what this step depends on (reads) and produces (writes)
   - Allow flowey determine execution order
   - Returns an inner closure that gets invoked during the job's runtime
2. **Runtime (Inner Closure)**: When the pipeline actually executes, the inner closure runs to:
   - Read actual values from claimed `ReadVar`s
   - Perform the real work (computations, running commands, etc.)
   - Write actual values to claimed `WriteVar`s

- [**`NodeCtx`**](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.NodeCtx.html): Used when emitting steps (during the build-time phase). Provides `emit_*` methods, `new_var()`, `req()`, etc.
  
- [**`StepCtx`**](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.StepCtx.html): Used inside step closures (during runtime execution). Provides access to `claim()` for variables, and basic environment info (`backend()`, `platform()`).

The type system enforces this separation: `claim()` requires `StepCtx` (only available in the outer closure), while `read()`/`write()` require `RustRuntimeServices` (only available in the inner closure).

## ClaimedReadVar and ClaimedWriteVar

These are type aliases for claimed variables:

- [`ClaimedReadVar<T>`](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/type.ClaimedReadVar.html) = `ReadVar<T, VarClaimed>`
- [`ClaimedWriteVar<T>`](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/type.ClaimedWriteVar.html) = `WriteVar<T, VarClaimed>`

Only claimed variables can be read/written at runtime.

### Implementation Detail: Zero-Sized Types (ZSTs)

The claim state markers [`VarClaimed`](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/enum.VarClaimed.html) and [`VarNotClaimed`](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/enum.VarNotClaimed.html) are zero-sized types (ZSTs) - they exist purely at the type level. It allows Rust to statically verify that all variables used in a runtime block have been claimed by that block.

The type system ensures that `claim()` is the only way to convert from `VarNotClaimed` to `VarClaimed`, and this conversion can only happen within the outer closure where `StepCtx` is available.

## Static Values vs Runtime Values

Sometimes you know a value at build-time:

```rust
// Create a ReadVar with a static value
let version = ReadVar::from_static("1.2.3".to_string());

// This is encoded directly in the pipeline, not computed at runtime
// WARNING: Never use this for secrets!
```

This can be used as an escape hatch when you have a Request (that expects a value to be determined at runtime), but in a given instance you know the value at build-time.

## Variable Operations

`ReadVar` provides operations for transforming and combining variables:

- **`map()`**: Transform a `ReadVar<T>` into a `ReadVar<U>`
- **`zip()`**: Combine two ReadVars into `ReadVar<(T, U)>`
- **`into_side_effect()`**: Convert `ReadVar<T>` to `ReadVar<SideEffect>` when you only care about ordering, not the value
- **`depending_on()`**: Create a new ReadVar with an explicit dependency

For detailed examples, see the [`ReadVar` documentation](https://openvmm.dev/rustdoc/linux/flowey/node/prelude/struct.ReadVar.html).
