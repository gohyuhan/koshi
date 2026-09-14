//! The `koshi remote` commands: save a server, change one, list the servers
//! this machine has saved, forget one, and replace the secret of one.
//!
//! Every verb reads the saved-server store on this machine. The ones that
//! change it read it again under a lock, apply the change, and write it back
//! through koshi's atomic replace. A change another koshi makes meanwhile is
//! never written over. A listing never prints a secret.
//!
//! `new` and `edit` ask three questions in turn — the name, the address and
//! the secret — and then dial the server once to check that it admits the
//! secret. A server that admits it pins the certificate it presented. A
//! server that does not is named, and the user answers whether to save what
//! they typed anyway. A saved server saved that way pins a certificate on its first
//! connection.
//!
//! `forget` and `set-secret` open no connection. A server that is switched
//! off is still forgotten, and still takes a fresh secret.

use std::time::SystemTime;

use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_servers::{SavedServer, SavedServerLookup, ServerStore};

use crate::cli::RemoteCommand;
use crate::{output, prompt};
use koshi_link::error::CliError;
use koshi_link::remote_client::{
    self, is_server_address, load_saved_server_store, prompt_line, prompt_secret,
    update_saved_server_store, validate_saved_server_name, DIAL_TIMEOUT_DURATION,
    REPLY_TIMEOUT_DURATION,
};

#[cfg(test)]
mod tests;

/// What the check of one server settled on.
#[derive(Debug, PartialEq, Eq)]
enum ServerCheckOutcome {
    /// The server admitted the secret, presenting this certificate
    /// fingerprint.
    Pinned(String),
    /// The server did not admit the secret, and the user said to save what
    /// they typed.
    Unpinned,
    /// The server did not admit the secret, and the user said not to save.
    Discarded,
}

/// Run one `remote` verb against the saved-server store and print the
/// rendered answer.
///
/// The store read here answers the listing and the questions the wizards ask.
/// Every verb that changes the store reads it again through
/// [`koshi_link::remote_client::update_saved_server_store`], which holds it
/// against every other koshi from that read to the write.
///
/// A `SERVER` argument matching no saved server is [`CliError::InvalidArgs`]
/// naming the command that lists what is saved.
pub fn run_remote_command(command: &RemoteCommand) -> Result<(), CliError> {
    let (_, mut server_store) = load_saved_server_store()?;
    match command {
        RemoteCommand::New => create_saved_server_from_prompts(&server_store),
        RemoteCommand::Edit { server_reference } => {
            edit_saved_server_from_prompts(&mut server_store, server_reference)
        }
        RemoteCommand::List { output_format } => {
            print!(
                "{}",
                output::render_remote_list(&server_store.saved_servers, *output_format)
            );
            Ok(())
        }
        RemoteCommand::Forget { server_reference } => {
            let server_address = update_saved_server_store(|server_store| {
                find_saved_server(server_store, server_reference)?;
                server_store
                    .forget_saved_server(server_reference)
                    .ok_or_else(|| build_saved_server_not_found_error(server_reference))
            })?;
            print!("{}", output::render_remote_forget(&server_address));
            Ok(())
        }
        RemoteCommand::SetSecret { server_reference } => {
            // Read first: the prompt names this address.
            let server_address = find_saved_server(&server_store, server_reference)?
                .server_address
                .clone();
            let connection_token = remote_client::resolve_server_connection_token(&server_address)?;
            update_saved_server_store(|server_store| {
                find_saved_server(server_store, server_reference)?;
                server_store.set_connection_token(server_reference, connection_token);
                Ok(())
            })?;
            print!("{}", output::render_remote_secret(&server_address));
            Ok(())
        }
    }
}

/// Save one server the user describes: ask for the name, the address and the
/// secret, check them against that server, and write the saved server.
///
/// Every answer is needed, and an empty one asks again. A saved server whose check
/// did not pass is saved with no pinned fingerprint once the user answers to
/// save it. Its first connection pins the certificate it meets.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, when the
/// input ended before an answer arrived, and when the saved server no longer fits
/// the store's naming rules. [`CliError::IpcUnavailable`] when the store could
/// not be read or written.
fn create_saved_server_from_prompts(server_store: &ServerStore) -> Result<(), CliError> {
    println!("every answer is needed. Ctrl-C stops without saving.");
    let server_name = prompt_until_valid_answer("name", None, |entered_name| {
        validate_server_name_available(server_store, entered_name)
    })?;
    let server_address = prompt_until_valid_answer("address", None, |entered_address| {
        validate_saved_server_address_is_available(server_store, entered_address)
    })?;
    let connection_token = ask_secret(None)?;

    let certificate_fingerprint = match check_saved_server_connection(
        &server_address,
        &connection_token,
        None,
        "save it anyway?",
    )? {
        ServerCheckOutcome::Pinned(certificate_fingerprint) => Some(certificate_fingerprint),
        ServerCheckOutcome::Unpinned => None,
        ServerCheckOutcome::Discarded => {
            print!("{}", output::render_remote_discarded());
            return Ok(());
        }
    };

    let current_time = SystemTime::now();
    let saved_server = SavedServer {
        server_name: Some(server_name),
        server_address,
        connection_token,
        last_used_at: certificate_fingerprint.is_some().then_some(current_time),
        certificate_fingerprint,
        added_at: current_time,
    };
    update_saved_server_store(|server_store| save_saved_server(server_store, &saved_server, None))?;
    print!("{}", output::render_remote_saved(&saved_server));
    Ok(())
}

