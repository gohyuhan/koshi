//! Tests for the paint window: each rule that changes the window, the
//! reports it refuses, and links simulated on a fake clock — a slow link, a
//! long and fast link, a client that paints at 60 Hz through a flood, and a
//! client that never reports a paint.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::*;

/// A paint window driven on a fake clock: each measurement sends one update
/// and reports it painted.
struct WindowDriver {
    paint_window: PaintWindow,
    clock: Instant,
    written_byte_count: u64,
    next_update_sequence: u64,
}

impl WindowDriver {
    fn new() -> Self {
        WindowDriver {
            paint_window: PaintWindow::new(),
            clock: Instant::now(),
            written_byte_count: 0,
            next_update_sequence: 1,
        }
    }

    /// Send an update of `update_byte_count` bytes `idle_ms` after the last
    /// event, and return its number.
    fn send_update(&mut self, idle_ms: u64, update_byte_count: u64) -> u64 {
        self.clock += Duration::from_millis(idle_ms);
        self.written_byte_count += update_byte_count;
        let update_sequence = self.next_update_sequence;
        self.next_update_sequence += 1;
        self.paint_window.record_update_sent(
            update_sequence,
            self.written_byte_count,
            self.clock,
            self.clock,
        );
        update_sequence
    }

    /// Report update `update_sequence` painted `round_trip_ms` after the last
    /// event.
    fn report_update(&mut self, update_sequence: u64, round_trip_ms: u64) {
        self.clock += Duration::from_millis(round_trip_ms);
        self.paint_window
            .record_update_painted(PaintedUpdateReport {
                update_sequence,
                reported_at: self.clock,
            })
            .expect("the update was sent");
    }

    /// Send one update and report it painted `round_trip_ms` after the send.
    /// A filled update brings the bytes in flight to the window, and any other
    /// update is 1 byte.
    fn measure_round_trip(&mut self, round_trip_ms: u64, is_window_filled: bool) {
        let update_byte_count = if is_window_filled {
            self.paint_window.window_byte_count
        } else {
            1
        };
        let update_sequence = self.send_update(0, update_byte_count);
        self.report_update(update_sequence, round_trip_ms);
    }

    /// Measure `round_trip_ms` three times: one change of the window.
    fn measure_batch(&mut self, round_trip_ms: u64, is_window_filled: bool) {
        for _ in 0..MEASUREMENT_COUNT_PER_CHANGE {
            self.measure_round_trip(round_trip_ms, is_window_filled);
        }
    }

    /// Leave the doubling at a 50 ms base with a window of 80 KiB: 64 KiB
    /// doubles to 128 KiB, and 80 ms round trips scale it by 50 / 80.
    fn leave_doubling_at_eighty_kib(&mut self) {
        self.measure_batch(50, true);
        self.measure_batch(80, true);
        assert_eq!(self.paint_window.window_byte_count, 80 * 1024);
        assert!(!self.paint_window.is_doubling);
    }

    /// Leave the doubling at a 300 ms base with a window of 100 KiB: 64 KiB
    /// doubles to 128 KiB, and 384 ms round trips scale it by 300 / 384.
    fn leave_doubling_at_three_hundred_ms_base(&mut self) {
        self.measure_batch(300, true);
        self.measure_batch(384, true);
        assert_eq!(self.paint_window.window_byte_count, 100 * 1024);
        assert!(!self.paint_window.is_doubling);
    }
}

/// The update number of each update `paint_window` holds in flight, oldest
/// first.
fn list_in_flight_update_sequences(paint_window: &PaintWindow) -> Vec<u64> {
    paint_window
        .in_flight_updates
        .iter()
        .map(|in_flight_update| in_flight_update.update_sequence)
        .collect()
}

#[test]
fn before_the_first_paint_report_one_update_waits_for_a_report_at_most() {
    let mut paint_window = PaintWindow::new();
    let sent_at = Instant::now();

    assert!(paint_window.can_send_update(0));
    paint_window.record_update_sent(1, 100, sent_at, sent_at);
    assert!(!paint_window.can_send_update(100));

    paint_window
        .record_update_painted(PaintedUpdateReport {
            update_sequence: 1,
            reported_at: sent_at + Duration::from_millis(10),
        })
        .expect("update 1 was sent");
    assert!(paint_window.can_send_update(100));
    paint_window.record_update_sent(2, 200, sent_at, sent_at);
    assert!(paint_window.can_send_update(200));
    assert!(paint_window.can_send_update(100 + INITIAL_WINDOW_BYTE_COUNT - 1));
    assert!(!paint_window.can_send_update(100 + INITIAL_WINDOW_BYTE_COUNT));
}

