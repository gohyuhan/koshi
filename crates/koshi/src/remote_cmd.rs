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
//! they typed anyway. A record saved that way pins a certificate on its first
//! connection.
//!
//! `forget` and `set-secret` open no connection. A server that is switched
//! off is still forgotten, and still takes a fresh secret.

use std::time::SystemTime;

use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_servers::{Lookup, SavedServer, ServerStore};

use crate::cli::RemoteCommand;
use crate::output;
use koshi_link::error::CliError;
use koshi_link::remote_client::{
    self, check_name_shape, looks_like_address, prompt_line, prompt_secret, read_store,
    update_store, DIAL_WAIT, REPLY_WAIT,
};

#[cfg(test)]
mod tests;

/// What the check of one server settled on.
#[derive(Debug, PartialEq, Eq)]
enum Checked {
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
/// [`koshi_link::remote_client::update_store`], which holds it
/// against every other koshi from that read to the write.
///
/// A `SERVER` argument matching no saved record is [`CliError::InvalidArgs`]
/// naming the command that lists what is saved.
pub fn run(command: &RemoteCommand) -> Result<(), CliError> {
    let (_, mut store) = read_store()?;
    match command {
        RemoteCommand::New => run_new(&store),
        RemoteCommand::Edit { server } => run_edit(&mut store, server),
        RemoteCommand::List { format } => {
            print!("{}", output::render_remote_list(&store.records, *format));
            Ok(())
        }
        RemoteCommand::Forget { server } => {
            let address = update_store(|disk| {
                named(disk, server)?;
                disk.forget(server).ok_or_else(|| not_saved(server))
            })?;
            print!("{}", output::render_remote_forget(&address));
            Ok(())
        }
        RemoteCommand::SetSecret { server } => {
            // Read first: the prompt names this address.
            let address = named(&store, server)?.address.clone();
            let secret = remote_client::secret_for(&address)?;
            update_store(|disk| {
                named(disk, server)?;
                disk.set_secret(server, secret);
                Ok(())
            })?;
            print!("{}", output::render_remote_secret(&address));
            Ok(())
        }
    }
}

/// Save one server the user describes: ask for the name, the address and the
/// secret, check them against that server, and write the record.
///
/// Every answer is needed, and an empty one asks again. A record whose check
/// did not pass is saved with no pinned fingerprint once the user answers to
/// save it. Its first connection pins the certificate it meets.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, when the
/// input ended before an answer arrived, and when the record no longer fits
/// the store's naming rules. [`CliError::IpcUnavailable`] when the store could
/// not be read or written.
fn run_new(store: &ServerStore) -> Result<(), CliError> {
    println!("every answer is needed. Ctrl-C stops without saving.");
    let name = ask_until("name", None, |typed| free_name(store, typed))?;
    let address = ask_until("address", None, |typed| free_address(store, typed))?;
    let secret = ask_secret(None)?;

    let pinned = match check_server(&address, &secret, None, "save it anyway?")? {
        Checked::Pinned(fingerprint) => Some(fingerprint),
        Checked::Unpinned => None,
        Checked::Discarded => {
            print!("{}", output::render_remote_discarded());
            return Ok(());
        }
    };

    let now = SystemTime::now();
    let record = SavedServer {
        name: Some(name),
        address,
        secret,
        last_used_at: pinned.is_some().then_some(now),
        fingerprint: pinned,
        added_at: now,
    };
    update_store(|disk| place(disk, &record, None))?;
    print!("{}", output::render_remote_saved(&record));
    Ok(())
}

/// Change what one saved server holds: ask for the name, the address and the
/// secret with what it holds now offered, check them against that server, and
/// write the record back.
///
/// An empty answer keeps the value in brackets, and an empty secret keeps the
/// saved secret. The record leaves the store before the questions, so its own
/// name and address are free to keep. It goes back once every answer has
/// settled, and nothing is written before that.
///
/// An address the user left alone requires the pinned fingerprint on the
/// check, and keeps it when the check does not pass. An address the user
/// changed requires none, and keeps none when the check does not pass; the
/// next connection to that address pins the certificate it meets. A check that
/// passes pins the certificate the server presented, either way.
///
/// The record on disk must still hold the name, the address, the secret and
/// the fingerprint it held when the questions opened, or nothing is written.
/// The added time and the last-used time come from the record on disk.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `server` names no saved record, when it
/// names more than one, when the terminal could not be read, when the input
/// ended before an answer arrived, when the record changed while the questions
/// were open, and when the record no longer fits the store's naming rules.
/// [`CliError::IpcUnavailable`] when the store could not be read or written.
fn run_edit(store: &mut ServerStore, server: &str) -> Result<(), CliError> {
    let record = named(store, server)?.clone();
    store.forget(server);

    println!(
        "press Enter to keep the value in brackets. An empty secret keeps the saved one. \
         Ctrl-C stops without saving."
    );
    let name = ask_until("name", record.name.as_deref(), |typed| {
        if typed.is_empty() {
            Ok(())
        } else {
            free_name(store, typed)
        }
    })?;
    let address = ask_until("address", Some(&record.address), |typed| {
        free_address(store, typed)
    })?;
    let secret = ask_secret(Some(&record.secret))?;

    let moved = address != record.address;
    let question = if moved {
        "save the change anyway? The certificate at that address is pinned on the \
         first connection to it."
    } else {
        "save the change anyway?"
    };
    let held_pin = kept_pin(record.fingerprint.clone(), moved);
    let pinned = match check_server(&address, &secret, held_pin.as_deref(), question)? {
        Checked::Pinned(fingerprint) => Some(fingerprint),
        Checked::Unpinned => None,
        Checked::Discarded => {
            print!("{}", output::render_remote_discarded());
            return Ok(());
        }
    };

    let now = SystemTime::now();
    let updated = update_store(|disk| {
        let on_disk = record_if_unchanged(disk, server, &record)?;
        let updated = SavedServer {
            name: (!name.is_empty()).then_some(name),
            address,
            secret,
            last_used_at: if pinned.is_some() {
                Some(now)
            } else {
                on_disk.last_used_at
            },
            fingerprint: pinned.or(held_pin),
            added_at: on_disk.added_at,
        };
        place(disk, &updated, Some(server))?;
        Ok(updated)
    })?;
    print!("{}", output::render_remote_updated(&updated));
    Ok(())
}

/// The record `server` names in `store`, when it is still the `held` the
/// questions were answered against.
///
/// The name, the address, the secret and the fingerprint are compared. The
/// last-used time and the added time are not: a record another koshi only
/// dialled still passes.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `server` names no record, when it names more
/// than one, and when one of the four compared values changed.
fn record_if_unchanged(
    store: &ServerStore,
    server: &str,
    held: &SavedServer,
) -> Result<SavedServer, CliError> {
    let now_held = named(store, server)?;
    if now_held.name != held.name
        || now_held.address != held.address
        || now_held.secret != held.secret
        || now_held.fingerprint != held.fingerprint
    {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "{server} changed while the questions were open, so nothing was \
                 saved; run `koshi remote edit {server}` again"
            ),
        });
    }
    Ok(now_held.clone())
}

