// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Derive macro for `mesh::MeshPayload` and `mesh_protobuf::Protobuf`.

use heck::ToSnakeCase;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use quote::format_ident;
use quote::quote;
use quote::quote_spanned;
use quote::ToTokens;
use std::collections::BTreeSet;
use syn::ext::IdentExt;
use syn::parse::Parse;
use syn::parse::ParseStream;
use syn::parse_macro_input;
use syn::parse_quote;
use syn::parse_quote_spanned;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::Attribute;
use syn::DataEnum;
use syn::DataStruct;
use syn::DeriveInput;
use syn::Fields;
use syn::GenericParam;
use syn::Generics;
use syn::Ident;
use syn::ImplGenerics;
use syn::Lifetime;
use syn::LifetimeParam;
use syn::LitInt;
use syn::LitStr;
use syn::Path;
use syn::Token;
use syn::TypePath;
use syn::WherePredicate;

/// The derive macro for `MeshPayload`.
///
/// This allows you to automatically generate serialization and deserialization
/// code for sending objects via a mesh channel.
///
/// `MeshPayload` can be derived for any struct or enum where all the fields'
/// types implement `MeshPayload`.
///
/// Note that currently there is no way to specify the field numbers used in the
/// message encoding, and there is no way to mark a field as non-optional. This
/// means that inserting or removing a field in a struct will break any existing
/// binaries using that struct. Similar problems exist with updating an enum.
/// For now, the best advice is to not use this encoding for messages that must
/// be durable.
///
/// # Attributes
///
/// The `#[mesh]` attribute can be used to customize the generated code:
///
/// **`#[mesh(bound = "T: MyTrait")]`** allows replacing the default trait
/// bounds with a custom where clause. That is, normally the type:
///
/// ```text
/// #[derive(MeshPayload)]
/// struct Foo<T: Trait>(T::Bar);
/// ```
///
/// would have a `T: MeshPayload` constraint on the generated `MeshPayload`
/// impl. However, by adding `#[mesh(bound = "T: MyTrait")]`, this can be
/// replaced with the specified bound (which can be empty).
#[proc_macro_derive(MeshPayload, attributes(mesh))]
pub fn derive_mesh_message(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input, "::mesh::payload", Some("mesh::resource::Resource"))
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

#[proc_macro_derive(MeshProtobuf, attributes(mesh))]
pub fn derive_protobuf_via_mesh(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input, "::mesh::payload", None)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

#[proc_macro_derive(Protobuf, attributes(mesh))]
pub fn derive_protobuf(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input, "::mesh_protobuf", None)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn derive(
    input: &DeriveInput,
    default_protobuf_mod: &str,
    default_resource_type: Option<&str>,
) -> syn::Result<TokenStream> {
    let modifiers = parse_attributes(&input.attrs, default_protobuf_mod, default_resource_type)?;
    match &input.data {
        syn::Data::Struct(data) => derive_struct(input, modifiers, data),
        syn::Data::Enum(data) => derive_enum(input, modifiers, data),
        syn::Data::Union(data) => Err(syn::Error::new_spanned(
            data.union_token,
            "unions not supported for MeshPayload",
        )),
    }
}

struct Modifiers {
    impl_for_type: Option<Path>,
    bound: Option<Punctuated<WherePredicate, Token![,]>>,
    prost: bool,
    transparent: Option<Span>,
    resource_type: Option<Path>,
    protobuf_mod: Path,
    package: Option<LitStr>,
    rename: Option<LitStr>,
}

impl Modifiers {
    fn resource_type(&self) -> Path {
        self.resource_type
            .clone()
            .unwrap_or_else(|| Ident::new("AnyR", Span::call_site()).into())
    }
}

fn parse_string_attr(input: ParseStream<'_>) -> syn::Result<LitStr> {
    let _: syn::token::Eq = input.parse()?;
    input.parse()
}

struct WithSpan<T>(T, Span);

impl<T: Parse> Parse for WithSpan<T> {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let span = input.span();
        Ok(Self(T::parse(input)?, span))
    }
}

enum Attr {
    Bound(Punctuated<WherePredicate, Token![,]>),
    ImplFor(Path),
    Mod(Path),
    Resource(Path),
    Prost,
    Transparent,
    Package(LitStr),
    Rename(LitStr),
}