#[test]
fn three_filled_round_trips_near_the_base_double_the_window() {
    let mut window_driver = WindowDriver::new();

    window_driver.measure_batch(50, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 128 * 1024);
    window_driver.measure_round_trip(74, true);
    window_driver.measure_round_trip(60, true);
    window_driver.measure_round_trip(70, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 256 * 1024);
    assert!(window_driver.paint_window.is_doubling);
}

#[test]
fn round_trips_of_updates_that_did_not_fill_the_window_do_not_grow_it() {
    let mut window_driver = WindowDriver::new();

    window_driver.measure_batch(50, false);
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        INITIAL_WINDOW_BYTE_COUNT
    );
    assert!(window_driver.paint_window.is_doubling);

    window_driver.leave_doubling_at_eighty_kib();
    window_driver.measure_batch(50, false);
    assert_eq!(window_driver.paint_window.window_byte_count, 80 * 1024);
}

#[test]
fn a_round_trip_over_25_ms_above_the_base_ends_the_doubling_and_scales_the_window() {
    let mut window_driver = WindowDriver::new();
    window_driver.measure_batch(50, true);

    window_driver.measure_round_trip(76, true);
    window_driver.measure_round_trip(90, false);
    window_driver.measure_round_trip(80, true);

    assert_eq!(
        window_driver.paint_window.window_byte_count,
        128 * 1024 * 50 / 76
    );
    assert!(!window_driver.paint_window.is_doubling);
}

#[test]
fn a_round_trip_25_ms_above_the_base_keeps_the_doubling() {
    let mut window_driver = WindowDriver::new();
    window_driver.measure_batch(50, true);

    window_driver.measure_batch(75, false);

    assert_eq!(window_driver.paint_window.window_byte_count, 128 * 1024);
    assert!(window_driver.paint_window.is_doubling);
}

#[test]
fn a_latency_spike_scales_the_window_and_shrinks_it_by_one_step() {
    let mut window_driver = WindowDriver::new();
    window_driver.measure_batch(50, true);

    window_driver.measure_batch(200, true);

    let scaled_window_byte_count = 128 * 1024 * 50 / 200;
    let window_step_byte_count = scaled_window_byte_count * 10 / 200;
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        scaled_window_byte_count - window_step_byte_count
    );
    assert!(!window_driver.paint_window.is_doubling);
}

#[test]
fn a_spike_needs_an_excess_over_half_the_base() {
    let mut window_driver = WindowDriver::new();
    window_driver.leave_doubling_at_three_hundred_ms_base();

    // 140 ms over a 300 ms base: over 100 ms, and not over 150 ms.
    window_driver.measure_batch(440, true);

    let window_step_byte_count = 100 * 1024 * 10 / 440;
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        100 * 1024 - window_step_byte_count
    );
}

#[test]
fn once_the_doubling_ends_the_excess_grows_holds_or_shrinks_the_window_by_steps() {
    let mut window_driver = WindowDriver::new();
    window_driver.leave_doubling_at_eighty_kib();

    // 4 ms over the base: two steps of 4 KiB.
    window_driver.measure_batch(54, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 88 * 1024);
    // 5 ms over the base: one step.
    window_driver.measure_batch(55, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 92 * 1024);
    // 24 ms over the base: one step.
    window_driver.measure_batch(74, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 96 * 1024);
    // 25 ms and 50 ms over the base: no change.
    window_driver.measure_batch(75, true);
    window_driver.measure_batch(100, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 96 * 1024);
    // 51 ms over the base: one step less.
    window_driver.measure_batch(101, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 92 * 1024);
    // 4 ms over the base, from updates that did not fill the window: no
    // change.
    window_driver.measure_batch(54, false);
    assert_eq!(window_driver.paint_window.window_byte_count, 92 * 1024);
}

