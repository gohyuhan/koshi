//! Tests for the attribute's argument parser and its unit-return branch.
//!
//! The attribute itself can only be run by the compiler, so what a gated
//! function does is proven in `koshi-beta`'s tests. What is proven here is the
//! part a caller gets wrong: the shape of the argument, and which fallback
//! expression counts as returning nothing.

use syn::parse_str;

use super::*;

/// The message a caller sees, or the parsed `otherwise` rendered back as text.
fn parse(args: &str) -> Result<String, String> {
    match parse_str::<Args>(args) {
        Ok(Args { otherwise }) => Ok(quote!(#otherwise).to_string()),
        Err(error) => Err(error.to_string()),
    }
}

#[test]
fn an_otherwise_expression_is_kept_whole() {
    assert_eq!(parse("otherwise = 0"), Ok("0".to_string()));
    assert_eq!(parse("otherwise = Ok(())"), Ok("Ok (())".to_string()));
}

/// The expression is parsed as one expression, so its own commas are its own.
/// `Err(E::new(1, 2))` must not read as two arguments.
#[test]
fn commas_inside_the_expression_do_not_end_it() {
    assert_eq!(
        parse("otherwise = Err(E::new(1, 2))"),
        Ok("Err (E :: new (1 , 2))".to_string())
    );
    assert_eq!(parse("otherwise = (1, 2)"), Ok("(1 , 2)".to_string()));
}

#[test]
fn an_argument_that_is_not_otherwise_is_rejected() {
    assert_eq!(
        parse("other = 1"),
        Err("expected `otherwise = <expression>`".to_string())
    );
}

#[test]
fn a_second_argument_is_rejected() {
    assert_eq!(
        parse("otherwise = 1, extra = 2"),
        Err("expected only `otherwise = <expression>`".to_string())
    );
}

#[test]
fn a_missing_name_or_value_is_rejected() {
    assert_eq!(
        parse(""),
        Err("unexpected end of input, expected identifier".to_string())
    );
    assert_eq!(parse("otherwise"), Err("expected `=`".to_string()));
    assert_eq!(
        parse("otherwise ="),
        Err("unexpected end of input, expected an expression".to_string())
    );
}

/// Only the literal `()` gives up with a bare `return;`. Anything else keeps
/// its expression, including a call that happens to evaluate to nothing.
#[test]
fn only_the_literal_unit_returns_without_an_expression() {
    let unit = parse_str::<Expr>("()").unwrap();
    assert!(returns_unit(&unit));

    for other in ["(1, 2)", "do_nothing()", "0", "Ok(())"] {
        let expression = parse_str::<Expr>(other).unwrap();
        assert!(!returns_unit(&expression), "{other}");
    }
}
