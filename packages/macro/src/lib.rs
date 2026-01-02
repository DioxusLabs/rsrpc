use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, Attribute, FnArg, Ident, ItemTrait, Pat, ReturnType, TraitItem, TraitItemFn,
    Type,
};

// =============================================================================
// HTTP METHOD TYPES
// =============================================================================

/// HTTP method for REST endpoints
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "get" => Some(Self::Get),
            "post" => Some(Self::Post),
            "put" => Some(Self::Put),
            "patch" => Some(Self::Patch),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    fn to_tokens(&self) -> TokenStream2 {
        match self {
            Self::Get => quote!(::rsrpc::http::Method::GET),
            Self::Post => quote!(::rsrpc::http::Method::POST),
            Self::Put => quote!(::rsrpc::http::Method::PUT),
            Self::Patch => quote!(::rsrpc::http::Method::PATCH),
            Self::Delete => quote!(::rsrpc::http::Method::DELETE),
        }
    }

    fn to_axum_method(&self) -> TokenStream2 {
        match self {
            Self::Get => quote!(::rsrpc::axum::routing::get),
            Self::Post => quote!(::rsrpc::axum::routing::post),
            Self::Put => quote!(::rsrpc::axum::routing::put),
            Self::Patch => quote!(::rsrpc::axum::routing::patch),
            Self::Delete => quote!(::rsrpc::axum::routing::delete),
        }
    }
}

/// Parsed HTTP route information
#[derive(Debug, Clone)]
struct HttpRoute {
    method: HttpMethod,
    path: String,              // Original path for URL building
    axum_path: String,         // Axum-compatible path (uses :param instead of {param})
    path_params: Vec<String>,  // Parameters extracted from {param} in path
    query_params: Vec<String>, // Parameters extracted from ?param
}

/// Parse route string like "/logs/{id}/?limit" into components
fn parse_route(path: &str, method: HttpMethod) -> HttpRoute {
    let mut path_params = Vec::new();
    let mut query_params = Vec::new();
    let mut clean_path = String::new();
    let mut axum_path = String::new();

    // Split path from query params (marked with /?)
    let (path_part, query_part) = if let Some(idx) = path.find("/?") {
        (&path[..idx], Some(&path[idx + 2..]))
    } else {
        (path, None)
    };

    // Parse path parameters {param}
    let mut chars = path_part.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut param = String::new();
            while let Some(&pc) = chars.peek() {
                chars.next();
                if pc == '}' {
                    break;
                }
                param.push(pc);
            }
            path_params.push(param.clone());
            clean_path.push_str(&format!("{{{}}}", param));
            axum_path.push(':');
            axum_path.push_str(&param);
        } else {
            clean_path.push(c);
            axum_path.push(c);
        }
    }

    // Parse query parameters ?param1&param2 or ?param1/?param2
    if let Some(query) = query_part {
        for part in query.split(&['?', '&', '/'][..]) {
            let param = part.trim();
            if !param.is_empty() {
                query_params.push(param.to_string());
            }
        }
    }

    HttpRoute {
        method,
        path: clean_path,
        axum_path,
        path_params,
        query_params,
    }
}

/// Extract HTTP route from method attributes
fn parse_http_attrs(attrs: &[Attribute]) -> syn::Result<Option<HttpRoute>> {
    for attr in attrs {
        let path = attr.path();
        if path.segments.len() == 1 {
            let method_name = path.segments[0].ident.to_string();
            if let Some(method) = HttpMethod::from_str(&method_name) {
                // Parse the route string from attribute like #[get("/path")]
                let route_str: syn::LitStr = attr.parse_args()?;
                return Ok(Some(parse_route(&route_str.value(), method)));
            }
        }
    }
    Ok(None)
}