#[test]
fn a_step_on_a_slow_link_is_the_bytes_that_10_ms_carry() {
    let mut window_driver = WindowDriver::new();
    window_driver.leave_doubling_at_three_hundred_ms_base();

    window_driver.measure_batch(320, true);

    assert_eq!(
        window_driver.paint_window.window_byte_count,
        100 * 1024 + 100 * 1024 * 10 / 320
    );
}

#[test]
fn the_window_stays_between_4_kib_and_4_mib() {
    let mut window_driver = WindowDriver::new();
    for _ in 0..7 {
        window_driver.measure_batch(10, true);
    }
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        MAXIMUM_WINDOW_BYTE_COUNT
    );

    window_driver.measure_batch(100_000, true);
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        MINIMUM_WINDOW_BYTE_COUNT
    );
}

#[test]
fn a_report_of_an_update_sent_before_the_last_change_is_no_measurement() {
    let mut paint_window = PaintWindow::new();
    let sent_at = Instant::now();
    for update_sequence in 1..=5 {
        paint_window.record_update_sent(update_sequence, update_sequence, sent_at, sent_at);
    }
    for update_sequence in 1..=3 {
        paint_window
            .record_update_painted(PaintedUpdateReport {
                update_sequence,
                reported_at: sent_at + Duration::from_millis(50),
            })
            .expect("the update was sent");
    }
    assert_eq!(paint_window.changed_through_update_sequence, 5);
    assert_eq!(paint_window.measurement_batch.measurement_count, 0);

    for update_sequence in 4..=5 {
        paint_window
            .record_update_painted(PaintedUpdateReport {
                update_sequence,
                reported_at: sent_at + Duration::from_millis(60),
            })
            .expect("the update was sent");
    }
    assert_eq!(paint_window.measurement_batch.measurement_count, 0);
    assert_eq!(
        paint_window.base_round_trip,
        Some(Duration::from_millis(50))
    );

    paint_window.record_update_sent(6, 6, sent_at, sent_at);
    paint_window
        .record_update_painted(PaintedUpdateReport {
            update_sequence: 6,
            reported_at: sent_at + Duration::from_millis(40),
        })
        .expect("update 6 was sent");
    assert_eq!(paint_window.measurement_batch.measurement_count, 1);
    assert_eq!(
        paint_window.base_round_trip,
        Some(Duration::from_millis(40))
    );
}

#[test]
fn an_update_after_an_idle_link_starts_the_window_over() {
    let mut window_driver = WindowDriver::new();
    window_driver.measure_batch(50, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 128 * 1024);

    // 100 ms with no update in flight is not idle.
    let update_sequence = window_driver.send_update(100, 1);
    assert_eq!(window_driver.paint_window.window_byte_count, 128 * 1024);
    window_driver.report_update(update_sequence, 50);
    assert_eq!(
        window_driver
            .paint_window
            .measurement_batch
            .measurement_count,
        1
    );

    // 101 ms is idle: over 100 ms, and over two 50 ms base round trips.
    window_driver.send_update(101, 1);
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        INITIAL_WINDOW_BYTE_COUNT
    );
    assert_eq!(window_driver.paint_window.base_round_trip, None);
    assert_eq!(
        window_driver
            .paint_window
            .measurement_batch
            .measurement_count,
        0
    );
    assert!(window_driver.paint_window.is_doubling);
}

#[test]
fn a_gap_of_100_ms_is_not_idle_on_a_link_with_a_short_base() {
    let mut window_driver = WindowDriver::new();
    window_driver.measure_batch(10, true);
    assert_eq!(window_driver.paint_window.window_byte_count, 128 * 1024);

    // 100 ms is over two 10 ms base round trips, and not over 100 ms.
    window_driver.send_update(100, 1);

    assert_eq!(window_driver.paint_window.window_byte_count, 128 * 1024);
    assert_eq!(
        window_driver.paint_window.base_round_trip,
        Some(Duration::from_millis(10))
    );
    assert!(window_driver.paint_window.is_doubling);
}

