//! `koshi-macro` — koshi's procedural macros.
//!
//! A `proc-macro` crate may export nothing but macros, so anything a macro's
//! generated code calls lives in an ordinary crate elsewhere. This crate runs
//! inside the compiler and puts no code of its own in the binary.
//!
//! What lives here: [`beta_feature`], which writes the beta-feature gate. Reach
//! it through `koshi-beta`, which re-exports it beside the flag the generated
//! code calls.

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

/// Whether `otherwise` is the literal `()`. A blocked call then gives up with a
/// bare `return;`, because `return ();` is what `clippy::unused_unit` rejects.
///
/// Only the literal counts: `otherwise = ()` is true, while a unit-valued call
/// such as `otherwise = do_nothing()` is false and returns through `return`
/// with its expression.
fn returns_unit(otherwise: &Expr) -> bool {
    matches!(otherwise, Expr::Tuple(tuple) if tuple.elems.is_empty())
}

/// Runs the function's body only when `koshi.kdl`'s top-level
/// `allow-beta-features` is on.
///
/// When it is off the body never runs, the call gives back the `otherwise`
/// expression, and a warning naming the function and the knob is logged the
/// first time that site is reached. Beta-gated entry points do not share a
/// return type, which is why the fallback is spelled out per site.
///
/// The gate is read where the body would start: at the call for an ordinary
/// function, at the first poll for an `async fn`. An `async fn` whose future
/// is built and dropped unpolled reads nothing and logs nothing.
///
/// The warning travels `tracing`, so it lands only where a subscriber is
/// installed — an interactive session running with `logging { enabled #true }`.
/// A `koshi <verb>` command installs none, so a call blocked there gives back
/// `otherwise` silently.
///
/// The generated code calls `koshi_beta::allowed` and `koshi_beta::log_blocked`,
/// so the gated function's crate depends on `koshi-beta`.
///
/// ```ignore
/// #[beta_feature(otherwise = Ok(()))]
/// fn attach_to_session(id: SessionId) -> Result<(), CliError> {
///     // ordinary, finished code
/// }
/// ```
#[proc_macro_attribute]
pub fn beta_feature(args: TokenStream, item: TokenStream) -> TokenStream {
    let Args { otherwise } = parse_macro_input!(args as Args);
    let mut function = parse_macro_input!(item as ItemFn);

    let name = function.sig.ident.to_string();
    let body = std::mem::take(&mut function.block.stmts);
    let give_up = if returns_unit(&otherwise) {
        quote!(return;)
    } else {
        quote!(return #otherwise;)
    };
    // The original statements are spliced in rather than nested as a block, so
    // the last one stays the function's tail expression.
    *function.block = syn::parse_quote!({
        if !::koshi_beta::allowed() {
            static BETA_WARNED: ::std::sync::Once = ::std::sync::Once::new();
            BETA_WARNED.call_once(|| ::koshi_beta::log_blocked(#name));
            #give_up
        }
        #(#body)*
    });

    quote!(#function).into()
}

#[cfg(test)]
mod tests;
