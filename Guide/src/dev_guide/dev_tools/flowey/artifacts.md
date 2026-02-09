# Artifacts

Artifacts enable typed data transfer between jobs with automatic dependency management, abstracting away CI system complexities like name collisions and manual job ordering.

## Typed vs Untyped Artifacts

**Typed artifacts (recommended)** provide type-safe artifact handling by defining
a custom type that implements the `Artifact` trait:

```rust
#[derive(Serialize, Deserialize)]
struct MyArtifact {
    #[serde(rename = "output.bin")]
    binary: PathBuf,
    #[serde(rename = "metadata.json")]
    metadata: PathBuf,
}

impl Artifact for MyArtifact {}

let (pub_artifact, use_artifact) = pipeline.new_typed_artifact("my-files");
```

**Untyped artifacts** provide simple directory-based artifacts for simpler cases:

```rust
let (pub_artifact, use_artifact) = pipeline.new_artifact("my-files");
```

For detailed examples of defining and using artifacts, see the [Artifact trait documentation](https://openvmm.dev/rustdoc/linux/flowey_core/pipeline/trait.Artifact.html).

Both `pipeline.new_typed_artifact("name")` and `pipeline.new_artifact("name")` return a tuple of handles: `(pub_artifact, use_artifact)`. When defining a job you convert them with the job context:

```rust
// In a producing job:
let artifact_out = ctx.publish_artifact(pub_artifact);
// artifact_out : WriteVar<MyArtifact>   (typed)
// or WriteVar<PathBuf> for untyped

// In a consuming job:
let artifact_in = ctx.use_artifact(use_artifact);
// artifact_in : ReadVar<MyArtifact>     (typed)
// or ReadVar<PathBuf> for untyped
```

After conversion, you treat the returned `WriteVar` / `ReadVar` like any other flowey variable (claim them in steps, write/read values).
Key concepts:

- The `Artifact` trait works by serializing your type to JSON in a format that reflects a directory structure
- Use `#[serde(rename = "file.exe")]` to specify exact file names
- Typed artifacts ensure compile-time type safety when passing data between jobs
- Untyped artifacts are simpler but don't provide type guarantees
- Tuple handles must be lifted with `ctx.publish_artifact(...)` / `ctx.use_artifact(...)` to become flowey variables

## How Flowey Manages Artifacts Under the Hood

During the **pipeline resolution phase** (build-time), flowey:

1. **Identifies artifact producers and consumers** by analyzing which jobs write to vs read from each artifact's `WriteVar`/`ReadVar`
2. **Constructs the job dependency graph** ensuring producers run before consumers
3. **Generates backend-specific upload/download steps** in the appropriate places:
   - For ADO: Uses `PublishPipelineArtifact` and `DownloadPipelineArtifact` tasks
   - For GitHub Actions: Uses `actions/upload-artifact` and `actions/download-artifact`
   - For local execution: Uses filesystem copying

At **runtime**, the artifact `ReadVar<PathBuf>` and `WriteVar<PathBuf>` work just like any other flowey variable:

- Producing jobs write artifact files to the path from `WriteVar<PathBuf>`
- Flowey automatically uploads those files as an artifact
- Consuming jobs read the path from `ReadVar<PathBuf>` where flowey has downloaded the artifact