/// Change what one saved server holds: ask for the name, the address and the
/// secret with what it holds now offered, check them against that server, and
/// write the saved server back.
///
/// An empty answer keeps the value in brackets, and an empty secret keeps the
/// saved secret. The saved server leaves the store before the questions, so its own
/// name and address are free to keep. It goes back once every answer has
/// settled, and nothing is written before that.
///
/// An address the user left alone requires the pinned fingerprint on the
/// check, and keeps it when the check does not pass. An address the user
/// changed requires none, and keeps none when the check does not pass; the
/// next connection to that address pins the certificate it meets. A check that
/// passes pins the certificate the server presented, either way.
///
/// The saved server on disk must still hold the name, the address, the secret and
/// the fingerprint it held when the questions opened, or nothing is written.
/// The added time and the last-used time come from the saved server on disk.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `server_reference` names no saved server, when it
/// names more than one, when the terminal could not be read, when the input
/// ended before an answer arrived, when the saved server changed while the questions
/// were open, and when the saved server no longer fits the store's naming rules.
/// [`CliError::IpcUnavailable`] when the store could not be read or written.
fn edit_saved_server_from_prompts(
    server_store: &mut ServerStore,
    server_reference: &str,
) -> Result<(), CliError> {
    let saved_server = find_saved_server(server_store, server_reference)?.clone();
    server_store.forget_saved_server(server_reference);

    println!(
        "press Enter to keep the value in brackets. An empty secret keeps the saved one. \
         Ctrl-C stops without saving."
    );
    let server_name = prompt_until_valid_answer(
        "name",
        saved_server.server_name.as_deref(),
        |entered_name| {
            if entered_name.is_empty() {
                Ok(())
            } else {
                validate_server_name_available(server_store, entered_name)
            }
        },
    )?;
    let server_address = prompt_until_valid_answer(
        "address",
        Some(&saved_server.server_address),
        |entered_address| validate_saved_server_address_is_available(server_store, entered_address),
    )?;
    let connection_token = ask_secret(Some(&saved_server.connection_token))?;

    let is_address_changed = server_address != saved_server.server_address;
    let confirmation_prompt = if is_address_changed {
        "save the change anyway? The certificate at that address is pinned on the \
         first connection to it."
    } else {
        "save the change anyway?"
    };
    let previous_certificate_fingerprint = resolve_certificate_fingerprint(
        saved_server.certificate_fingerprint.clone(),
        is_address_changed,
    );
    let certificate_fingerprint = match check_saved_server_connection(
        &server_address,
        &connection_token,
        previous_certificate_fingerprint.as_deref(),
        confirmation_prompt,
    )? {
        ServerCheckOutcome::Pinned(certificate_fingerprint) => Some(certificate_fingerprint),
        ServerCheckOutcome::Unpinned => None,
        ServerCheckOutcome::Discarded => {
            print!("{}", output::render_remote_discarded());
            return Ok(());
        }
    };

    let current_time = SystemTime::now();
    let updated_saved_server = update_saved_server_store(|server_store| {
        let disk_saved_server =
            find_unchanged_saved_server(server_store, server_reference, &saved_server)?;
        let updated_saved_server = SavedServer {
            server_name: (!server_name.is_empty()).then_some(server_name),
            server_address,
            connection_token,
            last_used_at: if certificate_fingerprint.is_some() {
                Some(current_time)
            } else {
                disk_saved_server.last_used_at
            },
            certificate_fingerprint: certificate_fingerprint.or(previous_certificate_fingerprint),
            added_at: disk_saved_server.added_at,
        };
        save_saved_server(server_store, &updated_saved_server, Some(server_reference))?;
        Ok(updated_saved_server)
    })?;
    print!("{}", output::render_remote_updated(&updated_saved_server));
    Ok(())
}