/// Marks a trait as an RPC service.
///
/// This macro generates:
/// - Per-method request structs (private)
/// - `impl Trait for Client<dyn Trait>` so clients can call methods directly
/// - `<dyn Trait>::serve(impl)` method to create a server
///
/// # Example
///
/// ```ignore
/// #[rsrpc::service]
/// pub trait Worker: Send + Sync + 'static {
///     async fn run_task(&self, task: Task) -> Result<Output, Error>;
///     async fn status(&self) -> WorkerStatus;
/// }
///
/// // Client: impl Worker for Client<dyn Worker>
/// // Server: <dyn Worker>::serve(my_impl)
/// ```
///
/// # Streaming
///
/// Methods returning `Result<RpcStream<T>>` are automatically handled as
/// server-side streaming - no special annotation needed:
///
/// ```ignore
/// #[rsrpc::service]
/// pub trait LogService: Send + Sync + 'static {
///     async fn stream_logs(&self, filter: Filter) -> Result<RpcStream<LogEntry>>;
/// }
/// ```
///
/// # Local (non-Send) services
///
/// Use `#[rsrpc::service(?Send)]` to allow non-Send futures. This is useful
/// when your async methods hold non-Send types across await points:
///
/// ```ignore
/// #[rsrpc::service(?Send)]
/// pub trait LocalWorker: 'static {
///     async fn run_task(&self, task: Task) -> Result<Output, Error>;
/// }
/// ```
///
/// Note: `?Send` services require a single-threaded runtime for the server.
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    let trait_def = parse_macro_input!(item as ItemTrait);
    let not_send = attr.to_string().contains("?Send");
    match generate_service(&trait_def, not_send) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn generate_service(trait_def: &ItemTrait, not_send: bool) -> syn::Result<TokenStream2> {
    let trait_name = &trait_def.ident;
    let trait_vis = &trait_def.vis;
    let trait_name_lower = to_snake_case(&trait_name.to_string());
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
    let request_structs: Vec<TokenStream2> =
        methods.iter().map(|m| generate_request_struct(m)).collect();

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
        .map(|(idx, m)| generate_client_method(m, idx as u16, trait_name))
        .collect();

    // Generate dispatch match arms
    let dispatch_arms: Vec<TokenStream2> = methods
        .iter()
        .enumerate()
        .map(|(idx, m)| generate_dispatch_arm(m, idx as u16))
        .collect();

    // Keep original trait but add async_trait
    let _trait_items = &trait_def.items;
    let trait_supertraits = &trait_def.supertraits;
    let trait_generics = &trait_def.generics;

    // Choose async_trait attribute based on Send requirement
    let async_trait_attr = if not_send {
        quote! { #[::rsrpc::async_trait(?Send)] }
    } else {
        quote! { #[::rsrpc::async_trait] }
    };

    // Dispatch function return type depends on Send requirement
    let dispatch_fn = if not_send {
        quote! {
            pub fn dispatch<'a, T: #trait_name + ?Sized>(
                service: &'a T,
                method_id: u16,
                payload: &'a [u8],
            ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::rsrpc::DispatchResult> + 'a>> {
                Box::pin(async move {
                    match method_id {
                        #(#dispatch_arms)*
                        _ => ::rsrpc::DispatchResult::Error(format!("Unknown method ID: {}", method_id)),
                    }
                })
            }
        }
    } else {
        quote! {
            pub fn dispatch<'a, T: #trait_name + ?Sized>(
                service: &'a T,
                method_id: u16,
                payload: &'a [u8],
            ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::rsrpc::DispatchResult> + Send + 'a>> {
                Box::pin(async move {
                    match method_id {
                        #(#dispatch_arms)*
                        _ => ::rsrpc::DispatchResult::Error(format!("Unknown method ID: {}", method_id)),
                    }
                })
            }
        }
    };

    // Generate HTTP client methods
    // Methods with HTTP attributes get real implementations
    // Methods without HTTP attributes get panic stubs
    let http_client_methods: Vec<TokenStream2> = methods
        .iter()
        .map(|m| {
            if let Some(tokens) = generate_http_client_method(m, trait_name) {
                tokens
            } else {
                // Generate panic stub for non-HTTP methods
                let name = &m.name;
                let return_type = &m.return_type;
                let arg_decls: Vec<TokenStream2> = m
                    .args
                    .iter()
                    .map(|(name, ty)| quote! { #name: #ty })
                    .collect();
                let method_name = name.to_string();
                quote! {
                    async fn #name(&self, #(#arg_decls),*) -> #return_type {
                        panic!(
                            "Method '{}' is not available over HTTP. Use RPC client instead.",
                            #method_name
                        )
                    }
                }
            }
        })
        .collect();

    // Generate HTTP server routes
    let http_routes = generate_http_routes(&methods, trait_name, trait_vis);

    // Check if any methods have HTTP attributes
    let has_http_methods = methods.iter().any(|m| m.http.is_some());

    // HTTP client impl (only if there are HTTP methods)
    let http_client_impl = if !has_http_methods {
        quote! {}
    } else {
        quote! {
            #[cfg(feature = "http")]
            #async_trait_attr
            impl #trait_name for ::rsrpc::HttpClient<dyn #trait_name> {
                #(#http_client_methods)*
            }
        }
    };

    // Strip HTTP attributes from trait items for the output
    let clean_trait_items: Vec<TokenStream2> = trait_def
        .items
        .iter()
        .map(|item| {
            if let TraitItem::Fn(method) = item {
                let mut clean_method = method.clone();
                // Remove HTTP method attributes (get, post, put, patch, delete)
                clean_method.attrs.retain(|attr| {
                    let path = attr.path();
                    if path.segments.len() == 1 {
                        let name = path.segments[0].ident.to_string().to_lowercase();
                        !matches!(name.as_str(), "get" | "post" | "put" | "patch" | "delete")
                    } else {
                        true
                    }
                });
                quote! { #clean_method }
            } else {
                quote! { #item }
            }
        })
        .collect();

    Ok(quote! {
        #async_trait_attr
        #trait_vis trait #trait_name #trait_generics : #trait_supertraits {
            #(#clean_trait_items)*
        }

        #[doc(hidden)]
        mod #mod_name {
            use super::*;
            use ::rsrpc::serde::{Serialize, Deserialize};

            #(#request_structs)*
            #(#method_ids)*

            // Dispatch function for the server
            #dispatch_fn
        }

        #async_trait_attr
        impl #trait_name for ::rsrpc::Client<dyn #trait_name> {
            #(#client_impl_methods)*
        }

        #http_client_impl

        /// Extension trait for creating servers from service implementations.
        impl dyn #trait_name {
            /// Create a server that hosts this service.
            #trait_vis fn serve<T: #trait_name + 'static>(service: T) -> ::rsrpc::Server<dyn #trait_name> {
                let service: ::std::sync::Arc<dyn #trait_name> = ::std::sync::Arc::new(service);
                ::rsrpc::Server::from_arc(service, #mod_name::dispatch)
            }
        }

        #http_routes
    })
}

