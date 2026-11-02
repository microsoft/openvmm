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

For an agent's edit/test/review loop, collect and report changed-line coverage
in one command:

```bash
cargo xflowey unit-test-coverage -p memory_range --base origin/main
```

This prints bounded JSON containing absolute coverage and changed Rust lines,
their covering tests, uncovered lines, and unmeasured files. The comparison is
from the merge base of the selected revision and HEAD to the working tree:
committed branch changes, staged/unstaged edits, and nonignored untracked Rust
files are included. Deletions do not contribute new executable lines.
Coverage gaps are advisory, not a percentage gate.

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

The command prints the path to `report.json` and emits a JSON query result.
Subsequent queries automatically use the latest local collection attempt:

```bash
python3 repo_support/unit_test_coverage.py query

python3 repo_support/unit_test_coverage.py query --changed

python3 repo_support/unit_test_coverage.py query \
    --file vm/vmcore/memory_range/src/lib.rs --start 100 --end 140
```

`query --changed` uses the recorded comparison base. Without a recorded base,
it resolves the configured `origin/HEAD`; use `--base <revision>` to override
that choice. It does not guess the default branch. `--report <path>` selects
an explicit report instead of the latest attempt. A newer failed/incomplete
collection is never silently replaced by older successful results.

Queries emit JSON with file summaries or measured lines and their covering
tests, including configuration-run names, attempts, and pass/fail results.
`--limit` bounds file/line results, changed-hunk summaries, and each line's
covering-test list; hunks list at most five covering attempts. Truncation
is explicit. Paths are relative to the repository root. Files absent from the
report are **unmeasured**, not covered. Source lines absent within a measured
file may be non-executable or compiled out. Patch queries compact those lines
into inclusive `unmapped_ranges`, with an `unmapped_lines` count; they are
outside the measured-line percentage. Uncovered measured lines are also
summarized as `uncovered_ranges`. Non-Rust changes are counted separately.
No measured changed lines means a null percentage, not 100%.

The version-2 report records HEAD plus a hash of tracked changes and nonignored
untracked content, so dirty-tree results are not mislabeled as committed PR
coverage. It also records the compiler/tool versions, exact build arguments,
test filter, and collection state. Changes during collection invalidate the
snapshot. Queries check freshness automatically: after source edits or a HEAD
change they emit a `stale_coverage` JSON error and exit with code 2, instructing
the agent to recollect. Run queries from the repository being reviewed.
`--allow-stale` permits explicit historical inspection, labels the result stale,
and cannot be combined with `--changed`.
Collection and test results are independent: `collection_state: complete`
means the selected attempts and their profiles were collected, not that tests
passed. `test_outcome` reports the suite result separately. A failed test can
produce complete coverage; a passing suite can have incomplete profiles.
The command still exits unsuccessfully on either test failures or missing
coverage data. Version-1 reports remain readable, but an old `incomplete` state
becomes an unknown collection status because it did not separate these causes.

The result directory retains the effective nextest configuration, inventories,
matching test binaries, per-attempt raw/indexed profiles, and LCOV exports.
The experimental nextest wrapper feature is enabled only in the copied
configuration; the repository configuration is not modified. Existing target
runners are composed inside the coverage wrapper. Existing nextest run-wrappers
are rejected rather than silently replaced.

Start with `review_summary` for selected-package production coverage and the
number of measured production lines without passing evidence. For `--changed`,
this summary describes the patch and also reports unmapped changes and
unmeasured files. Changed-hunk summaries associate ranges with covering tests.
Package selection remains explicit; the comparison base never selects packages.

Coverage includes repository source linked into the selected test binaries,
including unit-test code and workspace dependencies. The aggregate percentage
is not a production-only or workspace-wide metric. Separate `sources` views
partition selected-package production, dependency production, test source,
auxiliary code (examples/build scripts), and unclassified source. Classification
uses Cargo targets and Rust syntax, including `#[cfg(test)]` modules, rather
than filenames. Unresolved path-based modules, macro item bodies, ambiguous
conditional compilation, and parse failures remain unclassified; classification
issues are reported. This is not compiler-expanded semantic classification.
Historical reports without this metadata remain unclassified.

Every coverage view distinguishes all-attempt `execution` from
`passing_attempts`. A line hit only by a failed test has execution evidence but
no passing evidence. A passing retry counts as a passing attempt, not proof of
a non-flaky test or successful overall suite. Neither metric proves an assertion
checked the line's behavior.
Doctests, separately built child binaries, Petri/VM tests, and branch coverage
are not included yet. No data is uploaded or published.

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
