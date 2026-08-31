//! Tests for the attribute's argument parser and its unit-return branch.
//!
//! Only the compiler runs the attribute itself. `koshi-beta`'s tests prove what
//! a gated function does. These tests prove the shape of the argument, the
//! message each malformed argument gets, and which fallback expression counts
//! as returning nothing.

use syn::parse_str;

use super::*;

/// Returns the parsed `otherwise` rendered back as text, or the error message
/// a caller sees.
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

/// A comma inside the expression stays inside it: `Err(E::new(1, 2))` is one
/// argument, not two.
#[test]
fn commas_inside_the_expression_do_not_end_it() {
    assert_eq!(
        parse("otherwise = Err(E::new(1, 2))"),
        Ok("Err (E :: new (1 , 2))".to_string())
    );
    assert_eq!(parse("otherwise = (1, 2)"), Ok("(1 , 2)".to_string()));
}

/// The literal `()` is an ordinary expression to the parser. `returns_unit`
/// decides what the blocked call does with it.
#[test]
fn the_literal_unit_parses_as_an_expression() {
    assert_eq!(parse("otherwise = ()"), Ok("()".to_string()));
    assert_eq!(parse("otherwise = ( )"), Ok("()".to_string()));
}

/// A block, a control-flow expression, and a `?` are each one whole argument.
#[test]
fn a_block_or_control_flow_value_is_kept_whole() {
    assert_eq!(parse("otherwise = { 1 }"), Ok("{ 1 }".to_string()));
    assert_eq!(
        parse("otherwise = if x { 1 } else { 2 }"),
        Ok("if x { 1 } else { 2 }".to_string())
    );
    assert_eq!(parse("otherwise = Err(e)?"), Ok("Err (e) ?".to_string()));
}

#[test]
fn an_argument_that_is_not_otherwise_is_rejected() {
    assert_eq!(
        parse("other = 1"),
        Err("expected `otherwise = <expression>`".to_string())
    );
}

/// The name is compared byte for byte: a capital letter or a raw identifier
/// does not match.
#[test]
fn the_name_must_match_otherwise_exactly() {
    assert_eq!(
        parse("Otherwise = 1"),
        Err("expected `otherwise = <expression>`".to_string())
    );
    assert_eq!(
        parse("r#otherwise = 1"),
        Err("expected `otherwise = <expression>`".to_string())
    );
}

#[test]
fn a_keyword_in_place_of_the_name_is_rejected() {
    assert_eq!(
        parse("fn = 1"),
        Err("expected identifier, found keyword `fn`".to_string())
    );
}

/// `otherwise: 1` fails at the separator. `otherwise == 1` takes the first `=`
/// as the separator and fails on the second.
#[test]
fn a_separator_other_than_equals_is_rejected() {
    assert_eq!(parse("otherwise: 1"), Err("expected `=`".to_string()));
    assert_eq!(
        parse("otherwise == 1"),
        Err("expected an expression".to_string())
    );
}

#[test]
fn a_value_that_is_not_an_expression_is_rejected() {
    assert_eq!(
        parse("otherwise = fn"),
        Err("expected an expression".to_string())
    );
}

#[test]
fn a_second_argument_is_rejected() {
    assert_eq!(
        parse("otherwise = 1, extra = 2"),
        Err("expected only `otherwise = <expression>`".to_string())
    );
}

/// A trailing comma, a semicolon, or a second bare token after the expression
/// gets the same message as a second named argument.
#[test]
fn anything_after_the_expression_is_rejected() {
    assert_eq!(
        parse("otherwise = 1,"),
        Err("expected only `otherwise = <expression>`".to_string())
    );
    assert_eq!(
        parse("otherwise = 1;"),
        Err("expected only `otherwise = <expression>`".to_string())
    );
    assert_eq!(
        parse("otherwise = 1 2"),
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

/// Only the literal `()` gives up with a bare `return;`. Every other expression
/// is kept, including a call that evaluates to nothing.
#[test]
fn only_the_literal_unit_returns_without_an_expression() {
    let unit = parse_str::<Expr>("()").unwrap();
    assert!(returns_unit(&unit));

    for other in ["(1, 2)", "do_nothing()", "0", "Ok(())"] {
        let expression = parse_str::<Expr>(other).unwrap();
        assert!(!returns_unit(&expression), "{other}");
    }
}

/// `( )` is the literal with whitespace. `(())` is a parenthesized unit and
/// `{ }` is an empty block: neither is the literal.
#[test]
fn a_wrapped_or_block_unit_is_not_the_literal() {
    let spaced = parse_str::<Expr>("( )").unwrap();
    assert!(returns_unit(&spaced));

    for other in ["(())", "{ }"] {
        let expression = parse_str::<Expr>(other).unwrap();
        assert!(!returns_unit(&expression), "{other}");
    }
}
