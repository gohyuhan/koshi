//! Tests for the process helpers.

use super::*;

/// A serving thread's SIGPIPE block must hold while the process-wide
/// disposition sits at its default, the state a running exec puts it in. The
/// raise is thread-directed, like the signal a write to a hung-up peer
/// raises; blocked, it stays pending, the thread runs on, and the pending
/// signal dies with the thread.
#[cfg(unix)]
#[test]
fn a_serving_threads_sigpipe_block_holds_under_the_default_disposition() {
    let survived = std::thread::spawn(|| {
        block_sigpipe_on_this_thread();
        let prior = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let raised = unsafe { libc::raise(libc::SIGPIPE) };
        unsafe { libc::signal(libc::SIGPIPE, prior) };
        raised == 0
    })
    .join()
    .expect("the thread survives the raised SIGPIPE");
    assert!(survived, "the raise itself reported an error");
}

/// A detached helper must survive the process that started it and must draw no
/// window. A transposed digit in either flag is a different flag, and
/// `std::process::Command` reports no creation flags, so the values are
/// checked here.
#[cfg(windows)]
#[test]
fn the_detach_flags_carry_their_win32_values() {
    assert_eq!(
        DETACHED_PROCESS, 0x0000_0008,
        "DETACHED_PROCESS is 8; another value is another flag"
    );
    assert_eq!(
        CREATE_NEW_PROCESS_GROUP, 0x0000_0200,
        "CREATE_NEW_PROCESS_GROUP is 512; another value is another flag"
    );
}
