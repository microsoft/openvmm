# Unit Tests

```admonish tip
Note: We recommend using [cargo-nextest](https://nexte.st/) to run unit / VMM
tests. It is a significant improvement over the built-in `cargo test` runner,
and is the test runner we use in all our CI pipelines.

You can install it locally by running: `cargo install cargo-nextest --locked`

See the [cargo-nextest](https://nexte.st/) documentation for more info.
```

Unit tests test individual functions or components without pulling in lots of
ambient infrastructure. In Rust, these are usually written in the same file as
the product code--this ensures that the test has access to any internal methods
or state it requires, and it makes it easier to ensure that tests and code are
updated at the same time.

A typical module with unit tests might look something like this:

```rust
fn add_5(n: u32) -> u32 {
    n + 5
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_add_5() {
        assert_eq!(add_5(3), 8);
    }
}
```

In the OpenVMM repo, all the unit tests are run on every pull request, on an
arbitrary build machine. As a result of this approach, it's important that unit
tests:

- run quickly
- do not affect the state of the machine that runs them
- do not take a dependency on machine configuration
  - e.g: no root/administrator access or virtualization requirement

We may loosen these guidelines over time if it becomes necessary. You can also
mark tests with `#[ignore]` if they do not meet these guidelines but are useful
for manual testing.

See the [unit testing section](https://doc.rust-lang.org/rust-by-example/testing/unit_testing.html)
in "Rust by example" for more details.

## Local coverage

On x64 Linux, collect LLVM line coverage with per-test attribution using:

```bash
cargo install cargo-llvm-cov --locked --version 0.8.5
rustup component add llvm-tools-preview
cargo xflowey unit-test-coverage -p memory_range -p headervec
```

Python 3.9 or newer is required. Flowey uses the active Rust toolchain, its
normal build-dependency setup, CI unit-test package exclusions, and CI feature
selections. Missing build dependencies can be installed with
`--install-missing-deps`. If selecting `crypto`, its separate feature/backend
runs are included too.

Use `-E 'test(test_align)'` to restrict the nextest test selection, or
`--output target/my-coverage` to choose a **new** result directory. Otherwise a
unique directory is created under `target/unit-coverage/`. The command never
reuses old execution counts. Instrumented builds use a separate reusable cache
under `target/unit-coverage-build/`; normal builds are unchanged.

The command prints the path to `report.json`. Query that report without parsing
logs or downloading HTML:

```bash
python3 repo_support/unit_test_coverage.py query \
    --report target/my-coverage/report.json

python3 repo_support/unit_test_coverage.py query \
    --report target/my-coverage/report.json \
    --file vm/vmcore/memory_range/src/lib.rs --start 100 --end 140
```

Queries emit JSON with file summaries or measured lines and their covering
tests, including configuration-run names, attempts, and pass/fail results.
`--limit` bounds file/line results and each line's covering-test list; truncation
is explicit. Paths are relative to the repository root. Files absent from the
report are **unmeasured**, not covered. Source lines absent within a measured
file may be non-executable or compiled out.

The version-1 report records HEAD plus a hash of tracked changes and nonignored
untracked content, so dirty-tree results are not mislabeled as committed PR
coverage. It also records the compiler/tool versions, exact build arguments,
test filter, and collection state. Changes during collection invalidate the
snapshot. A report remains evidence for its recorded snapshot, not subsequent
edits. Keep failed or incomplete reports distinct from successful collections;
the command exits unsuccessfully on test failures or missing coverage data.

The result directory retains the effective nextest configuration, inventories,
matching test binaries, per-attempt raw/indexed profiles, and LCOV exports.
The experimental nextest wrapper feature is enabled only in the copied
configuration; the repository configuration is not modified. Existing target
runners are composed inside the coverage wrapper. Existing nextest run-wrappers
are rejected rather than silently replaced.

Coverage includes repository source linked into the selected test binaries,
including unit-test code and workspace dependencies. Percentages are not a
production-only or workspace-wide metric. Counts include failed attempts;
per-line test outcomes let agents distinguish those from passing-test evidence.
Doctests, separately built child binaries, Petri/VM tests, branch coverage, and
PR diff reporting are not included yet. No data is uploaded or published.

The collector/reader regression tests run without coverage-tool installation:

```bash
python3 -m unittest discover -s repo_support -p 'test_unit_test_coverage.py'
```

## Doc tests

Rust has another type of unit tests known as doc tests. These are unit tests
that are written in the API documentation comments of public functions. They
will be run automatically along with the unit tests, so the same guidelines
apply.

When do you choose a doc test over a unit test?

Doc tests can only access public functionality, and they are intended to
document the usage of a function or method, not to exhaustively check every
case. So write doc tests primarily as examples for other developers, and rely on
unit tests for your main coverage.

An example might look like this:

```rust
/// Adds 5 to `n`.
///
/// ```
/// assert_eq!(mycrate::add_5(3), 8);
/// ```
pub fn add_5(n: u32) -> u32 {
    n + 5
}
```

See the [documentation testing
section](https://doc.rust-lang.org/rust-by-example/testing/doc_testing.html) in
Rust by example for more info.
