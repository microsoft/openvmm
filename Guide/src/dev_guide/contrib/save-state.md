# Save State

OpenHCL supports the mechanism of saving & restoring state. This primitive can
be used for various VM operations, but a key use case is for updating OpenHCL at
runtime (a.k.a. "Servicing"). This save state can be stored in memory or on
durable media, read back at a later time.

## Save State First Principles

Here are the principles you must maintain when adding new save & restore code:

1. **Save & Restore is Forward & Backwards Compatible**: A newer version of
   OpenHCL must understand save state from a prior version, and an older version
   must not crash when reading save state from a newer version.
2. **Do not break save state after that state is in use**: Save state must be
   compatible from *any* commit to any other commit, once the product has
   shipped and started using that save state.[^1]
3. **All Save State is Protocol Buffers**: All save state is encoded as
   `ProtoBuf`, using `mesh`.

## Conventions

1. **Put save state in it's own module**: This makes PR reviews easier, to catch
   any mistakes updating save state.
2. **Create a unique package per crate**: A logical grouping of saved state
   should have the same `package`
3. **Avoid unsupported types, when possible**: These types don't support the
   default values needed for safely extending save state:
    * Arrays: if you need to add an array, consider a `vec` or `Option<[T; N]>`
      instead.
    * Enum: if you need to add an enum, add it as `Option<MyEnum>' instead.

## Updating (Extending) Saved State

Since saved state is just Protocol Buffers, use the [guide to updating Protocol
Buffers messages](https://protobuf.dev/programming-guides/proto3/#updating) as a
starting point, with the following caveats:

1. OpenVMM uses `sint32` to represent the `i32` native type. Therefore, changing
   `i32` to `u32` is a breaking change, for example.
2. The Protocol Buffers docs mention what happens for newly added fields, but it
   bears adding some nuance here:
    1. `arrays` and `enums` are **not supported**. Reading new save state with
       either will fail on an older build.
    2. Old -> New Save State: Save state from a prior revision will not contain
       some newly added fields. Those fields will get the [default
       values](https://protobuf.dev/programming-guides/proto3/#default). This is
       how that breaks down for the rust types:
        * `Option<T>` => `None`
        * Structs => each field gets that field's default value
        * Vecs => empty vec
        * Numbers => 0
        * Strings => `""`
    3. New -> Old Save State: Unknown fields are ignored.

## Defining Saved State

Saved state is defined as a `struct` that has `#[derive(Protobuf)]` and
`#[mesh(package = "package_name")]` attributes. Here is an example, taken from
the `nvme_driver`:

```rust
pub mod save_restore {
    use super::*;

    /// Save/restore state for IoQueue.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "nvme_driver")]
    pub struct IoQueueSavedState {
        #[mesh(1)]
        /// Which CPU handles requests.
        pub cpu: u32,
        #[mesh(2)]
        /// Interrupt vector (MSI-X)
        pub iv: u32,
        #[mesh(3)]
        pub queue_data: QueuePairSavedState,
    }
}
```

[^1]: Saved state is in use when it reaches a release branch that is in tell
    mode. See [release management](./release.md) for details.
