// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A code generator for protobuf service definitions.
//!
//! Used with the prost protobuf code generator.

#![warn(missing_docs)]
use heck::ToUpperCamelCase;
use proc_macro2::Span;
use syn::Ident;

/// A service generator for mesh services.
pub struct MeshServiceGenerator {
    replacements: Vec<(syn::TypePath, syn::Type)>,
}

impl MeshServiceGenerator {
    /// Creates a new service generator.
    pub fn new() -> Self {
        Self {
            replacements: Vec::new(),
        }
    }

    /// Configures the generator to replace any instance of Rust `ty` with
    /// `replacement`.
    ///
    /// This can be useful when some input or output messages already have mesh
    /// types defined, and you want to use them instead of the generated prost
    /// types.
    pub fn replace_type(mut self, ty: &str, replacement: &str) -> Self {
        let ty = syn::parse_str(ty).unwrap();
        let replacement = syn::parse_str(replacement).unwrap();
        self.replacements.push((ty, replacement));
        self
    }

    fn lookup_type(&self, ty: &str) -> syn::Type {
        let ty: syn::Type = syn::parse_str(ty).unwrap_or_else(|err| {
            panic!("failed to parse type {}: {}", ty, err);
        });
        if let syn::Type::Path(ty) = &ty {
            for (from, to) in &self.replacements {
                if from == ty {
                    return to.clone();
                }
            }
        }
        ty
    }
}

impl prost_build::ServiceGenerator for MeshServiceGenerator {
    fn generate(&mut self, service: prost_build::Service, buf: &mut String) {
        let name = format!("{}.{}", service.package, service.proto_name);
        let ident = Ident::new(&service.name, Span::call_site());
        let method_names: Vec<_> = service.methods.iter().map(|m| &m.proto_name).collect();
        let method_idents: Vec<_> = service
            .methods
            .iter()
            .map(|m| Ident::new(&m.name.to_upper_camel_case(), Span::call_site()))
            .collect();
        let request_types: Vec<_> = service
            .methods
            .iter()
            .map(|m| self.lookup_type(&m.input_type))
            .collect();
        let response_types: Vec<_> = service
            .methods
            .iter()
            .map(|m| self.lookup_type(&m.output_type))
            .collect();

        *buf += &quote::quote! {
            #[derive(Debug)]
            pub enum #ident {
                #(
                    #method_idents(
                        #request_types,
                        ::mesh::OneshotSender<::core::result::Result<#response_types, ::mesh_rpc::service::Status>>,
                    ),
                )*
            }

            impl #ident {
                #[allow(dead_code)]
                pub fn fail(self, status: ::mesh_rpc::service::Status) {
                    match self {
                        #(
                            #ident::#method_idents(_, response) => response.send(Err(status)),
                        )*
                    }
                }
            }

            impl ::mesh_rpc::service::ServiceRpc for #ident {
                const NAME: &'static str = #name;

                fn method(&self) -> &'static str {
                    match self {
                        #(
                            #ident::#method_idents(_, _) => #method_names,
                        )*
                    }
                }

                fn encode(
                    self,
                    writer: ::mesh::payload::protobuf::FieldWriter<'_, '_, ::mesh::resource::Resource>,
                ) -> ::mesh::local_node::Port {
                    match self {
                        #(
                            #ident::#method_idents(req, port) => {
                                <<#request_types as ::mesh::payload::DefaultEncoding>::Encoding as ::mesh::payload::FieldEncode<_, _>>::write_field(req, writer);
                                port.into()
                            }
                        )*
                    }
                }

                fn compute_size(&mut self, sizer: ::mesh::payload::protobuf::FieldSizer<'_>) {
                    match self {
                        #(
                            #ident::#method_idents(req, _) => {
                                <<#request_types as ::mesh::payload::DefaultEncoding>::Encoding as ::mesh::payload::FieldEncode::<_, ::mesh::resource::Resource>>::compute_field_size(
                                    req,
                                    sizer);
                            }
                        )*
                    }
                }

                fn decode(
                    method: &str,
                    port: ::mesh::local_node::Port,
                    data: &[u8],
                ) -> Result<Self, (::mesh_rpc::service::ServiceRpcError, ::mesh::local_node::Port)> {
                    match method {
                        #(
                            #method_names => {
                                match mesh::payload::decode(data) {
                                    Ok(req) => Ok(#ident::#method_idents(req, port.into())),
                                    Err(e) => Err((::mesh_rpc::service::ServiceRpcError::InvalidInput(e), port)),
                                }
                            }
                        )*
                        _ => Err((::mesh_rpc::service::ServiceRpcError::UnknownMethod, port)),
                    }
                }
            }
        }
        .to_string();
    }
}