#[test]
fn an_idle_link_needs_two_base_round_trips_and_keeps_a_window_under_64_kib() {
    let mut window_driver = WindowDriver::new();
    window_driver.measure_batch(300, true);
    // 660 ms over the base: a spike to 128 KiB × 300 / 960 = 40 KiB, then one
    // step less.
    window_driver.measure_batch(960, true);
    let spiked_window_byte_count = 40 * 1024 - 40 * 1024 * 10 / 960;
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        spiked_window_byte_count
    );

    // 600 ms is not over two 300 ms base round trips.
    let update_sequence = window_driver.send_update(600, 1);
    assert_eq!(
        window_driver.paint_window.base_round_trip,
        Some(Duration::from_millis(300))
    );
    assert!(!window_driver.paint_window.is_doubling);
    window_driver.report_update(update_sequence, 300);

    window_driver.send_update(601, 1);
    assert_eq!(
        window_driver.paint_window.window_byte_count,
        spiked_window_byte_count
    );
    assert_eq!(window_driver.paint_window.base_round_trip, None);
    assert!(window_driver.paint_window.is_doubling);
}

#[test]
fn a_report_in_the_instant_of_its_send_counts_as_a_1_ms_round_trip() {
    let mut window_driver = WindowDriver::new();
    window_driver.leave_doubling_at_eighty_kib();

    window_driver.measure_batch(0, true);

    // A 1 ms round trip on a 1 ms base: two steps of 4 KiB, the most a step
    // adds.
    assert_eq!(
        window_driver.paint_window.base_round_trip,
        Some(MINIMUM_ROUND_TRIP_DURATION)
    );
    assert_eq!(window_driver.paint_window.window_byte_count, 88 * 1024);
}

#[test]
fn the_lowest_round_trip_of_a_filled_update_decides_the_growth() {
    let mut window_driver = WindowDriver::new();
    window_driver.leave_doubling_at_eighty_kib();

    // Filled round trips of 52 ms and 80 ms over a 50 ms base: the 52 ms one
    // is under 5 ms above the base, so the window grows by two steps.
    window_driver.measure_round_trip(52, true);
    window_driver.measure_round_trip(80, true);
    window_driver.measure_round_trip(51, false);

    assert_eq!(window_driver.paint_window.window_byte_count, 88 * 1024);
}

#[test]
fn a_report_of_an_unsent_update_is_refused_and_changes_nothing() {
    let mut paint_window = PaintWindow::new();
    let sent_at = Instant::now();

    assert_eq!(
        paint_window.record_update_painted(PaintedUpdateReport {
            update_sequence: 1,
            reported_at: sent_at,
        }),
        Err(PaintReportError::UnsentUpdate {
            reported_update_sequence: 1,
            last_sent_update_sequence: 0,
        })
    );

    paint_window.record_update_sent(1, 100, sent_at, sent_at);
    assert_eq!(
        paint_window.record_update_painted(PaintedUpdateReport {
            update_sequence: 2,
            reported_at: sent_at,
        }),
        Err(PaintReportError::UnsentUpdate {
            reported_update_sequence: 2,
            last_sent_update_sequence: 1,
        })
    );
    assert_eq!(list_in_flight_update_sequences(&paint_window), vec![1]);
    assert!(!paint_window.has_paint_report);
    assert_eq!(
        PaintReportError::UnsentUpdate {
            reported_update_sequence: 2,
            last_sent_update_sequence: 1,
        }
        .to_string(),
        "the client reported painted update 2, and the last update sent is 1"
    );
}

#[test]
fn a_report_frees_its_update_and_every_earlier_one_and_an_older_report_changes_nothing() {
    let mut paint_window = PaintWindow::new();
    let sent_at = Instant::now();
    for update_sequence in 1..=3 {
        paint_window.record_update_sent(update_sequence, update_sequence * 100, sent_at, sent_at);
    }

    paint_window
        .record_update_painted(PaintedUpdateReport {
            update_sequence: 2,
            reported_at: sent_at + Duration::from_millis(20),
        })
        .expect("update 2 was sent");
    assert_eq!(paint_window.painted_byte_position, 200);
    assert_eq!(list_in_flight_update_sequences(&paint_window), vec![3]);
    assert_eq!(paint_window.measurement_batch.measurement_count, 1);

    for update_sequence in [1, 2] {
        paint_window
            .record_update_painted(PaintedUpdateReport {
                update_sequence,
                reported_at: sent_at + Duration::from_millis(30),
            })
            .expect("the update was sent");
    }
    assert_eq!(paint_window.painted_byte_position, 200);
    assert_eq!(list_in_flight_update_sequences(&paint_window), vec![3]);
    assert_eq!(paint_window.measurement_batch.measurement_count, 1);
    assert_eq!(paint_window.flight_emptied_at, None);
}