impl Parse for Attr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ident = Ident::parse_any(input)?;
        if ident == "bound" {
            let val = parse_string_attr(input)?;
            Ok(Self::Bound(val.parse_with(Punctuated::parse_terminated)?))
        } else if ident == "impl_for" {
            let val = parse_string_attr(input)?;
            Ok(Self::ImplFor(val.parse_with(Path::parse_mod_style)?))
        } else if ident == "prost" {
            Ok(Self::Prost)
        } else if ident == "mod" {
            let val = parse_string_attr(input)?;
            Ok(Self::Mod(val.parse_with(Path::parse_mod_style)?))
        } else if ident == "resource" {
            let val = parse_string_attr(input)?;
            Ok(Self::Resource(val.parse()?))
        } else if ident == "transparent" {
            Ok(Self::Transparent)
        } else if ident == "package" {
            Ok(Self::Package(parse_string_attr(input)?))
        } else if ident == "rename" {
            Ok(Self::Rename(parse_string_attr(input)?))
        } else {
            return Err(syn::Error::new_spanned(ident, "unknown attribute"));
        }
    }
}

fn parse_attr_list<T: Parse>(
    input: ParseStream<'_>,
) -> syn::Result<Punctuated<WithSpan<T>, syn::token::Comma>> {
    Punctuated::parse_terminated(input)
}

fn parse_attributes(
    attrs: &[Attribute],
    default_protobuf_mod: &str,
    default_resource_type: Option<&str>,
) -> syn::Result<Modifiers> {
    let mut impl_for_type = None;
    let mut bound = None;
    let mut prost = false;
    let mut protobuf_mod = None;
    let mut resource_type = None;
    let mut transparent = None;
    let mut package = None;
    let mut rename = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("mesh")) {
        for WithSpan(attr, span) in attr.parse_args_with(parse_attr_list)? {
            match attr {
                Attr::Bound(pred) => bound = Some(pred),
                Attr::ImplFor(path) => impl_for_type = Some(path),
                Attr::Mod(path) => protobuf_mod = Some(path),
                Attr::Prost => prost = true,
                Attr::Resource(path) => resource_type = Some(path),
                Attr::Transparent => transparent = Some(span),
                Attr::Package(val) => package = Some(val),
                Attr::Rename(val) => rename = Some(val),
            }
        }
    }
    Ok(Modifiers {
        impl_for_type,
        bound,
        prost,
        transparent,
        resource_type: resource_type
            .or_else(|| default_resource_type.map(|t| syn::parse_str(t).unwrap())),
        protobuf_mod: protobuf_mod.unwrap_or_else(|| syn::parse_str(default_protobuf_mod).unwrap()),
        package,
        rename,
    })
}

#[derive(Default)]
struct ItemModifiers {
    field_number: Option<LitInt>,
    field_encoding: Option<TypePath>,
    transparent: Option<Span>,
}

enum ItemAttr {
    Number(LitInt),
    Encoding(TypePath),
    Transparent,
}

impl Parse for ItemAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if let Ok(n) = LitInt::parse(input) {
            return Ok(Self::Number(n));
        }
        let ident = Ident::parse_any(input)?;
        if ident == "encoding" {
            let val = parse_string_attr(input)?;
            Ok(Self::Encoding(val.parse()?))
        } else if ident == "transparent" {
            Ok(Self::Transparent)
        } else {
            return Err(syn::Error::new_spanned(ident, "unknown attribute"));
        }
    }
}

fn parse_item_attributes(attrs: &[Attribute], in_enum: bool) -> syn::Result<ItemModifiers> {
    let mut modifiers = ItemModifiers::default();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("mesh")) {
        for WithSpan(attr, span) in attr.parse_args_with(parse_attr_list::<ItemAttr>)? {
            match attr {
                ItemAttr::Number(number) => modifiers.field_number = Some(number),
                ItemAttr::Encoding(encoding) => modifiers.field_encoding = Some(encoding),
                ItemAttr::Transparent => {
                    if !in_enum {
                        return Err(syn::Error::new(
                            span,
                            "transparent not supported on struct fields",
                        ));
                    }
                    modifiers.transparent = Some(span);
                }
            }
        }
    }
    Ok(modifiers)
}