/// The fingerprint a record still holds once its address settled.
///
/// `held` is what it pinned before. The answer is `held` while `moved` is
/// false, and `None` while `moved` is true.
///
/// Example — a record pinning `aa…aa` whose address the user left alone keeps
/// `aa…aa`. The same record moved to another address keeps nothing.
fn kept_pin(held: Option<String>, moved: bool) -> Option<String> {
    if moved {
        None
    } else {
        held
    }
}

/// Put `record` in `store`, taking the place of the record `replacing` names.
///
/// The replaced record leaves before the checks, so `record` may keep its name
/// and its address. A refusal leaves `store` as it was. The store is not
/// written; the caller does that.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `replacing` names no record or more than
/// one, and when another record answers to `record`'s name or its address.
fn place(
    store: &mut ServerStore,
    record: &SavedServer,
    replacing: Option<&str>,
) -> Result<(), CliError> {
    let mut settled = store.clone();
    if let Some(server) = replacing {
        named(&settled, server)?;
        settled.forget(server);
    }
    if let Some(name) = record.name.as_deref() {
        free_name(&settled, name)?;
    }
    free_address(&settled, &record.address)?;
    settled
        .save(record.clone())
        .map_err(|taken| CliError::InvalidArgs {
            detail: taken.to_string(),
        })?;
    *store = settled;
    Ok(())
}

/// Ask for one value until `check` takes it, and return what it settled on.
///
/// `current` is printed in brackets, and an empty answer settles on it. With
/// no `current` an empty answer settles on the empty string, which `check`
/// answers for. Surrounding whitespace is trimmed. A `check` that refuses
/// prints its reason and the question is asked again.
///
/// Example — `ask_until("name", Some("work"), …)` prints `name [work]: `, and
/// pressing Enter settles on `work`.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer arrived.
fn ask_until(
    label: &str,
    current: Option<&str>,
    check: impl Fn(&str) -> Result<(), CliError>,
) -> Result<String, CliError> {
    let prompt = match current {
        Some(value) => format!("{label} [{value}]: "),
        None => format!("{label}: "),
    };
    loop {
        let typed = prompt_line(&prompt)?;
        let settled = if typed.is_empty() {
            current.unwrap_or_default()
        } else {
            &typed
        };
        match check(settled) {
            Ok(()) => return Ok(settled.to_string()),
            Err(error) => eprintln!("koshi: {error}"),
        }
    }
}

