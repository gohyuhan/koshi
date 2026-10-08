//! What koshi reads from the machine it runs on, the processes it starts there,
//! and the signals it sends to the processes running there.
//!
//! [`process_tree`] reads the running processes and stops them.
//! [`host_addresses`] reads the network addresses another machine can connect
//! to. [`program_path`] reads the path of the koshi program this process runs.
//! [`detached_process`] sets a command to start a process that outlives this
//! one. On Windows, `standard_handles` keeps the standard handles of this
//! process from passing to a child process by inheritance.

pub mod detached_process;
pub mod host_addresses;
pub mod process_tree;
pub mod program_path;
#[cfg(windows)]
pub mod standard_handles;
