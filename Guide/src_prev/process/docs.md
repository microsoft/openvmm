# Documentation

OpenVMM maintains two classes of documentation:

- The Guide (available online [here](https://aka.ms/openvmmguide/))
- `rustdoc`-generated API documentation (not currently published online)

Both the Guide and the API docs live in the same OpenVMM repo.

## Guide

The Guide (which you are currently reading) contains information on how to
build, use, test, and contribute to the OpenVMM project.

### Building locally

The guide's pages are formatted using Markdown and are generated into HTML using
[mdbook](https://github.com/rust-lang/mdBook).

You can generally make changes just by modifying the Markdown files and
previewing the result in your editor (Visual Studio Code has good support for
this).

If you want a full rendered copy of the guide locally, you can build one by
installing mdbook:

```
cargo install mdbook
```

Then from the root directory of the repo, running:

```
mdbook build --open Guide
```

If the docs are built in WSL, you may need to find the HTML files in file
explorer. Open the appropriate `\\wsl.localhost` share and navigate to the
repo root. Then open `Guide\book\index.html` in your web browser.

## API Documentation

API documentation takes the form of Rust doc-comments at the module, struct, and
function level.

Note that API documentation should also contain brief examples when possible.
These examples are automatically run as unit tests during the CI build process.

You can learn more about Rust's documentation comment support [in the Rust
Book](https://doc.rust-lang.org/book/ch14-02-publishing-to-crates-io.html#making-useful-documentation-comments).

To build the API docs for the entire OpenVMM project locally (omitting docs for
third-party dependencies), run:

```
cargo doc \
    --workspace \
    --no-deps
```

If the docs are built in WSL, you may need to find the HTML files in file
explorer. Open the appropriate `\\wsl.localhost` share and navigate to the
repo root. Then open `target\doc\openvmm\index.html` in your web browser.
