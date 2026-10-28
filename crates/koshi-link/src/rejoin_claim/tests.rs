//! Unit tests for reading the rejoin claim from the environment.

use super::*;

use crate::in_session::tests::{build_environment_lookup, CLIENT_UUID};

const RESUME_TOKEN_SECRET: &str = "resume-token-secret";

/// The client id [`CLIENT_UUID`] names.
fn build_test_client_id() -> ClientId {
    ClientId::from_uuid(CLIENT_UUID.parse().expect("the test uuid parses"))
}

#[test]
fn no_rejoin_variable_reads_as_no_claim() {
    assert_eq!(
        RejoinClaim::from_environment_lookup(build_environment_lookup(&[]))
            .expect("no variable is no error"),
        None
    );
}

#[test]
fn both_rejoin_variables_read_as_the_claim_they_name() {
    let client_id_text = format!("client-{CLIENT_UUID}");
    let rejoin_claim = RejoinClaim::from_environment_lookup(build_environment_lookup(&[
        (REJOIN_CLIENT_ID_VARIABLE_NAME, &client_id_text),
        (REJOIN_TOKEN_VARIABLE_NAME, RESUME_TOKEN_SECRET),
    ]))
    .expect("both variables read");

    assert_eq!(
        rejoin_claim,
        Some(RejoinClaim {
            client_id: build_test_client_id(),
            resume_token: ConnectionToken::from_secret(RESUME_TOKEN_SECRET),
        })
    );
}

#[test]
fn a_client_id_written_by_its_display_form_reads_back_as_the_same_id() {
    let client_id = ClientId::new();
    let client_id_text = client_id.to_string();

    let rejoin_claim = RejoinClaim::from_environment_lookup(build_environment_lookup(&[
        (REJOIN_CLIENT_ID_VARIABLE_NAME, &client_id_text),
        (REJOIN_TOKEN_VARIABLE_NAME, RESUME_TOKEN_SECRET),
    ]))
    .expect("the display form reads")
    .expect("both variables are set");

    assert_eq!(rejoin_claim.client_id, client_id);
}

#[test]
fn a_client_id_with_no_token_is_refused_naming_the_missing_variable() {
    let client_id_text = format!("client-{CLIENT_UUID}");
    let rejoin_claim_error = RejoinClaim::from_environment_lookup(build_environment_lookup(&[(
        REJOIN_CLIENT_ID_VARIABLE_NAME,
        &client_id_text,
    )]))
    .expect_err("a client id with no token is refused");

    assert_eq!(
        rejoin_claim_error.to_string(),
        "invalid arguments: `KOSHI_REJOIN_CLIENT_ID` is set and `KOSHI_REJOIN_TOKEN` is not"
    );
}

#[test]
fn a_token_with_no_client_id_is_refused_naming_the_missing_variable() {
    let rejoin_claim_error = RejoinClaim::from_environment_lookup(build_environment_lookup(&[(
        REJOIN_TOKEN_VARIABLE_NAME,
        RESUME_TOKEN_SECRET,
    )]))
    .expect_err("a token with no client id is refused");

    assert_eq!(
        rejoin_claim_error.to_string(),
        "invalid arguments: `KOSHI_REJOIN_TOKEN` is set and `KOSHI_REJOIN_CLIENT_ID` is not"
    );
}

#[test]
fn a_client_id_that_is_not_a_client_id_is_refused_naming_the_value() {
    let rejoin_claim_error = RejoinClaim::from_environment_lookup(build_environment_lookup(&[
        (REJOIN_CLIENT_ID_VARIABLE_NAME, "pane-1"),
        (REJOIN_TOKEN_VARIABLE_NAME, RESUME_TOKEN_SECRET),
    ]))
    .expect_err("a pane id is not a client id");

    assert_eq!(
        rejoin_claim_error.to_string(),
        "invalid arguments: `KOSHI_REJOIN_CLIENT_ID` is `pane-1`: expected `client-<uuid>` or a bare UUID"
    );
}

#[test]
fn a_bare_uuid_reads_as_the_client_id() {
    let rejoin_claim = RejoinClaim::from_environment_lookup(build_environment_lookup(&[
        (REJOIN_CLIENT_ID_VARIABLE_NAME, CLIENT_UUID),
        (REJOIN_TOKEN_VARIABLE_NAME, RESUME_TOKEN_SECRET),
    ]))
    .expect("a bare UUID reads");

    assert_eq!(
        rejoin_claim,
        Some(RejoinClaim {
            client_id: build_test_client_id(),
            resume_token: ConnectionToken::from_secret(RESUME_TOKEN_SECRET),
        })
    );
}

#[test]
fn an_empty_client_id_is_refused_naming_the_empty_value() {
    let rejoin_claim_error = RejoinClaim::from_environment_lookup(build_environment_lookup(&[
        (REJOIN_CLIENT_ID_VARIABLE_NAME, ""),
        (REJOIN_TOKEN_VARIABLE_NAME, RESUME_TOKEN_SECRET),
    ]))
    .expect_err("an empty client id is refused");

    assert_eq!(
        rejoin_claim_error.to_string(),
        "invalid arguments: `KOSHI_REJOIN_CLIENT_ID` is ``: expected `client-<uuid>` or a bare UUID"
    );
}

#[test]
fn a_claim_printed_for_debugging_shows_no_token() {
    let client_id = ClientId::new();
    let rejoin_claim = RejoinClaim {
        client_id,
        resume_token: ConnectionToken::from_secret(RESUME_TOKEN_SECRET),
    };

    assert_eq!(
        format!("{rejoin_claim:?}"),
        format!(
            "RejoinClaim {{ client_id: {client_id:?}, resume_token: {:?} }}",
            ConnectionToken::from_secret("another-secret")
        )
    );
}
