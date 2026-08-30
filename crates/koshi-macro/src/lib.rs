//! `koshi-macro` — koshi's procedural macros.
//!
//! This crate runs inside the compiler. It puts no code in the binary.
//!
//! It holds [`beta_feature`], the attribute that writes the beta-feature gate.
//! `koshi-beta` re-exports the attribute beside the functions the generated
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

/// Reports whether `otherwise` is the literal `()`. A blocked call then gives
/// up with a bare `return;`.
///
/// Only the literal counts. `otherwise = ()` is true. A unit-valued call such
/// as `otherwise = do_nothing()` is false, and the blocked call returns that
/// expression.
fn returns_unit(otherwise: &Expr) -> bool {
    matches!(otherwise, Expr::Tuple(tuple) if tuple.elems.is_empty())
}

/// Runs the function's body only when `koshi.kdl`'s top-level
/// `allow-beta-features` is on.
///
/// If the setting is off, the body does not run and the call gives back the
/// `otherwise` expression. The first blocked call of each gated function logs a
/// warning that names the function and the setting. The other blocked calls of
/// that function log nothing.
///
/// The gate reads the setting where the body would start. An ordinary function
/// reads it at the call. An `async fn` reads it at the first poll. A future
/// that nobody polls reads nothing and logs nothing.
///
/// The warning travels through `tracing` and appears only where a subscriber is
/// installed, such as an interactive session with `logging { enabled #true }`. A
/// `koshi <verb>` command installs no subscriber: a call blocked there gives back
/// `otherwise` and shows no warning.
///
/// The attribute takes exactly one argument, `otherwise = <expression>`. A
/// missing, misnamed, or extra argument is a compile error.
///
/// The generated code calls `koshi_beta::allowed` and `koshi_beta::log_blocked`.
/// The gated function's crate depends on `koshi-beta`.
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
    // The original statements go back in one by one. The last one stays the
    // function's tail expression.
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
