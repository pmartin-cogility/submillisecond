use proc_macro::TokenStream;
use quote::{format_ident, quote, ToTokens};
use submillisecond_core::router::tree::{Node, NodeType};
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
    FnArg, Ident, ItemFn, LitStr, Pat, PatType, ReturnType, Type,
};

const REQUEST_TYPES: [&str; 3] = [
    "::submillisecond::Request",
    "submillisecond::Request",
    "Request",
];

pub struct Route {
    extractors: Vec<(Pat, Type)>,
    item_fn: ItemFn,
    method: RouteMethod,
    req_pat: Option<(Pat, Type)>,
    return_ty: Option<Type>,
    router_node: Node,
}

impl Route {
    pub fn parse_with_attributes(
        method: RouteMethod,
        attr: TokenStream,
        item: TokenStream,
    ) -> syn::Result<Self> {
        let attrs: RouteAttrs = syn::parse(attr)?;
        let mut item_fn: ItemFn = syn::parse(item)?;

        let mut router_node = Node::default();
        if let Err(err) = router_node.insert(attrs.path.value()) {
            return Err(syn::Error::new(attrs.path.span(), err.to_string()));
        }

        // Get request param and overwrite it with submillisecond::Request
        let mut req_pat = None;

        let extractors = item_fn
            .sig
            .inputs
            .iter()
            .filter_map(|input| match input {
                FnArg::Receiver(_) => Some(Err(syn::Error::new(
                    input.span(),
                    "routes cannot take self",
                ))),
                FnArg::Typed(PatType { pat, ty, .. }) => {
                    let ty_string = ty.to_token_stream().to_string().replace(' ', "");
                    if REQUEST_TYPES
                        .iter()
                        .any(|request_type| ty_string.starts_with(request_type))
                    {
                        if req_pat.is_some() {
                            Some(Err(syn::Error::new(ty.span(), "request defined twice")))
                        } else {
                            req_pat = Some((pat.as_ref().clone(), ty.as_ref().clone()));
                            None
                        }
                    } else {
                        Some(Ok((pat.as_ref().clone(), ty.as_ref().clone())))
                    }
                }
            })
            .collect::<Result<_, _>>()?;

        let req_arg: FnArg = syn::parse2(quote! { mut req: ::submillisecond::Request }).unwrap();
        item_fn.sig.inputs = Punctuated::from_iter([req_arg]);

        // Get return type and overwrite it with submillisecond::Response
        let return_ty = match item_fn.sig.output {
            ReturnType::Default => None,
            ReturnType::Type(_, ty) => match ty.as_ref() {
                Type::ImplTrait(_) => {
                    return Err(syn::Error::new(
                        ty.span(),
                        "routes cannot return impl types, use `_` instead",
                    ));
                }
                Type::Infer(_) => None,
                _ => Some(*ty),
            },
        };

        item_fn.sig.output = syn::parse2(quote! { -> ::std::result::Result<::submillisecond::Response, ::submillisecond::router::RouteError> }).unwrap();

        Ok(Route {
            extractors,
            item_fn,
            method,
            req_pat,
            return_ty,
            router_node,
        })
    }