/// The saved server `server_reference` names in `server_store`, when it is still the
/// `saved_server_snapshot` the
/// questions were answered against.
///
/// The name, the address, the secret and the fingerprint are compared. The
/// last-used time and the added time are not: a saved server another koshi only
/// dialled still passes.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `server_reference` names no saved server, when it names more
/// than one, and when one of the four compared values changed.
fn find_unchanged_saved_server(
    server_store: &ServerStore,
    server_reference: &str,
    saved_server_snapshot: &SavedServer,
) -> Result<SavedServer, CliError> {
    let current_saved_server = find_saved_server(server_store, server_reference)?;
    if current_saved_server.server_name != saved_server_snapshot.server_name
        || current_saved_server.server_address != saved_server_snapshot.server_address
        || current_saved_server.connection_token != saved_server_snapshot.connection_token
        || current_saved_server.certificate_fingerprint
            != saved_server_snapshot.certificate_fingerprint
    {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "{server_reference} changed while the questions were open, so nothing was \
                 saved; run `koshi remote edit {server_reference}` again"
            ),
        });
    }
    Ok(current_saved_server.clone())
}

/// The fingerprint a saved server still holds after its address settles.
///
/// `held_certificate_fingerprint` is what it pinned before. The answer keeps
/// it when `is_address_changed` is false, and returns `None` when it is true.
///
/// Example — a saved server pinning `aa…aa` whose address the user left alone keeps
/// `aa…aa`. The same saved server moved to another address keeps nothing.
fn resolve_certificate_fingerprint(
    held_certificate_fingerprint: Option<String>,
    is_address_changed: bool,
) -> Option<String> {
    if is_address_changed {
        None
    } else {
        held_certificate_fingerprint
    }
}

/// Put `saved_server` in `server_store`, replacing the saved server named by
/// `replaced_server_reference`.
///
/// The replaced saved server leaves before the checks, so `saved_server` may keep its name
/// and its address. A refusal leaves `server_store` as it was. The store is not
/// written; the caller does that.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `replaced_server_reference` names no saved server or more than
/// one, and when another saved server answers to `saved server`'s name or its address.
fn save_saved_server(
    server_store: &mut ServerStore,
    saved_server: &SavedServer,
    replaced_server_reference: Option<&str>,
) -> Result<(), CliError> {
    let mut updated_server_store = server_store.clone();
    if let Some(replaced_server_reference) = replaced_server_reference {
        find_saved_server(&updated_server_store, replaced_server_reference)?;
        updated_server_store.forget_saved_server(replaced_server_reference);
    }
    if let Some(server_name) = saved_server.server_name.as_deref() {
        validate_server_name_available(&updated_server_store, server_name)?;
    }
    validate_saved_server_address_is_available(
        &updated_server_store,
        &saved_server.server_address,
    )?;
    updated_server_store
        .save_server(saved_server.clone())
        .map_err(|taken| CliError::InvalidArgs {
            detail: taken.to_string(),
        })?;
    *server_store = updated_server_store;
    Ok(())
}

/// Ask for one value until `validate_entered_answer` accepts it, and return what it settled on.
///
/// `previous_answer` is printed in brackets, and an empty answer keeps it.
/// With no `previous_answer` an empty answer uses the empty string, which
/// `validate_entered_answer` checks. Surrounding whitespace is trimmed. A
/// `validate_entered_answer` failure prints its reason and the question is asked again.
///
/// Example — `prompt_until_valid_answer("name", Some("work"), …)` prints `name [work]: `, and
/// pressing Enter settles on `work`.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer arrived.
fn prompt_until_valid_answer(
    prompt_label: &str,
    previous_answer: Option<&str>,
    validate_entered_answer: impl Fn(&str) -> Result<(), CliError>,
) -> Result<String, CliError> {
    let prompt_text = match previous_answer {
        Some(previous_answer) => format!("{prompt_label} [{previous_answer}]: "),
        None => format!("{prompt_label}: "),
    };
    loop {
        let entered_answer = prompt_line(&prompt_text)?;
        let accepted_answer = if entered_answer.is_empty() {
            previous_answer.unwrap_or_default()
        } else {
            &entered_answer
        };
        match validate_entered_answer(accepted_answer) {
            Ok(()) => return Ok(accepted_answer.to_string()),
            Err(validation_error) => eprintln!("koshi: {validation_error}"),
        }
    }
}

/// Ask for the secret to present to the server, without printing what is
/// typed.
///
/// `previous_connection_token` is the saved secret an empty answer keeps. With no
/// `previous_connection_token` an
/// empty answer asks again.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer arrived.
fn ask_secret(
    previous_connection_token: Option<&ConnectionToken>,
) -> Result<ConnectionToken, CliError> {
    loop {
        let entered_secret = prompt_secret("secret: ")?;
        if !entered_secret.is_empty() {
            return Ok(ConnectionToken::from_secret(entered_secret));
        }
        match previous_connection_token {
            Some(saved_connection_token) => return Ok(saved_connection_token.clone()),
            None => eprintln!("koshi: a secret is needed; paste the one the grant handed out"),
        }
    }
}

