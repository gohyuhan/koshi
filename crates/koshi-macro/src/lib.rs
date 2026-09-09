//! `koshi-macro` provides koshi's procedural macros.
//!
//! The crate runs in the compiler and emits no runtime code.
//!
//! It provides [`beta_feature`], which wraps a function body with koshi-beta's
//! gate. `koshi-beta` re-exports the attribute and provides the generated calls.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, Expr, Ident, ItemFn, Token};

/// The attribute's one argument: what a blocked call returns instead.
struct Args {
    otherwise: Expr,
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        if name != "otherwise" {
            return Err(syn::Error::new(
                name.span(),
                "expected `otherwise = <expression>`",
            ));
        }
        input.parse::<Token![=]>()?;
        let otherwise: Expr = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("expected only `otherwise = <expression>`"));
        }
        Ok(Args { otherwise })
    }
}

/// Returns `true` only for the empty tuple expression `()`.
///
/// `otherwise = ()` produces `return;`. `otherwise = do_nothing()` produces
/// `return do_nothing();`, even when that call returns unit.
fn returns_unit(otherwise: &Expr) -> bool {
    matches!(otherwise, Expr::Tuple(tuple) if tuple.elems.is_empty())
}

/// Runs the function body only when `koshi.kdl`'s top-level
/// `allow-beta-features` is on.
///
/// When the setting is off, the body does not run and the call returns the
/// `otherwise` expression. `otherwise = ()` uses a bare `return;`; every other
/// expression uses `return <expression>;`.
///
/// The first blocked call of each gated function logs one warning. Subsequent
/// blocked calls of that function log nothing. The warning names the module
/// path and function identifier joined by `::`: `attach` in `session` is
/// `session::attach`. An `impl` method such as `Server::attach` in `session`
/// also uses `session::attach`; the type name is not included. The warning
/// tells the user to add a top-level `allow-beta-features #true` line to
/// `koshi.kdl`.
///
/// An ordinary function reads the setting at the call. An `async fn` reads it
/// at its first poll. An async computation that nobody polls reads nothing and
/// logs nothing.
///
/// The warning uses `tracing` and appears only when a subscriber is installed,
/// such as in an interactive session with `logging { enabled #true }`. A
/// `koshi <verb>` command has no subscriber, so a blocked call returns
/// `otherwise` without a warning.
///
/// The attribute requires exactly one argument, `otherwise = <expression>`.
/// Missing, misnamed, and extra arguments are compile errors.
///
/// Generated code calls `koshi_beta::allowed` and `koshi_beta::log_blocked`, so
/// the gated function's crate depends on `koshi-beta`.
///
/// ```ignore
/// #[beta_feature(otherwise = Ok(()))]
/// fn attach_to_session(id: SessionId) -> Result<(), CliError> {
///     // function body
/// }
/// ```
#[proc_macro_attribute]
pub fn beta_feature(args: TokenStream, item: TokenStream) -> TokenStream {
    let otherwise = parse_macro_input!(args as Args).otherwise;
    let mut function = parse_macro_input!(item as ItemFn);

    let name = function.sig.ident.to_string();
    // The path contains the module and function name, such as
    // `session::attach`; an `impl` type is not included.
    let path = quote!(::core::concat!(::core::module_path!(), "::", #name));
    let body = std::mem::take(&mut function.block.stmts);
    let give_up = if returns_unit(&otherwise) {
        quote!(return;)
    } else {
        quote!(return #otherwise;)
    };
    // Each original statement is interpolated separately, so the final
    // expression remains the function's tail expression.
    *function.block = syn::parse_quote!({
        if !::koshi_beta::allowed() {
            static BETA_WARNED: ::std::sync::Once = ::std::sync::Once::new();
            BETA_WARNED.call_once(|| ::koshi_beta::log_blocked(#path));
            #give_up
        }
        #(#body)*
    });

    quote!(#function).into()
}

#[cfg(test)]
mod tests;
