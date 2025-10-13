// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use heck::ToSnakeCase;
use proc_macro2::Ident;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use quote::ToTokens;
use quote::quote;
use quote::quote_spanned;
use syn::Attribute;
use syn::DataEnum;
use syn::DataStruct;
use syn::DeriveInput;
use syn::LitStr;
use syn::Token;
use syn::Type;
use syn::WherePredicate;
use syn::ext::IdentExt;
use syn::parse::Parse;
use syn::parse::ParseStream;
use syn::parse_macro_input;
use syn::parse_quote;
use syn::parse_quote_spanned;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::token::Comma;

// Documented in the inspect crate.
#[proc_macro_derive(Inspect, attributes(inspect))]
pub fn derive_inspect(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input, false)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

// Documented in the inspect crate.
#[proc_macro_derive(InspectMut, attributes(inspect))]
pub fn derive_inspect_mut(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input, true)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn derive(input: &DeriveInput, mutable: bool) -> syn::Result<TokenStream> {
    match &input.data {
        syn::Data::Struct(data) => derive_struct(input, data, mutable),
        syn::Data::Enum(data) => derive_enum(input, data, mutable),
        _ => Err(syn::Error::new(
            Span::call_site(),
            "unions not supported for Inspect",
        )),
    }
}

enum StructAttr {
    Transparent(Vec<Attr<FieldAttr>>),
    Skip,
    With(syn::Expr),
    Extra(syn::Expr),
    Bound(Punctuated<WherePredicate, Token![,]>),
    Hex,
}

#[derive(Clone)]
enum FieldAttr {
    Rename(String),
    Flatten,
    Skip,
    Mut,
    With(syn::Expr),
    Safe,
    Sensitive,
    Hex,
    Binary,
}

enum SensitivityLevel {
    Safe,
    Sensitive,
}

enum EnumAttr {
    Skip,
    With(syn::Expr),
    ExternalTag,
    Untagged,
    Tag(LitStr),
    Extra(syn::Expr),
    Bound(Punctuated<WherePredicate, Token![,]>),
    Hex,
}

enum VariantAttr {
    Rename(String),
    Transparent(Vec<Attr<FieldAttr>>),
}

#[derive(Clone)]
struct Attr<T> {
    kind: T,
    span: Span,
}

impl<T: Parse> Parse for Attr<T> {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let span = input.span();
        Ok(Self {
            kind: T::parse(input)?,
            span,
        })
    }
}

fn parse_string_attr(input: ParseStream<'_>) -> syn::Result<LitStr> {
    let _: syn::token::Eq = input.parse()?;
    input.parse()
}

fn parse_wrapped_attr<T: Parse>(input: ParseStream<'_>) -> syn::Result<T> {
    let _: syn::token::Eq = input.parse()?;
    let lit: LitStr = input.parse()?;
    lit.parse()
}

fn parse_wrapped_attr_with<T>(
    input: ParseStream<'_>,
    parser: impl FnOnce(ParseStream<'_>) -> syn::Result<T>,
) -> syn::Result<T> {
    let _: syn::token::Eq = input.parse()?;
    let lit: LitStr = input.parse()?;
    lit.parse_with(parser)
}

impl Parse for StructAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ident = Ident::parse_any(input)?;
        let kind = if ident == "skip" {
            Self::Skip
        } else if ident == "transparent" {
            let field_attr = if input.peek(syn::token::Paren) {
                let content;
                syn::parenthesized!(content in input);
                parse_attr_list(&content)?.into_iter().collect()
            } else {
                Vec::new()
            };
            Self::Transparent(field_attr)
        } else if ident == "with" {
            Self::With(parse_wrapped_attr(input)?)
        } else if ident == "display" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsDisplay))
        } else if ident == "debug" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsDebug))
        } else if ident == "extra" {
            Self::Extra(parse_wrapped_attr(input)?)
        } else if ident == "bound" {
            Self::Bound(parse_wrapped_attr_with(
                input,
                Punctuated::parse_terminated,
            )?)
        } else if ident == "hex" {
            Self::Hex
        } else {
            return Err(syn::Error::new(
                ident.span(),
                format_args!("unknown attribute `{ident}`"),
            ));
        };
        Ok(kind)
    }
}