/// A session that makes new screens, the writing thread that sends them
/// through a [`PaintWindow`], a link, and a client, on a fake clock.
struct LinkSimulation {
    /// The bytes the link carries in one second.
    link_byte_count_per_second: u64,
    /// The time from one end of the link to the other, past the time the
    /// bytes take on the link.
    one_way_delay: Duration,
    update_byte_count: u64,
    /// The time between two new screens.
    update_interval: Duration,
    /// How long the session makes new screens.
    flood_duration: Duration,
    /// The time between two paints of the client, or `None` for a client that
    /// paints as each update arrives.
    paint_interval: Option<Duration>,
    /// Whether the client reports its paints.
    is_reporting: bool,
    /// How long the simulation runs.
    run_duration: Duration,
}

/// What happened to one screen of the session, at times since the start.
#[derive(Debug, Clone, Default)]
struct SimulatedScreen {
    produced_at: Duration,
    /// When the writing thread sent it, or `None` when a newer screen took
    /// its place first.
    sent_at: Option<Duration>,
    /// When its first byte went on the link.
    transmission_started_at: Option<Duration>,
    /// When the client painted it.
    painted_at: Option<Duration>,
}

/// Every screen of a run, and the screen that waited to go out at its end.
struct SimulationOutcome {
    screens: Vec<SimulatedScreen>,
    waiting_screen_index: Option<usize>,
}

/// One event of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SimulatedEvent {
    ScreenProduced,
    UpdateArrived { update_sequence: u64 },
    ClientPainted,
    ReportArrived { update_sequence: u64 },
}

/// The events of a run in time order, and in the order they were added at
/// the same time.
#[derive(Default)]
struct EventSchedule {
    events: BinaryHeap<Reverse<(Duration, u64, SimulatedEvent)>>,
    added_event_count: u64,
}

impl EventSchedule {
    fn add_event(&mut self, event_at: Duration, simulated_event: SimulatedEvent) {
        self.events
            .push(Reverse((event_at, self.added_event_count, simulated_event)));
        self.added_event_count += 1;
    }

    fn take_next_event(&mut self) -> Option<(Duration, SimulatedEvent)> {
        self.events
            .pop()
            .map(|Reverse((event_at, _, simulated_event))| (event_at, simulated_event))
    }
}

