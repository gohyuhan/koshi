//! `koshi-beta` — the `#[beta_feature]` attribute.
//!
//! Marks an entry point as not stable enough to run for everyone yet. The
//! function is written as ordinary, finished code; the attribute is the whole
//! gate, so taking the feature out of beta is deleting one line per site.
//!
//! The gate it reads is `koshi_config::beta`, so a crate using the attribute
//! depends on both `koshi-beta` and `koshi-config`.

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
    // A function returning nothing is written `otherwise = ()`, and
    // `return ();` is what `clippy::unused_unit` rejects — emit `return;`.
    let is_unit = matches!(&otherwise, Expr::Tuple(tuple) if tuple.elems.is_empty());
    let give_up = if is_unit {
        quote!(return;)
    } else {
        quote!(return #otherwise;)
    };
    // The original statements are spliced in rather than nested as a block, so
    // the last one stays the function's tail expression.
    *function.block = syn::parse_quote!({
        if !::koshi_config::beta::allowed() {
            static BETA_WARNED: ::std::sync::Once = ::std::sync::Once::new();
            BETA_WARNED.call_once(|| ::koshi_config::beta::log_blocked(#name));
            #give_up
        }
        #(#body)*
    });

    quote!(#function).into()
}
