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
    assert_eq!(ending_notice.count_running_writers(), 0);

    ending_notice.record_writer_started();
    ending_notice.record_writer_started();
    assert_eq!(ending_notice.count_running_writers(), 2);

    ending_notice.record_writer_ended();
    assert_eq!(ending_notice.count_running_writers(), 1);

    ending_notice.record_writer_ended();
    assert_eq!(ending_notice.count_running_writers(), 0);
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

    assert_eq!(ending_notice.count_running_writers(), 8);
}