/// Ask for the secret to present to the server, without printing what is
/// typed.
///
/// `current` is the saved secret an empty answer keeps. With no `current` an
/// empty answer asks again.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer arrived.
fn ask_secret(current: Option<&ConnectionToken>) -> Result<ConnectionToken, CliError> {
    loop {
        let typed = prompt_secret("secret: ")?;
        if !typed.is_empty() {
            return Ok(ConnectionToken::new(typed));
        }
        match current {
            Some(secret) => return Ok(secret.clone()),
            None => eprintln!("koshi: a secret is needed; paste the one the grant handed out"),
        }
    }
}

/// Dial the server at `address` once to check that it admits `secret`, and ask
/// `question` when it does not.
///
/// `pinned` is the fingerprint the server must present, or `None` to take
/// whatever certificate it presents. The connection closes as soon as the
/// server admits the secret. No session is listed, and a server serving no
/// session passes this check.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer to `question` arrived.
fn check_server(
    address: &str,
    secret: &ConnectionToken,
    pinned: Option<&str>,
    question: &str,
) -> Result<Checked, CliError> {
    println!("checking {address} …");
    match remote_client::connect(address, secret, pinned, DIAL_WAIT, Some(REPLY_WAIT)) {
        Ok(link) => Ok(Checked::Pinned(link.fingerprint)),
        Err(error) => {
            eprintln!("koshi: {}", CliError::from(error));
            if confirmed(question)? {
                Ok(Checked::Unpinned)
            } else {
                Ok(Checked::Discarded)
            }
        }
    }
}

/// Ask `question` and answer whether the user said yes.
///
/// `y` and `yes`, in any letter case, are yes. Every other answer, an empty
/// one included, is no.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before an answer arrived.
fn confirmed(question: &str) -> Result<bool, CliError> {
    let typed = prompt_line(&format!("{question} [y/N]: "))?;
    Ok(matches!(typed.to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// `Ok(())` when `name` is a word this store can give to a record.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `name` is empty, when it has the shape of an
/// address, and when another record already answers to it by its own name or
/// its own address.
fn free_name(store: &ServerStore, name: &str) -> Result<(), CliError> {
    if name.is_empty() {
        return Err(CliError::InvalidArgs {
            detail: "a name is needed, such as work".to_string(),
        });
    }
    check_name_shape(name)?;
    free_word(store, name, "run `koshi remote list` and pick another name")
}

/// `Ok(())` when `address` is an address this store can give to a record.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `address` is not `host:port`, and when
/// another record already answers to it by its own name or its own address.
fn free_address(store: &ServerStore, address: &str) -> Result<(), CliError> {
    if !looks_like_address(address) {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "an address is host:port, such as laptop.local:7654, and {address} is not"
            ),
        });
    }
    free_word(
        store,
        address,
        &format!("run `koshi remote edit {address}` to change it"),
    )
}

/// `Ok(())` when no record in `store` answers to `word`.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming `word` and ending in `remedy` when a
/// record answers to it by its own name or its own address.
fn free_word(store: &ServerStore, word: &str, remedy: &str) -> Result<(), CliError> {
    match store.find(word) {
        Lookup::NotSaved => Ok(()),
        Lookup::Saved(_) | Lookup::Ambiguous => Err(CliError::InvalidArgs {
            detail: format!("{word} already answers for a saved server; {remedy}"),
        }),
    }
}

/// The one server `server` names.
///
/// # Errors
/// [`CliError::InvalidArgs`] when nothing is saved under that word, and a
/// different [`CliError::InvalidArgs`] when more than one record answers to
/// it.
fn named<'a>(store: &'a ServerStore, server: &str) -> Result<&'a SavedServer, CliError> {
    match store.find(server) {
        Lookup::Saved(record) => Ok(record),
        Lookup::NotSaved => Err(not_saved(server)),
        Lookup::Ambiguous => Err(CliError::InvalidArgs {
            detail: format!(
                "{server} is the name of one saved server and the address of another; \
                 run `koshi remote list` and name the one you mean"
            ),
        }),
    }
}

/// A `SERVER` argument that matches neither a saved name nor a saved address.
fn not_saved(server: &str) -> CliError {
    CliError::InvalidArgs {
        detail: format!("no saved server is named {server}; run `koshi remote list`"),
    }
}