struct MethodInfo {
    name: Ident,
    args: Vec<(Ident, Type)>, // (name, type) excluding self
    return_type: Type,
    http: Option<HttpRoute>, // HTTP route info if method has #[get], #[post], etc.
}

fn parse_method(method: &TraitItemFn) -> syn::Result<MethodInfo> {
    let name = method.sig.ident.clone();

    // Extract HTTP route from attributes like #[get("/path")]
    let http = parse_http_attrs(&method.attrs)?;

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
        http,
    })
}

fn generate_request_struct(method: &MethodInfo) -> TokenStream2 {
    // Use double underscore prefix to avoid conflicts with user types
    let struct_name = format_ident!("__{}Request", to_pascal_case(&method.name.to_string()));
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

fn generate_client_method(method: &MethodInfo, method_id: u16, trait_name: &Ident) -> TokenStream2 {
    let name = &method.name;
    let return_type = &method.return_type;

    let arg_names: Vec<&Ident> = method.args.iter().map(|(n, _)| n).collect();
    let arg_decls: Vec<TokenStream2> = method
        .args
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();

    // Use trait-based dispatch - ClientEncoding will select the right behavior
    // based on whether the return type is Result<T> or Result<RpcStream<T>>
    if arg_names.is_empty() {
        quote! {
            async fn #name(&self) -> #return_type {
                <#return_type as ::rsrpc::ClientEncoding<dyn #trait_name>>::invoke(
                    self,
                    #method_id,
                    &(),
                ).await
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
                #[derive(::rsrpc::serde::Serialize)]
                struct __Request { #(#request_fields),* }
                <#return_type as ::rsrpc::ClientEncoding<dyn #trait_name>>::invoke(
                    self,
                    #method_id,
                    &__Request { #(#arg_names),* },
                ).await
            }
        }
    }
}

