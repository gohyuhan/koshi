//! The `koshi remote` commands: list the servers this machine has saved,
//! forget one, and replace the secret of one.
//!
//! Every verb reads the saved-server store on this machine, and the two that
//! change it write it back through koshi's atomic replace. Nothing here opens
//! a connection, so a server that is switched off is still forgotten and still
//! takes a fresh secret. A listing never prints a secret.

use std::path::PathBuf;

use koshi_ipc::remote_servers::{store_path, Lookup, SavedServer, ServerStore};

use crate::cli::RemoteCommand;
use crate::output;
use koshi_link::error::CliError;
use koshi_link::remote_client;

#[cfg(test)]
mod tests;

/// Run one `remote` verb against the saved-server store and print the
/// rendered answer.
///
/// A `SERVER` argument matching no saved record is [`CliError::InvalidArgs`]
/// naming the command that lists what is saved.
pub fn run(command: &RemoteCommand) -> Result<(), CliError> {
    let (path, mut store) = read_store()?;
    match command {
        RemoteCommand::List { format } => {
            print!("{}", output::render_remote_list(&store.records, *format));
            Ok(())
        }
        RemoteCommand::Forget { server } => {
            named(&store, server)?;
            let address = store.forget(server).ok_or_else(|| not_saved(server))?;
            store.write(&path).map_err(store_failed)?;
            print!("{}", output::render_remote_forget(&address));
            Ok(())
        }
        RemoteCommand::SetSecret { server } => {
            // Read first: the prompt names this address.
            let address = named(&store, server)?.address.clone();
            let secret = remote_client::secret_for(&address)?;
            store.set_secret(server, secret);
            store.write(&path).map_err(store_failed)?;
            print!("{}", output::render_remote_secret(&address));
            Ok(())
        }
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

/// The saved-server store and the path it came from. A machine with no data
/// directory, and one whose store has never been written, both read as no
/// saved servers.
fn read_store() -> Result<(PathBuf, ServerStore), CliError> {
    let data_dir = koshi_paths::data_dir().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no data directory found".to_string(),
    })?;
    let path = store_path(&data_dir);
    let store = ServerStore::read(&path).map_err(|error| CliError::IpcUnavailable {
        detail: error.to_string(),
    })?;
    Ok((path, store))
}

/// A `SERVER` argument that matches neither a saved name nor a saved address.
fn not_saved(server: &str) -> CliError {
    CliError::InvalidArgs {
        detail: format!("no saved server is named {server}; run `koshi remote list`"),
    }
}

/// A saved-server store that could not be written.
fn store_failed(error: koshi_ipc::error::IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}