fn doc_string(attrs: &[Attribute]) -> String {
    attrs
        .iter()
        .filter_map(|attr| {
            if attr.path().get_ident()? == "doc" {
                match &attr.meta.require_name_value().ok()?.value {
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(x),
                        ..
                    }) => Some(x.value()),
                    _ => None,
                }
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct FieldData<'a> {
    field: &'a syn::Field,
    span: Span,
    field_name: TokenStream,
    field_number: u32,
    field_number_span: Option<Span>,
    field_encoding_type: TokenStream,
}

fn field_data<'a>(protobuf_mod: &Path, fields: &'a Fields) -> syn::Result<Vec<FieldData<'a>>> {
    let fields = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let mods = parse_item_attributes(&field.attrs, false)?;
            let field_number = mods
                .field_number
                .as_ref()
                .map_or(Ok(i as u32 + 1), |n| n.base10_parse())?;
            let field_number_span = mods.field_number.as_ref().map(|n| n.span());

            let field_encoding_type = mods
                .field_encoding
                .as_ref()
                .map(|e| e.into_token_stream())
                .unwrap_or_else(|| {
                    let ty = &field.ty;
                    quote_spanned!(ty.span()=> <#ty as #protobuf_mod::DefaultEncoding>::Encoding)
                });

            let field_name = if let Some(ident) = &field.ident {
                ident.into_token_stream()
            } else {
                syn::Index::from(i).into_token_stream()
            };

            syn::Result::Ok(FieldData {
                field,
                span: field.ident.as_ref().map_or(field.ty.span(), |i| i.span()),
                field_name,
                field_number,
                field_number_span,
                field_encoding_type,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut numbers_and_spans = fields
        .iter()
        .map(|f| (f.field_number, f.field_number_span.unwrap_or(f.span)))
        .collect::<Vec<_>>();

    numbers_and_spans.sort_by_key(|x| x.0);
    // check for duplicate numbers
    for (&(left, span), &(right, _)) in numbers_and_spans
        .iter()
        .zip(numbers_and_spans.iter().skip(1))
    {
        if left == right {
            return Err(syn::Error::new(span, "duplicate field number"));
        }
    }

    Ok(fields)
}

enum BoundType {
    Encode,
    Decode,
    None,
}

/// Adds MeshPayload bounds to each generic parameter.
fn add_payload_bounds(
    protobuf_mod: &Path,
    orig_generics: &Generics,
    bound: &Option<Punctuated<WherePredicate, Token![,]>>,
    resource_type: &Path,
    bound_type: BoundType,
) -> Generics {
    let mut generics = orig_generics.clone();
    if let Some(bound) = bound {
        generics
            .make_where_clause()
            .predicates
            .extend(bound.clone());
    } else {
        // Add the default bounds on all generics.
        for param in &orig_generics.params {
            if let GenericParam::Type(type_param) = param {
                let param = &type_param.ident;
                let encoding_bound = match bound_type {
                    BoundType::Encode => {
                        quote_spanned!(param.span()=> #protobuf_mod::FieldEncode<#param, #resource_type>)
                    }
                    BoundType::Decode => quote_spanned!(param.span()=>
                        #protobuf_mod::FieldDecode<'encoding, #param, #resource_type>
                    ),
                    BoundType::None => break,
                };
                generics
                    .make_where_clause()
                    .predicates
                    .extend::<[WherePredicate; 2]>([
                        parse_quote_spanned!(param.span()=> #param: #protobuf_mod::DefaultEncoding),
                        parse_quote_spanned!(param.span()=> #param::Encoding: #encoding_bound),
                    ]);
            }
        }
    }
    generics
}

fn add_encoding_params(
    modifiers: &Modifiers,
    generics: &ImplGenerics<'_>,
    for_encode: bool,
) -> Generics {
    let mut generics: Generics = syn::parse2(generics.to_token_stream()).unwrap();
    if modifiers.resource_type.is_none() {
        generics.params.push(parse_quote!(AnyR: 'static));
    }
    if !for_encode {
        let mut encoding_def = LifetimeParam::new(Lifetime::new("'encoding", Span::call_site()));
        for param in &mut generics.params {
            if let GenericParam::Lifetime(lifetime_def) = param {
                lifetime_def.bounds.push(encoding_def.lifetime.clone());
                encoding_def.bounds.push(lifetime_def.lifetime.clone());
            }
        }
        generics.params.push(GenericParam::Lifetime(encoding_def));
    }
    generics
}

fn derive_transparent_struct(
    input: &DeriveInput,
    modifiers: Modifiers,
    data: &DataStruct,
) -> syn::Result<TokenStream> {
    let Modifiers {
        impl_for_type,
        bound,
        prost,
        transparent,
        resource_type,
        protobuf_mod,
        package,
        rename: name,
    } = &modifiers;

    if impl_for_type.is_some()
        || bound.is_some()
        || *prost
        || resource_type.is_some()
        || package.is_some()
        || name.is_some()
    {
        return Err(syn::Error::new(
            transparent.unwrap(),
            "invalid attribute mix",
        ));
    }

    if data.fields.iter().len() != 1 {
        return Err(syn::Error::new(
            transparent.unwrap(),
            "must have only one field",
        ));
    }

    let field = &field_data(protobuf_mod, &data.fields)?[0];
    let inner_field = field
        .field
        .ident
        .as_ref()
        .map_or_else(|| quote!(0), |ident| ident.to_token_stream());
    let inner_type = &field.field.ty;
    let inner_encoding = &field.field_encoding_type;

    let mut generics = input.generics.clone();

    // Add requested bound.
    if let Some(bound) = bound {
        generics
            .make_where_clause()
            .predicates
            .extend(bound.clone());
    } else {
        // Add a default bounds on all generics.
        let bound: syn::TypeParamBound = parse_quote!(#protobuf_mod::DefaultEncoding);
        for param in &mut generics.params {
            if let GenericParam::Type(ref mut type_param) = *param {
                type_param.bounds.push(bound.clone());
            }
        }
    }

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let this_ident = input.ident.clone();
    let this = quote! { #this_ident #ty_generics };

    Ok(quote! {
        // SAFETY: the type has only one field, so it can be encoded/decoded
        // just by encoding/decoding the struct. And the offset is correct.
        unsafe impl #impl_generics #protobuf_mod::transparent::Transparent for #this #where_clause {
            type Inner = #inner_type;
            const OFFSET: usize = ::core::mem::offset_of!(#this, #inner_field);
        }

        impl #impl_generics #protobuf_mod::DefaultEncoding for #this #where_clause {
            type Encoding = #protobuf_mod::transparent::TransparentEncoding<#inner_encoding>;
        }
    })
}

/// Derives MeshPayload for a struct.
fn derive_struct(
    input: &DeriveInput,
    modifiers: Modifiers,
    data: &DataStruct,
) -> syn::Result<TokenStream> {
    if modifiers.transparent.is_some() {
        return derive_transparent_struct(input, modifiers, data);
    }

    let protobuf_mod = &modifiers.protobuf_mod;
    let type_ident = modifiers
        .impl_for_type
        .clone()
        .unwrap_or_else(|| input.ident.clone().into());

    let none_generics = add_payload_bounds(
        protobuf_mod,
        &input.generics,
        &modifiers.bound,
        &modifiers.resource_type(),
        BoundType::None,
    );
    let (message_impl_generics, message_ty_generics, message_where_clause) =
        none_generics.split_for_impl();

    let encode_generics = add_payload_bounds(
        protobuf_mod,
        &input.generics,
        &modifiers.bound,
        &modifiers.resource_type(),
        BoundType::Encode,
    );
    let (encode_impl_generics, encode_ty_generics, encode_where_clause) =
        encode_generics.split_for_impl();
    let encode_impl_generics = add_encoding_params(&modifiers, &encode_impl_generics, true);

    let decode_generics = add_payload_bounds(
        protobuf_mod,
        &input.generics,
        &modifiers.bound,
        &modifiers.resource_type(),
        BoundType::Decode,
    );
    let (decode_impl_generics, _, decode_where_clause) = decode_generics.split_for_impl();
    let decode_impl_generics = add_encoding_params(&modifiers, &decode_impl_generics, false);

    if modifiers.prost {
        return Ok(quote! {
            impl #message_impl_generics #protobuf_mod::DefaultEncoding for #type_ident #message_ty_generics #message_where_clause {
                type Encoding = #protobuf_mod::encoding::MessageEncoding<#protobuf_mod::prost::ProstMessage>;
            }
        });
    }

    let field_data = field_data(protobuf_mod, &data.fields)?;

    let this = quote! { #type_ident #encode_ty_generics };
    let resource_type = modifiers.resource_type();

    let describe = if modifiers.package.is_some() {
        if matches!(data.fields, Fields::Named(_)) {
            if let Some(field) = field_data
                .iter()
                .find(|field| field.field_number_span.is_none())
            {
                return Err(syn::Error::new(
                    field.span,
                    "all fields must have explicit numbers when package is set",
                ));
            }
        }
        let field_descriptors = describe_fields(protobuf_mod, &field_data);
        describe_message(input, &modifiers, &field_descriptors, &[], &[], true)
    } else {
        TokenStream::new()
    };

    let field_numbers = field_data.iter().map(|field| field.field_number);
    let field_names = field_data.iter().map(|field| &field.field_name);
    let field_types = field_data
        .iter()
        .map(|field| &field.field.ty)
        .collect::<Vec<_>>();
    let field_encodings = field_data
        .iter()
        .map(|field| &field.field_encoding_type)
        .collect::<Vec<_>>();

    Ok(quote! {
        unsafe impl #message_impl_generics #protobuf_mod::table::StructMetadata for #this #message_where_clause {
            const NUMBERS: &'static [u32] = &[#(#field_numbers,)*];
            const OFFSETS: &'static [usize] = &[#(::core::mem::offset_of!(Self, #field_names),)*];
        }

        unsafe impl #encode_impl_generics #protobuf_mod::table::encode::StructEncodeMetadata<#resource_type> for #this #encode_where_clause {
            const ENCODERS: &'static [#protobuf_mod::table::encode::ErasedEncoderEntry] =
                &[#(<#field_encodings as #protobuf_mod::FieldEncode<#field_types, #resource_type>>::ENTRY.erase(),)*];
        }

        unsafe impl #decode_impl_generics #protobuf_mod::table::decode::StructDecodeMetadata<'encoding, #resource_type> for #this #decode_where_clause {
            const DECODERS: &'static [#protobuf_mod::table::decode::ErasedDecoderEntry] =
                &[#(<#field_encodings as #protobuf_mod::FieldDecode<'encoding, #field_types, #resource_type>>::ENTRY.erase(),)*];
        }

        impl #message_impl_generics #protobuf_mod::DefaultEncoding for #this #message_where_clause {
            type Encoding = #protobuf_mod::table::TableEncoder;
        }

        #describe
    })
}

fn describe_fields(protobuf_mod: &Path, field_data: &[FieldData<'_>]) -> Vec<TokenStream> {
    field_data.iter().map(|field| {
        let field_doc = doc_string(&field.field.attrs);
        let field_type = &field.field.ty;
        let field_name = field.field.ident.as_ref().map_or_else(|| format!("field{}", field.field_number), |id| id.to_string());
        let field_number = field.field_number;
        let field_encoding = &field.field_encoding_type;
        quote_spanned! {field.span=>
            #protobuf_mod::protofile::FieldDescriptor::new(#field_doc, <#field_encoding as #protobuf_mod::protofile::DescribeField<#field_type>>::FIELD_TYPE, #field_name, #field_number)
         }
    }).collect()
}

fn describe_message(
    input: &DeriveInput,
    modifiers: &Modifiers,
    field_descriptors: &[TokenStream],
    oneof_descriptors: &[TokenStream],
    message_descriptors: &[TokenStream],
    table_encoder: bool,
) -> TokenStream {
    let protobuf_mod = &modifiers.protobuf_mod;
    let package = modifiers.package.as_ref().unwrap();

    let ty = &input.ident;
    let name = if let Some(name) = &modifiers.rename {
        name.clone()
    } else {
        LitStr::new(&input.ident.to_string(), input.ident.span())
    };

    let tr = if table_encoder {
        quote!(#protobuf_mod::table::DescribeTable)
    } else {
        quote!(#protobuf_mod::oneof::DescribeOneof)
    };

    let doc = doc_string(&input.attrs);
    quote_spanned! {ty.span()=>
        impl #tr for #ty {
            const DESCRIPTION: #protobuf_mod::protofile::MessageDescription<'static> = {
                let tld = &#protobuf_mod::protofile::TopLevelDescriptor::message(
                    #package,
                    &#protobuf_mod::protofile::MessageDescriptor::new(#name, #doc, &[#(#field_descriptors,)*], &[#(#oneof_descriptors,)*], &[#(#message_descriptors,)*])
                );
                #protobuf_mod::protofile::MessageDescription::Internal(tld)
            };
        }
    }
}

/// Derives MeshPayload for an enum.
fn derive_enum(
    input: &DeriveInput,
    modifiers: Modifiers,
    data: &DataEnum,
) -> syn::Result<TokenStream> {
    let protobuf_mod = &modifiers.protobuf_mod;
    if modifiers.prost {
        // Prost enums (used for oneof) are not directly mesh compatible.
        return Ok(TokenStream::new());
    }

    if let Some(transparent) = modifiers.transparent {
        return Err(syn::Error::new(
            transparent,
            "transparent not supported on enums",
        ));
    }

    let type_ident = modifiers
        .impl_for_type
        .clone()
        .unwrap_or_else(|| input.ident.clone().into());

    let none_generics = add_payload_bounds(
        protobuf_mod,
        &input.generics,
        &modifiers.bound,
        &modifiers.resource_type(),
        BoundType::None,
    );
    let (message_impl_generics, _, message_where_clause) = none_generics.split_for_impl();

    let encode_generics = add_payload_bounds(
        protobuf_mod,
        &input.generics,
        &modifiers.bound,
        &modifiers.resource_type(),
        BoundType::Encode,
    );
    let (encode_impl_generics, encode_ty_generics, encode_where_clause) =
        encode_generics.split_for_impl();
    let encode_impl_generics = add_encoding_params(&modifiers, &encode_impl_generics, true);

    let decode_generics = add_payload_bounds(
        protobuf_mod,
        &input.generics,
        &modifiers.bound,
        &modifiers.resource_type(),
        BoundType::Decode,
    );
    let (decode_impl_generics, _, decode_where_clause) = decode_generics.split_for_impl();
    let decode_impl_generics = add_encoding_params(&modifiers, &decode_impl_generics, false);

    let this = quote! { #type_ident #encode_ty_generics };
    let resource_type = modifiers.resource_type();

    let mut variant_numbers = BTreeSet::new();

    // Make a write, size, and read case for each variant in the enum.
    let mut writes = Vec::new();
    let mut sizes = Vec::new();
    let mut reads = Vec::new();
    let mut message_descriptors = Vec::new();
    let mut variant_descriptors = Vec::new();
    for (variant_index, variant) in data.variants.iter().enumerate() {
        let variant_ident = &variant.ident;

        let mods = parse_item_attributes(&variant.attrs, true)?;
        if let Some(transparent) = mods.transparent {
            if variant.fields.len() != 1 {
                return Err(syn::Error::new(
                    transparent,
                    "transparent variants must have exactly one field",
                ));
            }
        }

        let variant_index = mods
            .field_number
            .as_ref()
            .map_or(Ok(variant_index as u32 + 1), |n| n.base10_parse())?;

        if !variant_numbers.insert(variant_index) {
            return Err(syn::Error::new_spanned(
                mods.field_number
                    .as_ref()
                    .map_or_else(|| variant.ident.to_token_stream(), |n| n.to_token_stream()),
                "duplicate field number",
            ));
        }

        // Get identifiers for the variant's fields.
        let field_idents: Vec<_> = (0..variant.fields.len())
            .map(|i| format_ident!("f{}", i))
            .collect();

        // Get something like This::Variant{x: f0, y: f1}
        let variant_destructured = match variant.fields {
            Fields::Named(_) => {
                let fieldname = variant.fields.iter().map(|f| f.ident.as_ref().unwrap());
                quote! { #type_ident::#variant_ident{#(#fieldname: #field_idents,)*} }
            }
            Fields::Unnamed(_) => quote! { #type_ident::#variant_ident(#(#field_idents,)*) },
            Fields::Unit => quote! {#type_ident::#variant_ident},
        };

        let variant_destructured_ref = match variant.fields {
            Fields::Named(_) => {
                let fieldname = variant.fields.iter().map(|f| f.ident.as_ref().unwrap());
                quote! { #type_ident::#variant_ident{#(#fieldname: ref #field_idents,)*} }
            }
            Fields::Unnamed(_) => {
                quote! { #type_ident::#variant_ident(#(ref #field_idents,)*) }
            }
            Fields::Unit => quote! {#type_ident::#variant_ident},
        };

        let field_data = field_data(protobuf_mod, &variant.fields)?;

        if modifiers.package.is_some() {
            if mods.field_number.is_none() {
                return Err(syn::Error::new_spanned(
                    &variant.ident,
                    "all variants must have explicit numbers when package is set",
                ));
            }
            if matches!(variant.fields, Fields::Named(_)) {
                if let Some(field) = field_data
                    .iter()
                    .find(|field| field.field_number_span.is_none())
                {
                    return Err(syn::Error::new(
                        field.span,
                        "all fields must have explicit numbers when package is set",
                    ));
                }
            }
        }

        if mods.transparent.is_some() {
            let field_ident = &field_idents[0];
            let field_encoding = &field_data[0].field_encoding_type;
            let field_type = &field_data[0].field.ty;
            writes.push(quote! {
                #variant_destructured => {
                    use #protobuf_mod::FieldEncode;
                    #field_encoding::write_field_in_sequence(#field_ident, &mut writer.field(#variant_index).sequence());
                }
            });

            sizes.push(quote! {
                #variant_destructured => {
                    <#field_encoding as #protobuf_mod::FieldEncode<_, #resource_type>>::compute_field_size_in_sequence(#field_ident, &mut sizer.field(#variant_index).sequence());
                }
            });

            reads.push(quote! {
                #variant_index => {
                    let mut v = match item.as_mut() {
                        Some(#variant_destructured) => {
                            let p = #field_ident as *mut #field_type;
                            item.forget();
                            // SAFETY: the inner field is valid.
                            unsafe { #protobuf_mod::inplace::InplaceOption::new_init_unchecked(&mut *p.cast::<::core::mem::MaybeUninit::<#field_type>>()) }
                        }
                        _ => {
                            // Try to write the variant data in place. If the
                            // sizes match, then there is only one place for it.
                            // Otherwise, the tag is probably at the beginning,
                            // so put the field at the end.
                            //
                            // If we get this wrong, the compiler will have to
                            // move the data but the code will still operate
                            // correctly.
                            let offset = if ::core::mem::size_of::<#field_type>() == ::core::mem::size_of::<Self>() {
                                0
                            } else {
                                ::core::mem::size_of::<Self>() - ::core::mem::size_of::<#field_type>()
                            };
                            // SAFETY: the offset is in bounds and the storage is unaliased.
                            #protobuf_mod::inplace::InplaceOption::uninit(unsafe { &mut *item.as_mut_ptr().byte_add(offset).cast() })
                        }
                    };
                    <#field_encoding as #protobuf_mod::FieldDecode<_, _>>::read_field_in_sequence(&mut v, field)?;
                    let #field_ident = v.take().unwrap();
                    item.set(#variant_destructured);
                }
            });

            if modifiers.package.is_some() {
                let field_type = &field_data[0].field.ty;
                let variant_snake_name = variant_ident.to_string().to_snake_case();
                let variant_doc = doc_string(&variant.attrs);
                variant_descriptors.push(quote! {
                    #protobuf_mod::protofile::FieldDescriptor::new(#variant_doc, <#field_encoding as #protobuf_mod::protofile::DescribeField<#field_type>>::FIELD_TYPE, #variant_snake_name, #variant_index)
                });
            }
        } else {
            let field_number = field_data
                .iter()
                .map(|f| f.field_number)
                .collect::<Vec<_>>();

            let field_types = field_data.iter().map(|f| &f.field.ty).collect::<Vec<_>>();

            let field_encoding = field_data
                .iter()
                .map(|f| &f.field_encoding_type)
                .collect::<Vec<_>>();

            let field_index = (0..field_data.len())
                .map(syn::Index::from)
                .collect::<Vec<_>>();

            let tuple = quote!(((#(#field_types,)*)));

            writes.push(quote! {
                #variant_destructured_ref => {
                    const NUMBERS: &[u32] = &[#(#field_number,)*];
                    let entries = const { &[#(<#field_encoding as #protobuf_mod::FieldEncode<#field_types, #resource_type>>::ENTRY.erase(),)*] };
                    // SAFETY: the fields are in the same object as `self`.
                    let offsets = &[#(unsafe { ::core::ptr::from_ref(#field_idents).byte_offset_from(&self) } as usize,)*];
                    writer.field(#variant_index).sequence().field().message(|message| {
                        // SAFETY: the encoders and offsets are correct for this enum variant.
                        unsafe {
                            #protobuf_mod::table::encode::write_fields(
                                NUMBERS,
                                entries,
                                offsets,
                                ::core::mem::MaybeUninit::new(self).as_mut_ptr().cast(),
                                message,
                            );
                        }
                    });
                }
            });

            sizes.push(quote! {
                #variant_destructured_ref => {
                    const NUMBERS: &[u32] = &[#(#field_number,)*];
                    let entries = const { &[#(<#field_encoding as #protobuf_mod::FieldEncode<#field_types, #resource_type>>::ENTRY.erase(),)*] };
                    // SAFETY: the fields are in the same object as `self`.
                    let offsets = &[#(unsafe { ::core::ptr::from_ref(#field_idents).byte_offset_from(self) } as usize,)*];
                    sizer.field(#variant_index).sequence().field().message(|message| {
                        // SAFETY: the encoders and offsets are correct for this enum variant.
                        unsafe {
                            #protobuf_mod::table::encode::compute_size_fields::<#resource_type>(
                                NUMBERS,
                                entries,
                                offsets,
                                ::core::ptr::from_mut(self).cast(),
                                message,
                            );
                        }
                    });
                }
            });

            reads.push(quote! {
                #variant_index => {
                    let mut init = false;
                    let mut tuple: ::core::mem::MaybeUninit<#tuple> = match item.take() {
                        Some(#variant_destructured) => {
                            init = true;
                            ::core::mem::MaybeUninit::new((#(#field_idents,)*))
                        }
                        _ => ::core::mem::MaybeUninit::uninit(),
                    };
                    const NUMBERS: &[u32] = &[#(#field_number,)*];
                    let offsets = const { &[#(::core::mem::offset_of!(#tuple, #field_index),)*] };
                    let entries = const { &[#(<#field_encoding as #protobuf_mod::FieldDecode<'encoding, #field_types, #resource_type>>::ENTRY.erase(),)*] };
                    // SAFETY: the encoders and offsets are correct for this enum variant.
                    let r = unsafe {
                        #protobuf_mod::table::decode::read_message(
                            NUMBERS,
                            entries,
                            offsets,
                            tuple.as_mut_ptr().cast(),
                            &mut init,
                            field,
                        )
                    };
                    if init {
                        // SAFETY: `read_message` guarantees that the value is initialized when `init` is true.
                        let (#(#field_idents,)*) = unsafe { tuple.assume_init() };
                        item.set(#variant_destructured);
                    }
                    r?;
                }
            });

            if modifiers.package.is_some() {
                let variant_name = variant_ident.to_string();
                let variant_snake_name = variant_name.to_snake_case();
                let variant_doc = doc_string(&variant.attrs);

                let proto_field_type = if variant.fields.is_empty() {
                    quote!(#protobuf_mod::protofile::FieldType::external("google.protobuf.Empty", "google/protobuf/empty.proto"))
                } else if is_standard_tuple(&variant.fields, &field_data) {
                    // Use a tuple type.
                    let fields:Vec<_> = field_data.iter().map(|field| {
                        let field_type = &field.field.ty;
                        let field_encoding = &field.field_encoding_type;
                        quote_spanned! {field.span=>
                            <#field_encoding as #protobuf_mod::protofile::DescribeField<#field_type>>::FIELD_TYPE
                        }
                    }).collect();

                    quote!(#protobuf_mod::protofile::FieldType::tuple(&[#(#fields,)*]))
                } else {
                    let field_descriptors = describe_fields(protobuf_mod, &field_data);
                    message_descriptors.push(quote! {
                        #protobuf_mod::protofile::MessageDescriptor::new(#variant_name, "", &[#(#field_descriptors,)*], &[], &[])
                    });
                    quote!(#protobuf_mod::protofile::FieldType::local(#variant_name))
                };

                variant_descriptors.push(quote! {
                    #protobuf_mod::protofile::FieldDescriptor::new(#variant_doc, #proto_field_type, #variant_snake_name, #variant_index)
                });
            }
        }
    }

    let describe = if modifiers.package.is_some() {
        let oneof_descriptor = quote!(#protobuf_mod::protofile::OneofDescriptor::new("variant", &[#(#variant_descriptors,)*]));
        describe_message(
            input,
            &modifiers,
            &[],
            &[oneof_descriptor],
            &message_descriptors,
            false,
        )
    } else {
        TokenStream::new()
    };

    Ok(quote! {
        impl #encode_impl_generics #protobuf_mod::oneof::OneofEncode<#resource_type> for #this #encode_where_clause {
            fn write_variant(self, mut writer: #protobuf_mod::protobuf::MessageWriter<'_, '_, #resource_type>) {
                match self {
                    #(#writes)*
                }
            }

            fn compute_variant_size(&mut self, mut sizer: #protobuf_mod::protobuf::MessageSizer<'_>) {
                match self {
                    #(#sizes)*
                }
            }
        }

        impl #decode_impl_generics #protobuf_mod::oneof::OneofDecode<'encoding, #resource_type> for #this #decode_where_clause {
            fn read_variant(item: &mut #protobuf_mod::inplace::InplaceOption<'_, Self>, n: u32, field: #protobuf_mod::protobuf::FieldReader<'encoding, '_, #resource_type>) -> #protobuf_mod::Result<()> {
                match n {
                    #(#reads)*
                    _ => item.clear(),
                }
                Ok(())
            }
        }

        impl #message_impl_generics #protobuf_mod::DefaultEncoding for #this #message_where_clause {
            type Encoding = #protobuf_mod::oneof::OneofEncoder;
        }

        #describe
    })
}

fn is_standard_tuple(fields: &Fields, field_data: &[FieldData<'_>]) -> bool {
    // If the fields have names, generate a type.
    if !matches!(fields, Fields::Unnamed(_)) {
        return false;
    }
    // If the individual fields have docs, then generate a type.
    if fields.iter().any(|f| !doc_string(&f.attrs).is_empty()) {
        return false;
    }
    // If there are any gaps in the field numbers, generate a type.
    if field_data
        .iter()
        .map(|f| f.field_number)
        .ne(1..=fields.len() as u32)
    {
        return false;
    }
    true
}
