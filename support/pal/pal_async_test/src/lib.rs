// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async test attribute macro for `pal_async` crate.

use quote::quote;
use syn::parse_macro_input;
use syn::spanned::Spanned;
use syn::Error;
use syn::ItemFn;

/// Attribute macro on async tests.
///
/// This attribute macro acts just like the `#[test]` attribute except that it
/// works on `async` functions. It works by running the test using the
/// `pal_async::DefaultPool` executor.
///
/// Your async function can optionally take an argument to receive the pool
/// driver/spawner, of type `pal_async::DefaultDriver`.
///
/// This macro combines well with `test_with_tracing::test`. If that macro is in
/// scope via `use`, then it will be used instead of Rust's default `#[test]`
/// attribute.
#[proc_macro_attribute]
pub fn async_test(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let item = parse_macro_input!(item as ItemFn);
    make_async_test(item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn make_async_test(item: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if item.sig.asyncness.is_none() {
        return Err(Error::new(
            item.sig.fn_token.span(),
            "test function must be async",
        ));
    }

    let name = &item.sig.ident;
    let args = match item.sig.inputs.len() {
        0 => quote! {},
        1 => quote! {driver},
        _ => {
            return Err(Error::new(
                item.sig.inputs.span(),
                "expected 0 arguments or 1 argument (the Driver or Spawn impl)",
            ))
        }
    };
    let attrs = &item.attrs;

    // Unwrap the test result directly rather than passing it through to the
    // outer test  because rust does not properly associate the error
    // information with the test's stdout, making it hard to determine which
    // test failed.
    let unwrap = match &item.sig.output {
        syn::ReturnType::Default => quote!(),
        syn::ReturnType::Type(_, ty) => match ty.as_ref() {
            syn::Type::Tuple(t) if t.elems.is_empty() => quote!(),
            _ => quote!(.unwrap()),
        },
    };

    Ok(quote! {
        #[test]
        #(#attrs)*
        fn #name() {
            #item
            ::pal_async::DefaultPool::run_with(|driver| async move {
                #name(#args).await
            })
            #unwrap
        }
    })
}