impl Parse for FieldAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ident = Ident::parse_any(input)?;
        let kind = if ident == "rename" {
            Self::Rename(parse_string_attr(input)?.value())
        } else if ident == "format" {
            let format = parse_string_attr(input)?;
            Self::With(parse_quote!(|x| ::inspect::adhoc(
                move |req| req.value(::core::format_args!(#format, x))
            )))
        } else if ident == "display" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsDisplay))
        } else if ident == "debug" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsDebug))
        } else if ident == "hex" {
            Self::Hex
        } else if ident == "binary" {
            Self::Binary
        } else if ident == "bytes" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsBytes))
        } else if ident == "iter_by_key" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::iter_by_key))
        } else if ident == "iter_by_index" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::iter_by_index))
        } else if ident == "flatten" {
            Self::Flatten
        } else if ident == "skip" {
            Self::Skip
        } else if ident == "mut" {
            Self::Mut
        } else if ident == "safe" {
            Self::Safe
        } else if ident == "sensitive" {
            Self::Sensitive
        } else if ident == "with" {
            Self::With(parse_wrapped_attr(input)?)
        } else if ident == "send" {
            let map: syn::Expr = parse_wrapped_attr(input)?;
            Self::With(parse_quote!(|x| ::inspect::send(x, #map)))
        } else {
            return Err(syn::Error::new(
                ident.span(),
                format_args!("unknown attribute `{ident}`"),
            ));
        };
        Ok(kind)
    }
}

impl Parse for EnumAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ident = Ident::parse_any(input)?;
        let kind = if ident == "skip" {
            Self::Skip
        } else if ident == "with" {
            Self::With(parse_wrapped_attr(input)?)
        } else if ident == "external_tag" {
            Self::ExternalTag
        } else if ident == "tag" {
            Self::Tag(parse_string_attr(input)?)
        } else if ident == "untagged" {
            Self::Untagged
        } else if ident == "display" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsDisplay))
        } else if ident == "debug" {
            Self::With(parse_quote_spanned!(ident.span()=> ::inspect::AsDebug))
        } else if ident == "extra" {
            Self::Extra(parse_wrapped_attr(input)?)
        } else if ident == "bound" {
            Self::Bound(parse_wrapped_attr_with(
                input,
                Punctuated::parse_terminated,
            )?)
        } else if ident == "hex" {
            Self::Hex
        } else {
            return Err(syn::Error::new(
                ident.span(),
                format_args!("unknown attribute `{ident}`"),
            ));
        };
        Ok(kind)
    }
}

impl Parse for VariantAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ident = Ident::parse_any(input)?;
        let kind = if ident == "rename" {
            Self::Rename(parse_string_attr(input)?.value())
        } else if ident == "transparent" {
            let field_attr = if input.peek(syn::token::Paren) {
                let content;
                syn::parenthesized!(content in input);
                parse_attr_list(&content)?.into_iter().collect()
            } else {
                Vec::new()
            };
            Self::Transparent(field_attr)
        } else {
            return Err(syn::Error::new(
                ident.span(),
                format_args!("unknown attribute `{ident}`"),
            ));
        };
        Ok(kind)
    }
}

fn parse_attr_list<T: Parse>(input: ParseStream<'_>) -> syn::Result<Punctuated<Attr<T>, Comma>> {
    Punctuated::parse_terminated(input)
}

fn parse_attrs<T: Parse>(attrs: &[Attribute]) -> syn::Result<Vec<Attr<T>>> {
    let mut idents = Vec::new();
    for attr in attrs {
        if attr.path().get_ident().is_none_or(|x| x != "inspect") {
            continue;
        }
        let attrs = attr.parse_args_with(parse_attr_list)?;
        idents.extend(attrs);
    }
    Ok(idents)
}

