//! Notifications: the entries koshi posts to tell the person at a client
//! something, and the bounded queue each owner keeps them in.
//!
//! A [`NotificationQueue`] belongs to one [`NotificationQueueOwner`]: a client,
//! whose entries only that client shows, or a session, whose entries every
//! client attached to it shows. The queue assigns each entry its
//! [`NotificationId`], its [`NotificationSource`] and its `created_at`; the
//! caller supplies its [`NotificationSeverity`] and its text. Example: the
//! session posts `pane …a1b2 killed by signal 9` as an `Error`, and every
//! attached client shows `[Koshi] pane …a1b2 killed by signal 9`.

use std::collections::VecDeque;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::{ClientId, SessionId};
use crate::text::sanitize_text_to_byte_count;

/// The longest text one entry holds, in bytes: 4096. A longer text is cut at
/// the last character boundary that fits.
pub const MAX_NOTIFICATION_TEXT_BYTE_COUNT: usize = 4096;

/// The most entries one queue holds: 256. The next entry evicts the oldest.
pub const MAX_QUEUED_NOTIFICATION_COUNT: usize = 256;

/// How loud one entry is. The order runs from the quietest to the loudest:
/// `Info` < `Warning` < `Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NotificationSeverity {
    /// A fact the person may want to know.
    Info,
    /// Something did not happen as the person asked.
    Warning,
    /// Something failed.
    Error,
}

/// Who wrote an entry's text. The queue sets it; no caller supplies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationSource {
    /// Koshi itself. A client shows the entry under the label `[Koshi]`.
    Core,
}

/// The process that keeps a queue, and that the queue's entries reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum NotificationQueueOwner {
    /// One client's own queue: only that client shows its entries.
    Client(ClientId),
    /// A session's queue: every client attached to the session shows its
    /// entries.
    Session(SessionId),
}

/// One run of a queue owner: a new value each time an owner starts a queue
/// with no entries carried from an earlier run.
///
/// A session that replaces its process image keeps its epoch; a session
/// server that starts with no resume body, and every client process, start a
/// new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NotificationEpoch(Uuid);

impl NotificationEpoch {
    /// A new epoch, different from every other epoch.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for NotificationEpoch {
    fn default() -> Self {
        Self::new()
    }
}

/// Names one entry: its queue's owner, the owner's epoch, and the entry's
/// place in that epoch, counted from 0. No two entries of any two queues share
/// an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NotificationId {
    /// The queue's owner.
    pub owner: NotificationQueueOwner,
    /// The owner's epoch when the entry was posted.
    pub epoch: NotificationEpoch,
    /// The entry's place in the epoch: the first entry is 0, the next 1.
    pub sequence: u64,
}

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationEntry {
    /// The entry's id, assigned by its queue.
    pub id: NotificationId,
    /// Who wrote the text, assigned by its queue.
    pub source: NotificationSource,
    /// How loud the entry is.
    pub severity: NotificationSeverity,
    /// The text, at most [`MAX_NOTIFICATION_TEXT_BYTE_COUNT`] bytes, with no
    /// character that [`sanitize_text_to_byte_count`] removes.
    pub text: String,
    /// When the newest post merged into this entry arrived, on the clock of
    /// the queue's owner.
    pub created_at: SystemTime,
    /// How many posts merged into this entry, from 1. Saturates at
    /// [`u32::MAX`].
    pub repeat_count: u32,
}

/// A bounded queue of entries, oldest first, kept by one owner for one epoch.
///
/// The queue holds at most [`MAX_QUEUED_NOTIFICATION_COUNT`] entries. Each
/// post advances the queue's revision by 1. The sequences of the entries it
/// holds run without a gap, and the oldest entry's sequence counts the entries
/// the queue evicted for room: a queue whose oldest entry has sequence 40
/// evicted 40 entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationQueue {
    /// The process that keeps the queue.
    owner: NotificationQueueOwner,
    /// The owner's run that posted every entry.
    epoch: NotificationEpoch,
    /// How many posts the queue took in this epoch: 0 for a new queue.
    revision: u64,
    /// The entries, oldest first. Each entry's id names this queue's owner and
    /// epoch, and the sequences rise from oldest to newest.
    entries: VecDeque<NotificationEntry>,
}

/// Why a queue another process sent cannot be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationQueueError {
    /// The queue holds more than [`MAX_QUEUED_NOTIFICATION_COUNT`] entries.
    TooManyEntries {
        /// How many entries it holds.
        entry_count: usize,
    },
    /// An entry's id names another owner or another epoch than the queue.
    ForeignEntry {
        /// The entry's id.
        notification_id: NotificationId,
    },
}

