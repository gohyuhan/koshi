//! Redaction helpers: scrub user data before it reaches logs, debug dumps,
//! snapshots, or IPC watchers.

/// What replaces a hidden value in any text output. Every type that withholds
/// a secret prints this.
pub const REDACTED: &str = "***";

/// Hide a spawned child's arguments: element 0, the program name, passes
/// through; every element after it becomes `***`, whatever it holds. An empty
/// `argv` yields an empty `Vec`.
///
/// `["mysql", "-pHUNTER2"]` results in `["mysql", "***"]`.
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    argv.iter()
        .enumerate()
        .map(|(index, arg)| {
            if index == 0 {
                arg.clone()
            } else {
                REDACTED.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