    pub fn expand(self) -> TokenStream {
        let Route {
            extractors,
            method,
            req_pat,
            return_ty,
            router_node,
            ..
        } = &self;

        self.expand_with_body(|req, body| {
            let define_req_expanded = match req_pat {
                Some((req_pat, req_ty)) => quote! { let mut #req_pat: #req_ty = #req; },
                None => quote! {},
            };

            let define_extractors_expanded = extractors.iter().map(|(pat, ty)| quote! {
                let #pat = match <#ty as ::submillisecond::extract::FromRequest>::from_request(&mut #req) {
                    Ok(val) => val,
                    Err(err) => return ::std::result::Result::Err(
                        ::submillisecond::router::RouteError::ExtractorError(::submillisecond::response::IntoResponse::into_response(err))
                    ),
                };
            });

            let return_ty_expanded = match return_ty {
                Some(return_ty) => quote! { #return_ty },
                None => quote! { _ },
            };

            let router_node_expanded = expand_node(router_node);

            let method_expanded = match method {
                RouteMethod::GET => quote! { ::http::Method::GET },
                RouteMethod::POST => quote! { ::http::Method::POST },
                RouteMethod::PUT => quote! { ::http::Method::PUT },
                RouteMethod::DELETE => quote! { ::http::Method::DELETE },
                RouteMethod::HEAD => quote! { ::http::Method::HEAD },
                RouteMethod::OPTIONS => quote! { ::http::Method::OPTIONS },
                RouteMethod::PATCH => quote! { ::http::Method::PATCH },
            };

            quote! {
                {
                    const ROUTER_NODE: ::submillisecond_core::router::tree::ConstNode = #router_node_expanded;

                    if #method_expanded != #req.method() {
                        return ::std::result::Result::Err(::submillisecond::router::RouteError::RouteNotMatch(#req));
                    }

                    let route = #req.extensions().get::<::submillisecond::router::Route>().unwrap();
                    match ROUTER_NODE.at(route.path().as_bytes()) {
                        Ok(params) => {
                            let extensions = #req.extensions_mut();
                            match extensions.get_mut::<::submillisecond_core::router::params::Params>() {
                                Some(mut ext_params) => {
                                    ext_params.merge(params);
                                },
                                None => {
                                    extensions.insert(params);
                                }
                            }
                        },
                        Err(err) => return ::std::result::Result::Err(::submillisecond::router::RouteError::RouteNotMatch(#req)),
                    }
                }

                #define_req_expanded
                #( #define_extractors_expanded )*

                let response: #return_ty_expanded = (move || {
                    #body
                })();

                ::std::result::Result::Ok(::submillisecond::response::IntoResponse::into_response(response))
            }
        })
    }

    fn expand_with_body(
        &self,
        f: impl FnOnce(Ident, proc_macro2::TokenStream) -> proc_macro2::TokenStream,
    ) -> TokenStream {
        let Route {
            item_fn:
                ItemFn {
                    attrs,
                    vis,
                    sig,
                    block,
                },
            ..
        } = self;

        let attrs_expanded = if attrs.is_empty() {
            quote! {}
        } else {
            quote! {
                #[#(#attrs)*]
            }
        };

        let stmts = &block.stmts;
        let stmts_expanded = quote! { #( #stmts )* };

        let body = f(format_ident!("req"), stmts_expanded);

        quote! {
            #attrs_expanded
            #vis #sig {
                #body
            }
        }
        .into()
    }
}

#[allow(clippy::upper_case_acronyms)]
pub enum RouteMethod {
    GET,
    POST,
    PUT,
    DELETE,
    HEAD,
    OPTIONS,
    PATCH,
}

#[derive(Debug)]
struct RouteAttrs {
    path: LitStr,
}

impl Parse for RouteAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path = input.parse().map_err(|mut err| {
            err.extend(syn::Error::new(
                input.span(),
                "missing or invalid route path",
            ));
            err
        })?;

        Ok(RouteAttrs { path })
    }
}

fn expand_node(
    Node {
        priority,
        wild_child,
        indices,
        node_type,
        prefix,
        children,
    }: &Node,
) -> proc_macro2::TokenStream {
    let indices_expanded = indices.iter().map(|indicie| {
        quote! {
            #indicie
        }
    });

    let node_type_expanded = match node_type {
        NodeType::Root => quote! { ::submillisecond_core::router::tree::NodeType::Root },
        NodeType::Param => quote! { ::submillisecond_core::router::tree::NodeType::Param },
        NodeType::CatchAll => quote! { ::submillisecond_core::router::tree::NodeType::CatchAll },
        NodeType::Static => quote! { ::submillisecond_core::router::tree::NodeType::Static },
    };

    let prefix_expanded = prefix.iter().map(|prefix| {
        quote! {
            #prefix
        }
    });

    let children_expanded = children.iter().map(expand_node);

    quote! {
        ::submillisecond_core::router::tree::ConstNode {
            priority: #priority,
            wild_child: #wild_child,
            indices: &[#( #indices_expanded, )*],
            node_type: #node_type_expanded,
            prefix: &[#( #prefix_expanded, )*],
            children: &[#( #children_expanded, )*],
        }
    }
}