impl std::fmt::Display for NotificationQueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotificationQueueError::TooManyEntries { entry_count } => write!(
                formatter,
                "the notification queue holds {entry_count} entries, more than \
                 {MAX_QUEUED_NOTIFICATION_COUNT}"
            ),
            NotificationQueueError::ForeignEntry { notification_id } => write!(
                formatter,
                "notification {} names another queue",
                notification_id.sequence
            ),
        }
    }
}

impl std::error::Error for NotificationQueueError {}

impl NotificationQueue {
    /// An empty queue for `owner`, in a new epoch, at revision 0.
    #[must_use]
    pub fn from_owner(owner: NotificationQueueOwner) -> Self {
        NotificationQueue {
            owner,
            epoch: NotificationEpoch::new(),
            revision: 0,
            entries: VecDeque::new(),
        }
    }

    /// The process that keeps the queue.
    #[must_use]
    pub fn get_owner(&self) -> NotificationQueueOwner {
        self.owner
    }

    /// The owner's run that posted every entry.
    #[must_use]
    pub fn get_epoch(&self) -> NotificationEpoch {
        self.epoch
    }

    /// How many posts the queue took in this epoch.
    #[must_use]
    pub fn get_revision(&self) -> u64 {
        self.revision
    }

    /// The entries, oldest first.
    pub fn list_entries(&self) -> impl Iterator<Item = &NotificationEntry> {
        self.entries.iter()
    }

    /// Post `text` from koshi itself at `severity`, arriving at `created_at`,
    /// and return the entry the post made or changed.
    ///
    /// `text` loses every character [`sanitize_text_to_byte_count`] removes
    /// and is cut to [`MAX_NOTIFICATION_TEXT_BYTE_COUNT`] bytes. When the
    /// newest entry has the same source, severity and text, the post merges
    /// into it: its `repeat_count` rises by 1 and its `created_at` becomes
    /// `created_at`, and no entry is added. Otherwise a new entry takes the
    /// next sequence, and a queue already holding
    /// [`MAX_QUEUED_NOTIFICATION_COUNT`] entries first evicts its oldest.
    /// Either way the revision rises by 1.
    ///
    /// Example: posting `a`, `a`, `b`, `a` gives the entries `a` ×2, `b`, `a`.
    pub fn post_core_entry(
        &mut self,
        severity: NotificationSeverity,
        text: &str,
        created_at: SystemTime,
    ) -> &NotificationEntry {
        let sanitized_text = sanitize_text_to_byte_count(text, MAX_NOTIFICATION_TEXT_BYTE_COUNT);
        self.revision += 1;
        let is_repeat = self.entries.back().is_some_and(|newest_entry| {
            newest_entry.source == NotificationSource::Core
                && newest_entry.severity == severity
                && newest_entry.text == sanitized_text
        });
        if is_repeat {
            let newest_entry = self.entries.back_mut().expect("the entry read above");
            newest_entry.repeat_count = newest_entry.repeat_count.saturating_add(1);
            newest_entry.created_at = created_at;
            return newest_entry;
        }
        let next_sequence = self
            .entries
            .back()
            .map_or(0, |newest_entry| newest_entry.id.sequence + 1);
        if self.entries.len() == MAX_QUEUED_NOTIFICATION_COUNT {
            self.entries.pop_front();
        }
        self.entries.push_back(NotificationEntry {
            id: NotificationId {
                owner: self.owner,
                epoch: self.epoch,
                sequence: next_sequence,
            },
            source: NotificationSource::Core,
            severity,
            text: sanitized_text,
            created_at,
            repeat_count: 1,
        });
        self.entries.back().expect("the entry pushed above")
    }

    /// Check a queue another process sent: it holds at most
    /// [`MAX_QUEUED_NOTIFICATION_COUNT`] entries, and every entry's id names
    /// this queue's owner and epoch.
    ///
    /// # Errors
    /// [`NotificationQueueError::TooManyEntries`] for a queue past the bound,
    /// and [`NotificationQueueError::ForeignEntry`] for the first entry whose
    /// id names another owner or epoch.
    pub fn validate_entries(&self) -> Result<(), NotificationQueueError> {
        if self.entries.len() > MAX_QUEUED_NOTIFICATION_COUNT {
            return Err(NotificationQueueError::TooManyEntries {
                entry_count: self.entries.len(),
            });
        }
        match self
            .entries
            .iter()
            .find(|entry| entry.id.owner != self.owner || entry.id.epoch != self.epoch)
        {
            Some(foreign_entry) => Err(NotificationQueueError::ForeignEntry {
                notification_id: foreign_entry.id,
            }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
