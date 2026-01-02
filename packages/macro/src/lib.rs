use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, FnArg, Ident, ItemTrait, Pat, ReturnType, TraitItem, TraitItemFn, Type,
};

/// Marks a trait as an RPC service.
///
/// This macro generates:
/// - Per-method request structs (private)
/// - `impl Trait for Client<dyn Trait>` so clients can call methods directly
/// - `serve_<trait_name>(impl Trait) -> Server<T>` helper function
///
/// # Example
///
/// ```ignore
/// #[rrpc::service]
/// pub trait Worker: Send + Sync + 'static {
///     async fn run_task(&self, task: Task) -> Result<Output, Error>;
///     async fn status(&self) -> WorkerStatus;
/// }
///
/// // Generated: impl Worker for Client<dyn Worker>
/// // Generated: fn serve_worker(impl Worker) -> Server<impl Worker>
/// ```
#[proc_macro_attribute]
pub fn service(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let trait_def = parse_macro_input!(item as ItemTrait);
    match generate_service(&trait_def) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn generate_service(trait_def: &ItemTrait) -> syn::Result<TokenStream2> {
    let trait_name = &trait_def.ident;
    let trait_vis = &trait_def.vis;
    let trait_name_lower = to_snake_case(&trait_name.to_string());
    let serve_fn_name = format_ident!("serve_{}", trait_name_lower);
    let mod_name = format_ident!("__{}_rpc_impl", trait_name_lower);

    // Collect method info
    let methods: Vec<MethodInfo> = trait_def
        .items
        .iter()
        .filter_map(|item| {
            if let TraitItem::Fn(method) = item {
                Some(parse_method(method))
            } else {
                None
            }
        })
        .collect::<syn::Result<Vec<_>>>()?;

    // Generate request structs for each method
    let request_structs: Vec<TokenStream2> = methods
        .iter()
        .map(|m| generate_request_struct(m))
        .collect();

    // Generate method ID constants
    let method_ids: Vec<TokenStream2> = methods
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let const_name = format_ident!("{}_METHOD_ID", m.name.to_string().to_uppercase());
            let idx = idx as u16;
            quote! {
                const #const_name: u16 = #idx;
            }
        })
        .collect();

    // Generate Client<dyn Trait> impl
    let client_impl_methods: Vec<TokenStream2> = methods
        .iter()
        .enumerate()
        .map(|(idx, m)| generate_client_method(m, idx as u16))
        .collect();

    // Generate dispatch match arms
    let dispatch_arms: Vec<TokenStream2> = methods
        .iter()
        .enumerate()
        .map(|(idx, m)| generate_dispatch_arm(m, idx as u16))
        .collect();

    // Keep original trait but add async_trait
    let trait_items = &trait_def.items;
    let trait_supertraits = &trait_def.supertraits;
    let trait_generics = &trait_def.generics;

    Ok(quote! {
        #[::rrpc::async_trait]
        #trait_vis trait #trait_name #trait_generics : #trait_supertraits {
            #(#trait_items)*
        }

        #[doc(hidden)]
        mod #mod_name {
            use super::*;
            use ::rrpc::serde::{Serialize, Deserialize};

            #(#request_structs)*
            #(#method_ids)*

            // Dispatch function for the server
            pub fn dispatch<'a, T: #trait_name>(
                service: &'a T,
                method_id: u16,
                payload: &'a [u8],
            ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::anyhow::Result<Vec<u8>>> + Send + 'a>> {
                Box::pin(async move {
                    match method_id {
                        #(#dispatch_arms)*
                        _ => ::anyhow::bail!("Unknown method ID: {}", method_id),
                    }
                })
            }
        }

        /// Create a server for the #trait_name service.
        #trait_vis fn #serve_fn_name<T: #trait_name>(service: T) -> ::rrpc::Server<T> {
            ::rrpc::Server::new(service, #mod_name::dispatch)
        }

        #[::rrpc::async_trait]
        impl #trait_name for ::rrpc::Client<dyn #trait_name> {
            #(#client_impl_methods)*
        }
    })
}

struct MethodInfo {
    name: Ident,
    args: Vec<(Ident, Type)>, // (name, type) excluding self
    return_type: Type,
}

fn parse_method(method: &TraitItemFn) -> syn::Result<MethodInfo> {
    let name = method.sig.ident.clone();

    // Extract args, skipping self
    let args: Vec<(Ident, Type)> = method
        .sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    return Some((pat_ident.ident.clone(), (*pat_type.ty).clone()));
                }
            }
            None
        })
        .collect();

    // Extract return type
    let return_type = match &method.sig.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, ty) => (**ty).clone(),
    };

    Ok(MethodInfo {
        name,
        args,
        return_type,
    })
}

fn generate_request_struct(method: &MethodInfo) -> TokenStream2 {
    let struct_name = format_ident!("{}Request", to_pascal_case(&method.name.to_string()));
    let fields: Vec<TokenStream2> = method
        .args
        .iter()
        .map(|(name, ty)| {
            quote! { pub #name: #ty }
        })
        .collect();

    if fields.is_empty() {
        quote! {
            #[derive(Serialize, Deserialize)]
            struct #struct_name;
        }
    } else {
        quote! {
            #[derive(Serialize, Deserialize)]
            struct #struct_name {
                #(#fields),*
            }
        }
    }
}

fn generate_client_method(method: &MethodInfo, method_id: u16) -> TokenStream2 {
    let name = &method.name;
    let return_type = &method.return_type;

    let arg_names: Vec<&Ident> = method.args.iter().map(|(n, _)| n).collect();
    let arg_decls: Vec<TokenStream2> = method
        .args
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();

    if arg_names.is_empty() {
        quote! {
            async fn #name(&self) -> #return_type {
                self.call(#method_id, &()).await
            }
        }
    } else {
        let request_fields: Vec<TokenStream2> = method
            .args
            .iter()
            .map(|(name, ty)| quote! { #name: #ty })
            .collect();

        quote! {
            async fn #name(&self, #(#arg_decls),*) -> #return_type {
                #[derive(::rrpc::serde::Serialize)]
                struct __Request { #(#request_fields),* }
                self.call(#method_id, &__Request { #(#arg_names),* }).await
            }
        }
    }
}

fn generate_dispatch_arm(method: &MethodInfo, method_id: u16) -> TokenStream2 {
    let name = &method.name;
    let request_struct = format_ident!("{}Request", to_pascal_case(&name.to_string()));
    let arg_names: Vec<&Ident> = method.args.iter().map(|(n, _)| n).collect();

    // Use IntoWireResult to convert Result<T, E> to Result<T, String> for wire serialization
    // This allows any error type (including anyhow::Error) to be sent over the wire
    if arg_names.is_empty() {
        quote! {
            #method_id => {
                use ::rrpc::IntoWireResult;
                let result = service.#name().await;
                let wire_result = result.into_wire_result();
                let serialized = ::rrpc::postcard::to_allocvec(&wire_result)?;
                Ok(serialized)
            }
        }
    } else {
        quote! {
            #method_id => {
                use ::rrpc::IntoWireResult;
                let req: #request_struct = ::rrpc::postcard::from_bytes(payload)?;
                let result = service.#name(#(req.#arg_names),*).await;
                let wire_result = result.into_wire_result();
                let serialized = ::rrpc::postcard::to_allocvec(&wire_result)?;
                Ok(serialized)
            }
        }
    }
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_uppercase().next().unwrap());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}
