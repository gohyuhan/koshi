//! Lifecycle state machines for the session model: the typed states a tab —
//! and later a session — moves through from creation to teardown.
//!
//! Each lifecycle is a small enum. A tab is born `Creating`, becomes the
//! shown tab (`Active`) or a background one (`Inactive`), then winds down
//! through `Closing` to `Closed`. Modelling the stages as a type turns an
//! illegal move — reviving a closed tab, showing one mid-teardown — into a
//! transition-time error instead of a silent bug.
//!
//! The enums live here; the transition functions that police the legal moves
//! land with the operations that drive them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabLifecycle {
    Creating,
    Active,
    Inactive,
    Closing,
    Closed,
}
