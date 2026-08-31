//! The bare `koshi` launch: ask the router for a new session and attach this
//! terminal to it.

use koshi_link::error::CliError;
use koshi_observability::logging::init_tracing;

/// Bare `koshi`: start or reuse the router, have it create a new session
/// server in this terminal's directory, and attach this terminal to it.
///
/// `profile` is handed to the router. A profile that will not launch falls
/// back to one shell inside the session server.
pub fn run(profile: Option<&str>) -> Result<(), CliError> {
    let runtime_dir = koshi_link::ipc_client::runtime_dir()?;
    // `koshi.kdl`'s `logging` section sets whether a log file is opened at
    // all, and at what level and format.
    let app = koshi_link::config::load_app_layer();
    // A `None` forced switch leaves who may reach the new session to that
    // session's own `koshi.kdl`; the interactive launch has no
    // `--allow-other-users` flag.
    let session_id = koshi_link::router_client::request_new_session(&runtime_dir, profile, None)?;
    let _ = init_tracing(koshi_link::config::logging_params(app.as_ref(), session_id));
    // Runs after the subscriber is installed, so its lines reach the log.
    // Creating the directory adds no `koshi.kdl`, so the layer read above sees
    // the same files either way.
    ensure_koshi_dirs();
    crate::attach::attach_session(&runtime_dir, session_id)
}

/// Create the config directory at the fixed per-platform path
/// `koshi_paths::config_dir` gives.
///
/// The caller installs the tracing subscriber before this runs, so every line
/// below reaches the log. No home directory, and a create that fails, each
/// warn; a directory that is ready logs at info.
fn ensure_koshi_dirs() {
    let Some(config) = koshi_paths::config_dir() else {
        tracing::warn!("no home directory found; skipping config directory setup");
        return;
    };
    match koshi_paths::ensure_dir(&config) {
        Ok(()) => tracing::info!(path = %config.display(), "config directory ready"),
        Err(error) => {
            tracing::warn!(path = %config.display(), %error, "could not create config directory");
        }
    }
}
