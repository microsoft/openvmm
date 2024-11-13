# Unit Tests

```admonish tip
{{ #include /_fragments/nextest_tip.md }}
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
