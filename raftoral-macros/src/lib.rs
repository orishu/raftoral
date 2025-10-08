extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, parse_quote, Expr, Stmt, Pat, PatIdent};

/// Attribute macro to create a replicated variable.
///
/// Transforms a `let` statement into a ReplicatedVar creation with an auto-generated key.
///
/// # Example
///
/// ```ignore
/// #[replicated(ctx)]
/// let mut counter = 0i32;
/// ```
///
/// Expands to:
///
/// ```ignore
/// let mut counter = ctx.create_replicated_var(
///     concat!("counter@", file!(), ":", line!(), ":", column!()),
///     0i32
/// ).await?;
/// ```
///
/// The variable can then be used with:
/// - `*counter` - to read the value (via Deref)
/// - `counter.set(value)` - to update the value
/// - `counter.update(|v| ...)` - to atomically update
#[proc_macro_attribute]
pub fn replicated(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse the context expression from the attribute
    let ctx_expr = parse_macro_input!(attr as Expr);

    // Parse as a TokenStream and convert to Stmt
    let item_tokens: proc_macro2::TokenStream = item.into();
    let stmt: Stmt = parse_quote! { #item_tokens };

    // Extract the Local from the statement
    let local = match stmt {
        Stmt::Local(local) => local,
        _ => {
            return syn::Error::new_spanned(
                item_tokens,
                "replicated macro can only be applied to let statements"
            ).to_compile_error().into();
        }
    };

    // Extract the variable name (simple identifier only)
    let var_name = match &local.pat {
        Pat::Ident(PatIdent { ident, .. }) => ident,
        Pat::Type(pat_type) => {
            if let Pat::Ident(PatIdent { ident, .. }) = &*pat_type.pat {
                ident
            } else {
                return syn::Error::new_spanned(
                    &local.pat,
                    "replicated macro only supports simple identifier patterns"
                ).to_compile_error().into();
            }
        }
        _ => {
            return syn::Error::new_spanned(
                &local.pat,
                "replicated macro only supports simple identifier patterns (let x = ...)"
            ).to_compile_error().into();
        }
    };

    // Extract the initializer expression
    let init_expr = match &local.init {
        Some(init) => &init.expr,
        None => {
            return syn::Error::new_spanned(
                &local,
                "replicated variable must have an initializer"
            ).to_compile_error().into();
        }
    };

    // Generate unique key using variable name and source location
    let var_name_str = var_name.to_string();
    let key_expr = quote! {
        concat!(#var_name_str, "@", file!(), ":", line!(), ":", column!())
    };

    // Extract mutability
    let mutability = if let Pat::Ident(PatIdent { mutability, .. }) = &local.pat {
        mutability
    } else if let Pat::Type(pat_type) = &local.pat {
        if let Pat::Ident(PatIdent { mutability, .. }) = &*pat_type.pat {
            mutability
        } else {
            &None
        }
    } else {
        &None
    };

    // Generate the expanded code
    let expanded = quote! {
        let #mutability #var_name = #ctx_expr.create_replicated_var(
            #key_expr,
            #init_expr
        ).await?;
    };

    TokenStream::from(expanded)
}
