//! What koshi reads from the machine it runs on, and the signals it sends to
//! the processes running there.
//!
//! [`process_tree`] reads the running processes and stops them.
//! [`host_addresses`] reads the network addresses another machine can connect
//! to. [`program_path`] reads the path of the koshi program this process runs.

pub mod host_addresses;
pub mod process_tree;
pub mod program_path;
