//! The thread that writes the last-used time of each admitted remote grant
//! into the token store.
//!
//! An admission reads the store, then hands the use — the hash of the
//! presented secret and the clock reading — to
//! [`TokenUseWriter::record_token_use`], which never blocks. The writer thread
//! takes every use that waits, then holds the store lock while it reads the
//! store, stamps each use with [`TokenStore::update_token_last_used`], drops
//! the grants that stopped working
//! [`TOKEN_RECORD_RETENTION_DURATION`](koshi_ipc::remote_tokens::TOKEN_RECORD_RETENTION_DURATION)
//! or longer ago, and writes the store when a record changed. A write that
//! fails keeps its uses, and the next use writes them again. Uses still kept
//! when the writer finishes are not written, and a warning gives their count.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

use koshi_ipc::remote_tokens::TokenStore;
use koshi_observability::repeated_warning::WarningRateLimiter;

use super::acquire_token_store_lock;

/// One admission's use of a grant.
struct TokenUse {
    /// The sha256 of the presented secret, as the store holds it.
    token_hash: String,
    /// The clock reading the admission took.
    used_at: SystemTime,
}

/// The running writer thread, and the sender that hands it uses.
pub(super) struct TokenUseWriter {
    /// Hands one use to the writer thread. Dropping it ends the thread once
    /// every waiting use has had one write.
    token_use_sender: Sender<TokenUse>,
    /// The writer thread, joined by [`TokenUseWriter::finish`].
    writer_thread: JoinHandle<()>,
}

impl TokenUseWriter {
    /// Start the writer thread for the store at `token_store_path`.
    ///
    /// Returns `None`, and logs an error, when the thread cannot start: no
    /// last-used time is written then, and admissions go on.
    pub(super) fn start(token_store_path: PathBuf) -> Option<TokenUseWriter> {
        let (token_use_sender, token_use_receiver) = mpsc::channel();
        let spawn_result = std::thread::Builder::new()
            .name("koshi-token-use-writer".to_string())
            .spawn(move || run_token_use_writer(&token_store_path, &token_use_receiver));
        match spawn_result {
            Ok(writer_thread) => Some(TokenUseWriter {
                token_use_sender,
                writer_thread,
            }),
            Err(spawn_error) => {
                tracing::error!(
                    %spawn_error,
                    "the remote token use writer could not start; last-used times are not written"
                );
                None
            }
        }
    }

    /// Hand the writer the use of the grant holding `token_hash` at
    /// `used_at`. Returns at once.
    pub(super) fn record_token_use(&self, token_hash: String, used_at: SystemTime) {
        let _ = self.token_use_sender.send(TokenUse {
            token_hash,
            used_at,
        });
    }

    /// Stop the writer thread once every use handed to it has had one write,
    /// and wait for it to end.
    pub(super) fn finish(self) {
        drop(self.token_use_sender);
        let _ = self.writer_thread.join();
    }
}

/// Write the uses `token_use_receiver` hands over into the store at
/// `token_store_path`, until every sender is dropped.
///
/// Each pass takes every use that waits and keeps the latest time per hash,
/// then writes them through [`write_kept_token_uses`]. Once every sender is
/// dropped and every waiting use has had its pass, the thread ends. Uses that a
/// failed pass kept are dropped then, with a warning that gives their count.
fn run_token_use_writer(token_store_path: &Path, token_use_receiver: &Receiver<TokenUse>) {
    let mut used_at_by_token_hash: HashMap<String, SystemTime> = HashMap::new();
    let mut write_failure_warning = WarningRateLimiter::new();
    while let Ok(first_token_use) = token_use_receiver.recv() {
        for token_use in std::iter::once(first_token_use).chain(token_use_receiver.try_iter()) {
            let used_at = used_at_by_token_hash
                .entry(token_use.token_hash)
                .or_insert(token_use.used_at);
            *used_at = (*used_at).max(token_use.used_at);
        }
        write_kept_token_uses(
            token_store_path,
            &mut used_at_by_token_hash,
            &mut write_failure_warning,
        );
    }
    if !used_at_by_token_hash.is_empty() {
        tracing::warn!(
            dropped_use_count = used_at_by_token_hash.len(),
            "the remote token use writer stopped with last-used times it could not write"
        );
    }
}

/// Write `used_at_by_token_hash` through [`write_token_uses`] and empty it
/// when the write succeeds. A failed write keeps every use and logs a
/// warning when `write_failure_warning` says one is due.
fn write_kept_token_uses(
    token_store_path: &Path,
    used_at_by_token_hash: &mut HashMap<String, SystemTime>,
    write_failure_warning: &mut WarningRateLimiter,
) {
    match write_token_uses(token_store_path, used_at_by_token_hash) {
        Ok(()) => used_at_by_token_hash.clear(),
        Err(error_detail) => {
            if write_failure_warning.record_warning_if_due(Instant::now()) {
                tracing::warn!(
                    %error_detail,
                    kept_use_count = used_at_by_token_hash.len(),
                    "the last-used times of remote grants could not be written; the next use writes them again"
                );
            }
        }
    }
}

/// Stamp each use in `used_at_by_token_hash` on the store at
/// `token_store_path`, holding the store lock from the read to the write.
///
/// The lock is taken through [`acquire_token_store_lock`]. When a record
/// changed, the grants that stopped working
/// [`TOKEN_RECORD_RETENTION_DURATION`](koshi_ipc::remote_tokens::TOKEN_RECORD_RETENTION_DURATION)
/// or longer ago are dropped, and the store is written. A store where no record holds
/// any of the hashes is not written, and neither is a store file that does not
/// exist.
///
/// # Errors
/// The detail of the lock, read or write that failed. Nothing is written
/// then.
fn write_token_uses(
    token_store_path: &Path,
    used_at_by_token_hash: &HashMap<String, SystemTime>,
) -> Result<(), String> {
    let _store_lock_file = acquire_token_store_lock(token_store_path)?;
    let mut token_store = TokenStore::load_token_store_from_path(token_store_path)
        .map_err(|read_error| read_error.to_string())?;
    let mut has_changed = false;
    for (token_hash, used_at) in used_at_by_token_hash {
        has_changed |= token_store.update_token_last_used(token_hash, *used_at);
    }
    if !has_changed {
        return Ok(());
    }
    token_store.remove_ended_token_records(SystemTime::now());
    token_store
        .write_token_store_to_path(token_store_path)
        .map_err(|write_error| write_error.to_string())
}

#[cfg(test)]
mod tests;
