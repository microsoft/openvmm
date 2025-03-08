// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test attribute macro for `test_with_tracing` crate.

use quote::quote;
use syn::parse_macro_input;
use syn::spanned::Spanned;
use syn::Error;
use syn::ItemFn;

/// Attribute macro on tests that have tracing output.
///
/// This attribute macro acts just like the `#[test]` attribute except that it
/// first initializes the `tracing` crate.
#[proc_macro_attribute]
pub fn test(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let item = parse_macro_input!(item as ItemFn);
    make_test(item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn make_test(item: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if item.sig.asyncness.is_some() {
        return Err(Error::new(
            item.sig.fn_token.span(),
            "test function must not be async",
        ));
    }

    let name = &item.sig.ident;
    let return_type = &item.sig.output;
    if !item.sig.inputs.is_empty() {
        return Err(Error::new(item.sig.inputs.span(), "expected 0 arguments"));
    };
    let attrs = &item.attrs;

    Ok(quote! {
        #[::core::prelude::v1::test]
        #(#attrs)*
        fn #name() #return_type {
            #item
            ::test_with_tracing::init();
            #name()
        }
    })
}
