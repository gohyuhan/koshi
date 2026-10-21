//! Tests for how an [`EndingNotice`] holds the session's last frame and counts
//! the client writing threads.

use super::*;

#[test]
fn an_ending_notice_starts_empty_and_holds_the_ending_it_was_raised_with() {
    for session_ending in [SessionEnding::Quit, SessionEnding::Restarting] {
        let ending_notice = EndingNotice::default();
        assert_eq!(ending_notice.get_session_ending(), None);
        ending_notice.raise_session_ending(session_ending);
        assert_eq!(ending_notice.get_session_ending(), Some(session_ending));
        ending_notice.raise_session_ending(session_ending);
        assert_eq!(ending_notice.get_session_ending(), Some(session_ending));
    }
}

#[test]
fn an_ending_notice_keeps_the_first_ending_when_a_second_one_is_raised() {
    let ending_notice = EndingNotice::default();

    ending_notice.raise_session_ending(SessionEnding::Restarting);
    ending_notice.raise_session_ending(SessionEnding::Quit);

    assert_eq!(
        ending_notice.get_session_ending(),
        Some(SessionEnding::Restarting)
    );
}

#[test]
fn an_ending_notice_counts_every_writing_thread_from_start_to_end() {
    let ending_notice = EndingNotice::default();
    assert_eq!(ending_notice.wait_until_writers_end(Instant::now()), 0);

    ending_notice.record_writer_started();
    ending_notice.record_writer_started();
    assert_eq!(ending_notice.wait_until_writers_end(Instant::now()), 2);

    ending_notice.record_writer_ended();
    assert_eq!(ending_notice.wait_until_writers_end(Instant::now()), 1);

    ending_notice.record_writer_ended();
    assert_eq!(ending_notice.wait_until_writers_end(Instant::now()), 0);
}

#[test]
fn a_writing_thread_that_ends_during_the_wait_ends_the_wait_at_once() {
    let ending_notice = Arc::new(EndingNotice::default());
    ending_notice.record_writer_started();
    let writer_ending_notice = Arc::clone(&ending_notice);
    let writer_thread = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        writer_ending_notice.record_writer_ended();
    });

    let wait_started_at = Instant::now();
    let running_writer_count =
        ending_notice.wait_until_writers_end(wait_started_at + std::time::Duration::from_secs(3));
    let waited_duration = wait_started_at.elapsed();
    writer_thread.join().expect("the writing thread ends");

    assert_eq!(running_writer_count, 0);
    assert!(
        waited_duration < std::time::Duration::from_secs(1),
        "the wait must end when the writer ends, not at the deadline; took {waited_duration:?}"
    );
}

#[test]
fn writing_threads_sharing_one_ending_notice_all_count_into_it() {
    let ending_notice = Arc::new(EndingNotice::default());

    let writer_threads: Vec<_> = (0..8)
        .map(|_| {
            let ending_notice = Arc::clone(&ending_notice);
            std::thread::spawn(move || ending_notice.record_writer_started())
        })
        .collect();
    for writer_thread in writer_threads {
        writer_thread.join().expect("the counting thread finished");
    }

    assert_eq!(ending_notice.wait_until_writers_end(Instant::now()), 8);
}