/// Dial the server at `server_address` once to check that it admits `connection_token`, and ask
/// `confirmation_prompt` when it does not.
///
/// `pinned_certificate_fingerprint` is the fingerprint the server must present, or `None` to take
/// whatever certificate it presents. The connection closes as soon as the
/// server admits the secret. No session is listed, and a server serving no
/// session passes this check.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer to `question` arrived.
fn check_saved_server_connection(
    server_address: &str,
    connection_token: &ConnectionToken,
    pinned_certificate_fingerprint: Option<&str>,
    confirmation_prompt: &str,
) -> Result<ServerCheckOutcome, CliError> {
    println!("checking {server_address} …");
    match remote_client::connect_remote_server(
        server_address,
        connection_token,
        pinned_certificate_fingerprint,
        DIAL_TIMEOUT_DURATION,
        Some(REPLY_TIMEOUT_DURATION),
    ) {
        Ok(remote_link) => Ok(ServerCheckOutcome::Pinned(
            remote_link.certificate_fingerprint,
        )),
        Err(connection_error) => {
            eprintln!("koshi: {}", CliError::from(connection_error));
            if read_confirmation_answer(confirmation_prompt)? {
                Ok(ServerCheckOutcome::Unpinned)
            } else {
                Ok(ServerCheckOutcome::Discarded)
            }
        }
    }
}

/// Ask `confirmation_prompt` and answer it with [`prompt::is_yes_answer`].
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer arrived.
fn read_confirmation_answer(confirmation_prompt: &str) -> Result<bool, CliError> {
    let entered_answer = prompt_line(&format!("{confirmation_prompt} [y/N]: "))?;
    Ok(prompt::is_yes_answer(&entered_answer))
}

/// `Ok(())` when `server_name` is a word this store can give to a saved server.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `server_name` is empty, when it has the shape of an
/// address, and when another saved server already answers to it by its own name or
/// its own address.
fn validate_server_name_available(
    server_store: &ServerStore,
    server_name: &str,
) -> Result<(), CliError> {
    if server_name.is_empty() {
        return Err(CliError::InvalidArgs {
            detail: "a name is needed, such as work".to_string(),
        });
    }
    validate_saved_server_name(server_name)?;
    validate_saved_server_reference_is_available(
        server_store,
        server_name,
        "run `koshi remote list` and pick another name",
    )
}

/// `Ok(())` when `server_address` is an address this store can give to a saved server.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `server_address` is not `host:port`, and when
/// another saved server already answers to it by its own name or its own address.
fn validate_saved_server_address_is_available(
    server_store: &ServerStore,
    server_address: &str,
) -> Result<(), CliError> {
    if !is_server_address(server_address) {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "an address is host:port, such as laptop.local:7654, and {server_address} is not"
            ),
        });
    }
    validate_saved_server_reference_is_available(
        server_store,
        server_address,
        &format!("run `koshi remote edit {server_address}` to change it"),
    )
}

/// `Ok(())` when no saved server in `server_store` answers to `server_reference`.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming `server_reference` and ending in `remedy` when a
/// saved server answers to it by its own name or its own address.
fn validate_saved_server_reference_is_available(
    server_store: &ServerStore,
    server_reference: &str,
    remedy: &str,
) -> Result<(), CliError> {
    match server_store.find_saved_server(server_reference) {
        SavedServerLookup::NotSaved => Ok(()),
        SavedServerLookup::Saved(_) | SavedServerLookup::Ambiguous => Err(CliError::InvalidArgs {
            detail: format!("{server_reference} already answers for a saved server; {remedy}"),
        }),
    }
}

/// The one saved server `server_reference` names.
///
/// # Errors
/// [`CliError::InvalidArgs`] when nothing is saved under that word, and a
/// different [`CliError::InvalidArgs`] when more than one saved server answers to
/// it.
fn find_saved_server<'a>(
    server_store: &'a ServerStore,
    server_reference: &str,
) -> Result<&'a SavedServer, CliError> {
    match server_store.find_saved_server(server_reference) {
        SavedServerLookup::Saved(saved_server) => Ok(saved_server),
        SavedServerLookup::NotSaved => Err(build_saved_server_not_found_error(server_reference)),
        SavedServerLookup::Ambiguous => Err(CliError::InvalidArgs {
            detail: format!(
                "{server_reference} is the name of one saved server and the address of another; \
                 run `koshi remote list` and name the one you mean"
            ),
        }),
    }
}

/// A `SERVER` argument that matches neither a saved name nor a saved address.
fn build_saved_server_not_found_error(server_reference: &str) -> CliError {
    CliError::InvalidArgs {
        detail: format!("no saved server is named {server_reference}; run `koshi remote list`"),
    }
}