/// Parses a `bitfield(u32)` style attribute, returning the bitfield type.
fn parse_bitfield_attr(attrs: &[Attribute]) -> syn::Result<Option<Ident>> {
    for attr in attrs {
        if attr.path().get_ident().is_some_and(|x| x == "bitfield") {
            return Ok(Some(attr.parse_args()?));
        }
    }
    Ok(None)
}

fn derive_struct(
    input: &DeriveInput,
    data: &DataStruct,
    mutable: bool,
) -> syn::Result<TokenStream> {
    let mut skip_struct = None;
    let mut transparent = None;
    let mut struct_with = None;
    let mut extra = None;
    let mut bound = None;
    let mut hex = false;
    let bitfield = parse_bitfield_attr(&input.attrs)?;
    for attr in parse_attrs(&input.attrs)? {
        match attr.kind {
            StructAttr::Skip => {
                insert_or_fail(&mut skip_struct, attr.span, attr.span)?;
            }
            StructAttr::Transparent(inner) => {
                insert_or_fail(&mut transparent, attr.span, (attr.span, inner))?;
            }
            StructAttr::With(with) => insert_or_fail(&mut struct_with, attr.span, with)?,
            StructAttr::Extra(x) => insert_or_fail(&mut extra, attr.span, x)?,
            StructAttr::Bound(x) => insert_or_fail(&mut bound, attr.span, x)?,
            StructAttr::Hex => hex = true,
        }
    }

    let req = Ident::new("req", Span::call_site());
    let respond = if let Some(span) = skip_struct {
        if struct_with.is_some() || bitfield.is_some() || transparent.is_some() || extra.is_some() {
            return Err(syn::Error::new(span, "incompatible attributes"));
        }
        for field in &data.fields {
            if !parse_attrs::<FieldAttr>(&field.attrs)?.is_empty() {
                return Err(syn::Error::new_spanned(
                    field,
                    "attributes not allowed on fields of skipped types",
                ));
            }
        }
        quote! {
            #req.ignore();
        }
    } else if let Some(with) = struct_with {
        if bitfield.is_some() || transparent.is_some() || extra.is_some() {
            return Err(syn::Error::new_spanned(with, "incompatible attributes"));
        }
        if mutable {
            quote! {
                ::inspect::InspectMut::inspect_mut(&mut #with(self), #req);
            }
        } else {
            quote! {
                ::inspect::Inspect::inspect(&#with(self), #req);
            }
        }
    } else {
        if let Some((span, _)) = transparent {
            if bitfield.is_some() || extra.is_some() {
                return Err(syn::Error::new(span, "incompatible attributes"));
            }
        }
        fields_response(&data.fields, &[], &req, bitfield, transparent, extra)?
    };

    impl_defs(input, mutable, bound, &req, &respond, hex)
}

fn impl_defs(
    input: &DeriveInput,
    mutable: bool,
    bound: Option<Punctuated<WherePredicate, Token![,]>>,
    req: &Ident,
    respond: &TokenStream,
    hex: bool,
) -> Result<TokenStream, syn::Error> {
    let mut generics = input.generics.clone();
    if let Some(bound) = bound {
        generics.make_where_clause().predicates.extend(bound);
    }

    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();

    let fmt = if hex {
        quote!(let #req = #req.with_hex_format();)
    } else {
        quote!()
    };

    let type_name = &input.ident;
    let tokens = if mutable {
        quote! {
            impl #impl_generics ::inspect::InspectMut for #type_name #type_generics #where_clause {
                fn inspect_mut(&mut self, #req: ::inspect::Request<'_>) {
                    #fmt
                    #respond
                }
            }
        }
    } else {
        quote! {
            impl #impl_generics ::inspect::Inspect for #type_name #type_generics #where_clause {
                fn inspect(&self, #req: ::inspect::Request<'_>) {
                    #fmt
                    #respond
                }
            }
        }
    };
    Ok(tokens)
}

fn fields_response(
    fields: &syn::Fields,
    field_vars: &[(TokenStream, String)],
    req: &Ident,
    bitfield: Option<Ident>,
    transparent: Option<(Span, Vec<Attr<FieldAttr>>)>,
    extra: Option<syn::Expr>,
) -> Result<TokenStream, syn::Error> {
    let resp = Ident::new("resp", Span::call_site());
    let field_vars =
        field_vars
            .iter()
            .map(|x| Some(x.clone()).unzip())
            .chain(fields.iter().enumerate().map(|(field_index, field)| {
                // Bitfield fields starting with '_' don't have accessor methods, so skip them.
                let skip = bitfield.is_some()
                    && field
                        .ident
                        .as_ref()
                        .is_some_and(|id| id.to_string().starts_with('_'));

                (!skip)
                    .then(|| {
                        let ident = field.ident.as_ref().map_or_else(
                            || syn::Index::from(field_index).into_token_stream(),
                            |ident| ident.into_token_stream(),
                        );
                        let field_ident = if bitfield.is_some() {
                            quote!(self.#ident())
                        } else {
                            quote!(self.#ident)
                        };
                        let inspect_name = ident.to_string();
                        (field_ident, inspect_name)
                    })
                    .unzip()
            }));

    let mut fields = fields
        .iter()
        .zip(field_vars)
        .map(|(field, (ident, name))| {
            field_response(
                field,
                ident,
                name,
                transparent.as_ref().map(|(_, attr)| attr.as_slice()),
                req,
                &resp,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(bitfield_ty) = bitfield {
        fields.push(Some(quote! {
            #resp.field("raw", ::inspect::AsHex(#bitfield_ty::from(*self)));
        }));
    }
    if let Some((transparent, _)) = &transparent {
        if fields.iter().filter(|x| x.is_some()).count() != 1 {
            return Err(syn::Error::new(
                *transparent,
                "there must be exactly one non-skipped field",
            ));
        }
    }
    let respond = if transparent.is_some() {
        assert!(extra.is_none());
        quote!(#(#fields)*)
    } else {
        let extra = extra.map(|extra| {
            quote_spanned! {extra.span()=>
                #extra(self, &mut #resp);
            }
        });

        quote! {
            let mut #resp = #req.respond();
            #extra
            #(#fields)*
        }
    };
    Ok(respond)
}

fn field_response(
    field: &syn::Field,
    ident: Option<TokenStream>,
    mut inspect_name: Option<String>,
    transparent_attrs: Option<&[Attr<FieldAttr>]>,
    req: &Ident,
    resp: &Ident,
) -> Result<Option<TokenStream>, syn::Error> {
    let mut attrs = parse_attrs(&field.attrs)?;

    // Automatically skip fields with a type of PhantomData.
    let auto_skip = if ident.is_none() {
        true
    } else if let Type::Path(ty) = &field.ty {
        if let Some(seg) = ty.path.segments.last() {
            seg.ident == "PhantomData"
        } else {
            false
        }
    } else {
        false
    };

    if let Some(inner) = transparent_attrs {
        if !auto_skip && !attrs.iter().any(|x| matches!(x.kind, FieldAttr::Skip)) {
            attrs.extend(inner.iter().cloned());
        }
    }

    #[derive(PartialEq)]
    enum Kind {
        Field,
        Flatten,
        Skip,
        Transparent,
    }

    #[derive(PartialEq, Eq)]
    enum Format {
        Default,
        Hex,
        Binary,
    }

    let mut sensitivity = None;
    let mut is_mut = false;
    let mut kind = None;
    let mut with = None;
    let mut format = Format::Default;
    for attr in attrs {
        let mut new_with = None;
        let mut new_sen = None;
        let new_kind = match attr.kind {
            FieldAttr::Flatten => Some(Kind::Flatten),
            FieldAttr::Skip => Some(Kind::Skip),
            FieldAttr::Rename(name) => {
                inspect_name = Some(name);
                None
            }
            FieldAttr::Mut => {
                is_mut = true;
                None
            }
            FieldAttr::With(ty) => {
                new_with = Some(ty);
                None
            }
            FieldAttr::Safe => {
                new_sen = Some(SensitivityLevel::Safe);
                None
            }
            FieldAttr::Sensitive => {
                new_sen = Some(SensitivityLevel::Sensitive);
                None
            }
            FieldAttr::Hex => {
                if format != Format::Default {
                    return Err(syn::Error::new(attr.span, "too many field formats"));
                }
                format = Format::Hex;
                None
            }
            FieldAttr::Binary => {
                if format != Format::Default {
                    return Err(syn::Error::new(attr.span, "too many field formats"));
                }
                format = Format::Binary;
                None
            }
        };
        if let Some(new_kind) = new_kind {
            if kind.is_some() {
                return Err(syn::Error::new(attr.span, "too many field types"));
            }
            kind = Some(new_kind);
        }
        if let Some(new_with) = new_with {
            if with.is_some() {
                return Err(syn::Error::new(attr.span, "too many with attributes"));
            }
            with = Some(new_with);
        }
        if let Some(new_sen) = new_sen {
            if sensitivity.is_some() {
                return Err(syn::Error::new(
                    attr.span,
                    "too many sensitivity attributes",
                ));
            }
            sensitivity = Some(new_sen);
        }
    }

    let kind = if let Some(kind) = kind {
        kind
    } else if auto_skip {
        Kind::Skip
    } else if transparent_attrs.is_some() {
        Kind::Transparent
    } else {
        Kind::Field
    };

    if transparent_attrs.is_some() && !matches!(kind, Kind::Transparent | Kind::Skip) {
        return Err(syn::Error::new(
            field.span(),
            "invalid attributes for field in transparent struct",
        ));
    }

    if is_mut {
        match kind {
            Kind::Field | Kind::Flatten | Kind::Transparent => {}
            _ => {
                return Err(syn::Error::new(
                    field.span(),
                    "type attribute incompatible with `mut` attribute",
                ));
            }
        }
    }

    let ref_ty = if is_mut { quote!(&mut) } else { quote!(&) };

    let mut field_ref = quote!(#ref_ty #ident);
    if let Some(with) = with {
        let allowed = match &kind {
            Kind::Field | Kind::Transparent | Kind::Flatten => true,
            Kind::Skip => false,
        };
        if !allowed {
            return Err(syn::Error::new(
                field.span(),
                "type attribute incompatible with `kind` attribute",
            ));
        }
        // If `with` is a closure, use the `call` helper to force
        // inference on the type of the closure's parameter. Don't
        // do this for non-closures because instead we want to infer
        // the type of the input (which might be coerced, e.g. from
        // &[u32; 6] to &[u32]).
        field_ref = if matches!(with, syn::Expr::Closure(_)) {
            quote!(#ref_ty ::inspect::derive_helpers::call(#field_ref, #with))
        } else {
            quote!(#ref_ty (#with)(#field_ref))
        }
    }

    match format {
        Format::Hex => {
            field_ref = quote!(#ref_ty ::inspect::AsHex(#field_ref));
        }
        Format::Binary => {
            field_ref = quote!(#ref_ty ::inspect::AsBinary(#field_ref));
        }
        Format::Default => {}
    }

    let tokens = match kind {
        Kind::Field => {
            let name = inspect_name.unwrap();
            let func = if is_mut {
                "sensitivity_field_mut"
            } else {
                "sensitivity_field"
            };
            let func = Ident::new(func, field.span());
            let sensitivity_level = match sensitivity {
                Some(SensitivityLevel::Safe) => quote!(::inspect::SensitivityLevel::Safe),
                Some(SensitivityLevel::Sensitive) => quote!(::inspect::SensitivityLevel::Sensitive),
                None => quote!(::inspect::SensitivityLevel::Unspecified),
            };
            quote_spanned! {field.span()=>
                #resp.#func(#name, #sensitivity_level, #field_ref);
            }
        }
        Kind::Flatten => quote_spanned! {field.span()=>
            #resp.merge(#field_ref);
        },
        Kind::Skip => return Ok(None),
        Kind::Transparent => {
            let tr = if is_mut { "InspectMut" } else { "Inspect" };
            let tr = Ident::new(tr, field.span());
            let func = if is_mut { "inspect_mut" } else { "inspect" };
            let func = Ident::new(func, field.span());
            quote_spanned! {field.span()=>
                ::inspect::#tr::#func(#field_ref, #req);
            }
        }
    };

    Ok(Some(tokens))
}

fn insert_or_fail<T>(opt: &mut Option<T>, span: Span, val: T) -> syn::Result<()> {
    if opt.is_some() {
        return Err(syn::Error::new(span, "duplicate attribute"));
    }
    *opt = Some(val);
    Ok(())
}

fn derive_enum(input: &DeriveInput, data: &DataEnum, mutable: bool) -> syn::Result<TokenStream> {
    let mut skip_struct = None;
    let mut struct_with = None;
    let mut tag = None;
    let mut extra = None;
    let mut bound = None;
    let mut hex = false;
    for attr in parse_attrs(&input.attrs)? {
        match attr.kind {
            EnumAttr::Skip => {
                insert_or_fail(&mut skip_struct, attr.span, attr.span)?;
            }
            EnumAttr::With(with) => insert_or_fail(&mut struct_with, attr.span, with)?,
            EnumAttr::Tag(v) => insert_or_fail(&mut tag, attr.span, TagMode::InternallyTagged(v))?,
            EnumAttr::ExternalTag => {
                insert_or_fail(&mut tag, attr.span, TagMode::ExternallyTagged)?
            }
            EnumAttr::Untagged => insert_or_fail(&mut tag, attr.span, TagMode::Untagged)?,
            EnumAttr::Extra(x) => insert_or_fail(&mut extra, attr.span, x)?,
            EnumAttr::Bound(x) => insert_or_fail(&mut bound, attr.span, x)?,
            EnumAttr::Hex => hex = true,
        }
    }

    let tag = tag.unwrap_or(TagMode::Unit);

    let req = Ident::new("req", Span::call_site());
    let respond = if let Some(span) = skip_struct {
        if struct_with.is_some() || !matches!(tag, TagMode::Unit) || extra.is_some() {
            return Err(syn::Error::new(span, "incompatible attributes"));
        }
        for variant in &data.variants {
            if !parse_attrs::<VariantAttr>(&variant.attrs)?.is_empty() {
                return Err(syn::Error::new_spanned(
                    variant,
                    "attributes not allowed on variants of skipped types",
                ));
            }
            for field in &variant.fields {
                if !parse_attrs::<FieldAttr>(&field.attrs)?.is_empty() {
                    return Err(syn::Error::new_spanned(
                        field,
                        "attributes not allowed on fields of skipped types",
                    ));
                }
            }
        }
        quote!(#req.ignore())
    } else if data.variants.is_empty() {
        quote! {
            let _ = #req;
            unreachable!()
        }
    } else {
        match tag {
            TagMode::Unit => {
                if let Some(extra) = extra {
                    return Err(syn::Error::new_spanned(
                        extra,
                        "`extra` not allowed on unit-only enums",
                    ));
                }
                derive_unit_only_enum(data, mutable, &req)?
            }
            TagMode::Untagged | TagMode::ExternallyTagged | TagMode::InternallyTagged(_) => {
                derive_tagged_enum(data, &req, tag, extra)?
            }
        }
    };

    impl_defs(input, mutable, bound, &req, &respond, hex)
}

enum TagMode {
    Unit,
    Untagged,
    ExternallyTagged,
    InternallyTagged(LitStr),
}

fn derive_tagged_enum(
    data: &DataEnum,
    req: &Ident,
    tag: TagMode,
    extra: Option<syn::Expr>,
) -> syn::Result<TokenStream> {
    let resp = Ident::new("resp", Span::call_site());

    let fields = data
        .variants
        .iter()
        .map(|variant| {
            let mut inspect_name = None;
            let mut transparent = None;
            for attr in parse_attrs(&variant.attrs)? {
                match attr.kind {
                    VariantAttr::Rename(name) => inspect_name = Some(name),
                    VariantAttr::Transparent(inner) => {
                        if transparent.is_some() {
                            return Err(syn::Error::new(
                                attr.span,
                                "duplicate transparent attribute",
                            ));
                        }
                        transparent = Some((attr.span, inner));
                    }
                }
            }

            let variant_name = &variant.ident;
            let inspect_name =
                inspect_name.unwrap_or_else(|| variant_name.to_string().to_snake_case());

            let fields = variant
                .fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    field.ident.as_ref().map_or_else(
                        || syn::Index::from(i).to_token_stream(),
                        |id| id.to_token_stream(),
                    )
                })
                .collect::<Vec<_>>();

            let field_vars = (0..variant.fields.len())
                .map(|i| Ident::new(&format!("v{i}"), Span::call_site()))
                .collect::<Vec<_>>();

            // Dereference the identifiers in expressions, since
            // `fields_response` will want to apply the appropriate reference
            // type, but we already have references from destructuring the enum
            // variant.
            let dereferenced = field_vars
                .iter()
                .zip(fields.iter())
                .map(|(x, name)| (quote!((*#x)), name.to_string()))
                .collect::<Vec<_>>();

            let inner_req = Ident::new("req", Span::call_site());
            let inner_response = fields_response(
                &variant.fields,
                &dereferenced,
                &inner_req,
                None,
                transparent,
                None,
            )?;
            let inner_adhoc = quote!(&mut ::inspect::adhoc_mut(|#inner_req| { #inner_response }));

            let tokens = match &tag {
                TagMode::Unit => unreachable!(),
                TagMode::Untagged => {
                    quote! {
                        #resp.merge(#inner_adhoc);
                    }
                }
                TagMode::ExternallyTagged => {
                    quote! {
                        #resp.field_mut(#inspect_name, #inner_adhoc);
                    }
                }
                TagMode::InternallyTagged(tag) => {
                    quote! {
                        #resp.field(#tag, #inspect_name).merge(#inner_adhoc);
                    }
                }
            };
            Ok(quote! {
                Self::#variant_name { #(#fields: #field_vars,)* } => { #tokens }
            })
        })
        .collect::<Result<Vec<_>, syn::Error>>()?;

    let extra = extra.map(|extra| {
        quote_spanned! {extra.span()=>
            #extra(self, &mut #resp);
        }
    });

    let tokens = quote! {
        let mut #resp = #req.respond();
        #extra
        match self {
            #(#fields)*
        }
    };

    Ok(tokens)
}

fn derive_unit_only_enum(data: &DataEnum, mutable: bool, req: &Ident) -> syn::Result<TokenStream> {
    let fields = data
        .variants
        .iter()
        .map(|variant| {
            if !matches!(variant.fields, syn::Fields::Unit) {
                return Err(syn::Error::new_spanned(
                    variant,
                    "non-unit-only enums require `tag` or `external_tag` attributes",
                ));
            }
            let field_name = &variant.ident;
            let mut inspect_name = None;
            for attr in parse_attrs(&variant.attrs)? {
                match attr.kind {
                    VariantAttr::Rename(name) => inspect_name = Some(name),
                    VariantAttr::Transparent(_) => {}
                }
            }
            let inspect_name =
                inspect_name.unwrap_or_else(|| field_name.to_string().to_snake_case());
            Ok((field_name, inspect_name))
        })
        .collect::<Result<Vec<_>, syn::Error>>()?;

    let field_name = fields.iter().map(|x| x.0);
    let field_name2 = field_name.clone();
    let inspect_name = fields.iter().map(|x| &x.1);
    let inspect_name2 = inspect_name.clone();

    let tokens = if mutable {
        quote! {
            match #req.update() {
                Ok(req) => {
                    let v = match req.new_value() {
                        #(#inspect_name => Self::#field_name,)*
                        _ => {
                            req.fail("unknown value");
                            return;
                        }
                    };
                    *self = v;
                    let v = req.new_value().to_string();
                    req.succeed(v);
                }
                Err(req) => {
                    let name = match self {
                        #(Self::#field_name2 => #inspect_name2,)*
                    };
                    req.value(name);
                }
            }
        }
    } else {
        quote! {
            let name: &str = match self {
                #(Self::#field_name => #inspect_name,)*
            };
            #req.value(name);
        }
    };
    Ok(tokens)
}