/// Run `link_simulation`. A new screen takes the place of the screen that
/// waits to go out. The writing thread sends the waiting screen as soon as
/// the paint window lets it. The link carries one update at a time, in the
/// order sent. The client applies every update that arrived, paints the last
/// one, and reports it.
fn run_link_simulation(link_simulation: &LinkSimulation) -> SimulationOutcome {
    let clock_start = Instant::now();
    let transmission_duration = Duration::from_micros(
        link_simulation.update_byte_count * 1_000_000 / link_simulation.link_byte_count_per_second,
    );
    let mut paint_window = PaintWindow::new();
    let mut event_schedule = EventSchedule::default();
    let mut screens: Vec<SimulatedScreen> = Vec::new();
    let mut screen_index_by_update_sequence: Vec<usize> = Vec::new();
    let mut waiting_screen_index: Option<usize> = None;
    let mut written_byte_count = 0;
    let mut link_free_at = Duration::ZERO;
    let mut arrived_update_sequence = 0;
    let mut painted_update_sequence = 0;
    event_schedule.add_event(Duration::ZERO, SimulatedEvent::ScreenProduced);
    if let Some(paint_interval) = link_simulation.paint_interval {
        event_schedule.add_event(paint_interval, SimulatedEvent::ClientPainted);
    }
    while let Some((event_at, simulated_event)) = event_schedule.take_next_event() {
        if event_at > link_simulation.run_duration {
            break;
        }
        let mut should_client_paint = false;
        match simulated_event {
            SimulatedEvent::ScreenProduced => {
                screens.push(SimulatedScreen {
                    produced_at: event_at,
                    ..SimulatedScreen::default()
                });
                waiting_screen_index = Some(screens.len() - 1);
                let next_screen_at = event_at + link_simulation.update_interval;
                if next_screen_at < link_simulation.flood_duration {
                    event_schedule.add_event(next_screen_at, SimulatedEvent::ScreenProduced);
                }
            }
            SimulatedEvent::UpdateArrived { update_sequence } => {
                arrived_update_sequence = update_sequence;
                should_client_paint = link_simulation.paint_interval.is_none();
            }
            SimulatedEvent::ClientPainted => {
                should_client_paint = true;
                if let Some(paint_interval) = link_simulation.paint_interval {
                    event_schedule
                        .add_event(event_at + paint_interval, SimulatedEvent::ClientPainted);
                }
            }
            SimulatedEvent::ReportArrived { update_sequence } => {
                paint_window
                    .record_update_painted(PaintedUpdateReport {
                        update_sequence,
                        reported_at: clock_start + event_at,
                    })
                    .expect("the client reports updates that were sent");
            }
        }
        if should_client_paint && arrived_update_sequence > painted_update_sequence {
            painted_update_sequence = arrived_update_sequence;
            let screen_index =
                screen_index_by_update_sequence[usize::try_from(painted_update_sequence - 1)
                    .expect("an update sequence fits a usize")];
            screens[screen_index].painted_at = Some(event_at);
            if link_simulation.is_reporting {
                event_schedule.add_event(
                    event_at + link_simulation.one_way_delay,
                    SimulatedEvent::ReportArrived {
                        update_sequence: painted_update_sequence,
                    },
                );
            }
        }
        let Some(screen_index) = waiting_screen_index else {
            continue;
        };
        if !paint_window.can_send_update(written_byte_count) {
            continue;
        }
        waiting_screen_index = None;
        screen_index_by_update_sequence.push(screen_index);
        let update_sequence = u64::try_from(screen_index_by_update_sequence.len())
            .expect("an update count fits a u64");
        written_byte_count += link_simulation.update_byte_count;
        paint_window.record_update_sent(
            update_sequence,
            written_byte_count,
            clock_start + event_at,
            clock_start + event_at,
        );
        let transmission_started_at = link_free_at.max(event_at);
        link_free_at = transmission_started_at + transmission_duration;
        screens[screen_index].sent_at = Some(event_at);
        screens[screen_index].transmission_started_at = Some(transmission_started_at);
        event_schedule.add_event(
            link_free_at + link_simulation.one_way_delay,
            SimulatedEvent::UpdateArrived { update_sequence },
        );
    }
    SimulationOutcome {
        screens,
        waiting_screen_index,
    }
}

#[test]
fn a_client_that_paints_every_16_ms_through_a_flood_paints_the_last_screen_after_it() {
    let flood_duration = Duration::from_secs(5);
    let simulation_outcome = run_link_simulation(&LinkSimulation {
        link_byte_count_per_second: 12_500_000,
        one_way_delay: Duration::from_millis(1),
        update_byte_count: 20_000,
        update_interval: Duration::from_millis(1),
        flood_duration,
        paint_interval: Some(Duration::from_millis(16)),
        is_reporting: true,
        run_duration: Duration::from_secs(6),
    });

    let last_screen_index = simulation_outcome.screens.len() - 1;
    assert_eq!(last_screen_index, 4_999);
    let last_painted_screen_index = simulation_outcome
        .screens
        .iter()
        .enumerate()
        .filter_map(|(screen_index, screen)| {
            screen
                .painted_at
                .map(|painted_at| (painted_at, screen_index))
        })
        .max()
        .map(|(_, screen_index)| screen_index);
    assert_eq!(last_painted_screen_index, Some(last_screen_index));
    let last_painted_at = simulation_outcome.screens[last_screen_index]
        .painted_at
        .expect("the last screen is painted");
    assert!(
        last_painted_at - flood_duration <= Duration::from_millis(100),
        "the last screen is painted {:?} after the flood",
        last_painted_at - flood_duration
    );
    assert_eq!(simulation_outcome.waiting_screen_index, None);
}