fn generate_dispatch_arm(method: &MethodInfo, method_id: u16) -> TokenStream2 {
    let name = &method.name;
    let request_struct = format_ident!("__{}Request", to_pascal_case(&name.to_string()));
    let arg_names: Vec<&Ident> = method.args.iter().map(|(n, _)| n).collect();

    // Use trait-based dispatch - ServerEncoding will select the right behavior
    // based on whether the return type is Result<T> or Result<RpcStream<T>>
    if arg_names.is_empty() {
        quote! {
            #method_id => {
                let result = service.#name().await;
                ::rsrpc::ServerEncoding::into_dispatch(result)
            }
        }
    } else {
        quote! {
            #method_id => {
                let req: #request_struct = match ::rsrpc::postcard::from_bytes(payload) {
                    Ok(r) => r,
                    Err(e) => return ::rsrpc::DispatchResult::Error(e.to_string()),
                };
                let result = service.#name(#(req.#arg_names),*).await;
                ::rsrpc::ServerEncoding::into_dispatch(result)
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

// =============================================================================
// HTTP CODE GENERATION
// =============================================================================

/// Generate HTTP client method implementation
fn generate_http_client_method(method: &MethodInfo, _trait_name: &Ident) -> Option<TokenStream2> {
    let route = method.http.as_ref()?;
    let name = &method.name;
    let return_type = &method.return_type;

    let arg_decls: Vec<TokenStream2> = method
        .args
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();

    let http_method = route.method.to_tokens();

    // Build path with parameter substitution
    let path_template = &route.path;
    let path_params: Vec<&Ident> = method
        .args
        .iter()
        .filter(|(n, _)| route.path_params.contains(&n.to_string()))
        .map(|(n, _)| n)
        .collect();

    // Query params
    let query_params: Vec<&Ident> = method
        .args
        .iter()
        .filter(|(n, _)| route.query_params.contains(&n.to_string()))
        .map(|(n, _)| n)
        .collect();

    // Body params (everything not in path or query)
    let body_params: Vec<(&Ident, &Type)> = method
        .args
        .iter()
        .filter(|(n, _)| {
            !route.path_params.contains(&n.to_string())
                && !route.query_params.contains(&n.to_string())
        })
        .map(|(n, t)| (n, t))
        .collect();

    // Generate path building code
    let path_build = if path_params.is_empty() {
        quote! { #path_template.to_string() }
    } else {
        // Replace {param} with actual values
        let mut format_str = path_template.clone();
        let mut format_args = Vec::new();
        for param in &path_params {
            let param_str = param.to_string();
            format_str = format_str.replace(&format!("{{{}}}", param_str), "{}");
            format_args.push(quote! { #param });
        }
        quote! { format!(#format_str, #(#format_args),*) }
    };

    // Generate query string
    let query_build = if query_params.is_empty() {
        quote! { Vec::<(&str, String)>::new() }
    } else {
        let query_pairs: Vec<TokenStream2> = query_params
            .iter()
            .map(|p| {
                let p_str = p.to_string();
                quote! { (#p_str, #p.to_string()) }
            })
            .collect();
        quote! { vec![#(#query_pairs),*] }
    };

    // Generate body
    let body_build = if body_params.is_empty() {
        quote! { None::<&()> }
    } else if body_params.len() == 1 {
        let (body_name, _) = body_params[0];
        quote! { Some(&#body_name) }
    } else {
        // Multiple body params - wrap in anonymous struct
        let body_fields: Vec<TokenStream2> =
            body_params.iter().map(|(n, t)| quote! { #n: #t }).collect();
        let body_values: Vec<&Ident> = body_params.iter().map(|(n, _)| *n).collect();
        quote! {
            {
                #[derive(::rsrpc::serde::Serialize)]
                struct __Body { #(#body_fields),* }
                Some(&__Body { #(#body_values),* })
            }
        }
    };

    Some(quote! {
        async fn #name(&self, #(#arg_decls),*) -> #return_type {
            let path = #path_build;
            let query = #query_build;
            let body = #body_build;
            self.request(#http_method, &path, &query, body).await
        }
    })
}

/// Generate query struct for axum handler
fn generate_query_struct(method: &MethodInfo) -> Option<TokenStream2> {
    let route = method.http.as_ref()?;
    if route.query_params.is_empty() {
        return None;
    }

    let struct_name = format_ident!("__{}Query", to_pascal_case(&method.name.to_string()));
    let fields: Vec<TokenStream2> = method
        .args
        .iter()
        .filter(|(n, _)| route.query_params.contains(&n.to_string()))
        .map(|(name, ty)| quote! { pub #name: #ty })
        .collect();

    Some(quote! {
        #[derive(::rsrpc::serde::Deserialize)]
        struct #struct_name {
            #(#fields),*
        }
    })
}

/// Generate HTTP server handler for a method
fn generate_http_handler(method: &MethodInfo, trait_name: &Ident) -> Option<TokenStream2> {
    let route = method.http.as_ref()?;
    let name = &method.name;
    let handler_name = format_ident!("__{}_handler", name);

    // Path params extraction
    let path_param_types: Vec<&Type> = method
        .args
        .iter()
        .filter(|(n, _)| route.path_params.contains(&n.to_string()))
        .map(|(_, t)| t)
        .collect();

    let path_param_names: Vec<&Ident> = method
        .args
        .iter()
        .filter(|(n, _)| route.path_params.contains(&n.to_string()))
        .map(|(n, _)| n)
        .collect();

    // Query params
    let query_struct_name = format_ident!("__{}Query", to_pascal_case(&method.name.to_string()));
    let has_query = !route.query_params.is_empty();
    let query_param_names: Vec<&Ident> = method
        .args
        .iter()
        .filter(|(n, _)| route.query_params.contains(&n.to_string()))
        .map(|(n, _)| n)
        .collect();

    // Body params
    let body_params: Vec<(&Ident, &Type)> = method
        .args
        .iter()
        .filter(|(n, _)| {
            !route.path_params.contains(&n.to_string())
                && !route.query_params.contains(&n.to_string())
        })
        .map(|(n, t)| (n, t))
        .collect();

    // Build extractor list
    let mut extractors = Vec::new();
    let mut call_args = Vec::new();

    // State extractor (always first)
    extractors.push(quote! {
        ::rsrpc::axum::extract::State(service): ::rsrpc::axum::extract::State<::std::sync::Arc<T>>
    });

    // Path extractor
    if !path_param_names.is_empty() {
        if path_param_names.len() == 1 {
            let ty = path_param_types[0];
            let name = path_param_names[0];
            extractors.push(quote! {
                ::rsrpc::axum::extract::Path(#name): ::rsrpc::axum::extract::Path<#ty>
            });
            call_args.push(quote! { #name });
        } else {
            let names = &path_param_names;
            let types = &path_param_types;
            extractors.push(quote! {
                ::rsrpc::axum::extract::Path((#(#names),*)): ::rsrpc::axum::extract::Path<(#(#types),*)>
            });
            for name in &path_param_names {
                call_args.push(quote! { #name });
            }
        }
    }

    // Query extractor
    if has_query {
        extractors.push(quote! {
            ::rsrpc::axum::extract::Query(query): ::rsrpc::axum::extract::Query<#query_struct_name>
        });
        for name in &query_param_names {
            call_args.push(quote! { query.#name });
        }
    }

    // Body extractor
    if !body_params.is_empty() {
        if body_params.len() == 1 {
            let (name, ty) = body_params[0];
            extractors.push(quote! {
                ::rsrpc::axum::extract::Json(#name): ::rsrpc::axum::extract::Json<#ty>
            });
            call_args.push(quote! { #name });
        } else {
            let body_struct_name =
                format_ident!("__{}Body", to_pascal_case(&method.name.to_string()));
            let body_names: Vec<&Ident> = body_params.iter().map(|(n, _)| *n).collect();

            // The struct definition is generated in generate_http_routes
            extractors.push(quote! {
                ::rsrpc::axum::extract::Json(body): ::rsrpc::axum::extract::Json<#body_struct_name>
            });
            for name in &body_names {
                call_args.push(quote! { body.#name });
            }
        }
    }

    Some(quote! {
        async fn #handler_name<T: #trait_name>(
            #(#extractors),*
        ) -> impl ::rsrpc::axum::response::IntoResponse {
            match service.#name(#(#call_args),*).await {
                Ok(v) => ::rsrpc::axum::Json(v).into_response(),
                Err(e) => (
                    ::rsrpc::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    e.to_string()
                ).into_response(),
            }
        }
    })
}

/// Generate the http_routes method that returns an axum Router
fn generate_http_routes(
    methods: &[MethodInfo],
    trait_name: &Ident,
    trait_vis: &syn::Visibility,
) -> TokenStream2 {
    let http_methods: Vec<&MethodInfo> = methods.iter().filter(|m| m.http.is_some()).collect();

    if http_methods.is_empty() {
        return quote! {};
    }

    // Generate query structs
    let query_structs: Vec<TokenStream2> = http_methods
        .iter()
        .filter_map(|m| generate_query_struct(m))
        .collect();

    // Generate body structs for multi-param bodies
    let body_structs: Vec<TokenStream2> = http_methods
        .iter()
        .filter_map(|m| {
            let route = m.http.as_ref()?;
            let body_params: Vec<(&Ident, &Type)> = m
                .args
                .iter()
                .filter(|(n, _)| {
                    !route.path_params.contains(&n.to_string())
                        && !route.query_params.contains(&n.to_string())
                })
                .map(|(n, t)| (n, t))
                .collect();

            if body_params.len() > 1 {
                let struct_name = format_ident!("__{}Body", to_pascal_case(&m.name.to_string()));
                let fields: Vec<TokenStream2> = body_params
                    .iter()
                    .map(|(n, t)| quote! { pub #n: #t })
                    .collect();
                Some(quote! {
                    #[derive(::rsrpc::serde::Deserialize)]
                    struct #struct_name {
                        #(#fields),*
                    }
                })
            } else {
                None
            }
        })
        .collect();

    // Generate handlers
    let handlers: Vec<TokenStream2> = http_methods
        .iter()
        .filter_map(|m| generate_http_handler(m, trait_name))
        .collect();

    // Generate route registrations
    let routes: Vec<TokenStream2> = http_methods
        .iter()
        .filter_map(|m| {
            let route = m.http.as_ref()?;
            let handler_name = format_ident!("__{}_handler", m.name);
            let axum_path = &route.axum_path;
            let method_fn = route.method.to_axum_method();
            Some(quote! {
                .route(#axum_path, #method_fn(#handler_name::<T>))
            })
        })
        .collect();

    quote! {
        #[cfg(feature = "http")]
        const _: () = {
            use ::rsrpc::axum::response::IntoResponse;

            #(#query_structs)*
            #(#body_structs)*
            #(#handlers)*

            impl dyn #trait_name {
                /// Create an axum Router for HTTP endpoints.
                #trait_vis fn http_routes<T: #trait_name + 'static>(
                    service: ::std::sync::Arc<T>
                ) -> ::rsrpc::axum::Router {
                    ::rsrpc::axum::Router::new()
                        #(#routes)*
                        .with_state(service)
                }
            }
        };
    }
}