#[test]
fn a_1_mbit_link_with_a_50_ms_round_trip_queues_2_kb_updates_under_50_ms() {
    let settled_at = Duration::from_secs(3);
    let simulation_outcome = run_link_simulation(&LinkSimulation {
        link_byte_count_per_second: 125_000,
        one_way_delay: Duration::from_millis(25),
        update_byte_count: 2_000,
        update_interval: Duration::from_millis(8),
        flood_duration: Duration::from_secs(10),
        paint_interval: None,
        is_reporting: true,
        run_duration: Duration::from_secs(10),
    });

    let settled_queueing_delays: Vec<Duration> = simulation_outcome
        .screens
        .iter()
        .filter_map(|screen| {
            let sent_at = screen.sent_at?;
            (sent_at >= settled_at).then(|| {
                screen
                    .transmission_started_at
                    .expect("a sent screen goes on the link")
                    - sent_at
            })
        })
        .collect();
    let longest_queueing_delay = settled_queueing_delays
        .iter()
        .max()
        .expect("updates go out after the window settles");
    assert!(
        *longest_queueing_delay < Duration::from_millis(50),
        "an update waited {longest_queueing_delay:?} on the link"
    );
    // The link carries 7 s × 125 000 bytes, 437 updates of 2 000 bytes.
    assert!(
        settled_queueing_delays.len() >= 420,
        "{} updates went out in 7 s",
        settled_queueing_delays.len()
    );
}

#[test]
fn a_1_mbit_link_with_a_50_ms_round_trip_queues_one_40_kb_update_at_most() {
    let simulation_outcome = run_link_simulation(&LinkSimulation {
        link_byte_count_per_second: 125_000,
        one_way_delay: Duration::from_millis(25),
        update_byte_count: 40_000,
        update_interval: Duration::from_millis(8),
        flood_duration: Duration::from_secs(10),
        paint_interval: None,
        is_reporting: true,
        run_duration: Duration::from_secs(10),
    });

    let sent_screens: Vec<&SimulatedScreen> = simulation_outcome
        .screens
        .iter()
        .filter(|screen| screen.sent_at.is_some())
        .collect();
    let most_queued_update_count = sent_screens
        .iter()
        .map(|sent_screen| {
            let sent_at = sent_screen.sent_at.expect("the screen was sent");
            sent_screens
                .iter()
                .filter(|other_screen| {
                    other_screen.sent_at.expect("the screen was sent") <= sent_at
                        && other_screen
                            .transmission_started_at
                            .expect("a sent screen goes on the link")
                            > sent_at
                })
                .count()
        })
        .max();
    assert_eq!(most_queued_update_count, Some(1));
    // The link carries 10 s × 125 000 bytes, 31 updates of 40 000 bytes.
    assert!(
        sent_screens.len() >= 25,
        "{} updates went out in 10 s",
        sent_screens.len()
    );
}

#[test]
fn a_1_gbit_link_with_a_150_ms_round_trip_carries_every_update() {
    let simulation_outcome = run_link_simulation(&LinkSimulation {
        link_byte_count_per_second: 125_000_000,
        one_way_delay: Duration::from_millis(75),
        update_byte_count: 64 * 1024,
        update_interval: Duration::from_millis(8),
        flood_duration: Duration::from_secs(10),
        paint_interval: None,
        is_reporting: true,
        run_duration: Duration::from_secs(10),
    });

    let settled_screens: Vec<&SimulatedScreen> = simulation_outcome
        .screens
        .iter()
        .filter(|screen| screen.produced_at >= Duration::from_secs(3))
        .collect();
    assert_eq!(settled_screens.len(), 875);
    assert!(settled_screens
        .iter()
        .all(|screen| screen.sent_at == Some(screen.produced_at)));
    let painted_screen_count = simulation_outcome
        .screens
        .iter()
        .filter(|screen| {
            screen.painted_at.is_some_and(|painted_at| {
                painted_at >= Duration::from_secs(4) && painted_at < Duration::from_secs(9)
            })
        })
        .count();
    assert_eq!(painted_screen_count, 625);
}

#[test]
fn a_client_that_never_reports_a_paint_receives_one_update() {
    let simulation_outcome = run_link_simulation(&LinkSimulation {
        link_byte_count_per_second: 125_000_000,
        one_way_delay: Duration::from_millis(1),
        update_byte_count: 2_000,
        update_interval: Duration::from_millis(8),
        flood_duration: Duration::from_secs(1),
        paint_interval: None,
        is_reporting: false,
        run_duration: Duration::from_secs(2),
    });

    assert_eq!(simulation_outcome.screens.len(), 125);
    assert_eq!(
        simulation_outcome
            .screens
            .iter()
            .filter(|screen| screen.sent_at.is_some())
            .count(),
        1
    );
    assert_eq!(simulation_outcome.screens[0].sent_at, Some(Duration::ZERO));
    assert_eq!(simulation_outcome.waiting_screen_index, Some(124));
}
