//! The byte window of one attached client's connection.
//!
//! The writing thread sends a painted update only while the bytes it wrote
//! after the last update the client reported painted stay below the window.
//! An update always goes out when no update waits for a paint report. Until
//! the client's first paint report, one update waits for a report at most.
//! Every frame counts toward the window: the updates, the image transfers that
//! follow them and the events between them.
//!
//! A measurement is the round trip of one reported update: from the moment
//! its last byte was written to the moment its paint report was read, and
//! 1 ms at least. The base is the lowest round trip measured. A round trip's
//! excess is how far it is above the base. Every 3 measurements of updates
//! sent after the last change, the window changes once. The measured round
//! trip is the lowest of those 3. The filled round trip is the lowest of
//! those whose update made the bytes in flight reach the window as it went
//! out, and there is none when no update did.
//!
//! 1. The window starts at 64 KiB, and doubles.
//! 2. A measured excess over 100 ms and over half the base multiplies the
//!    window by the base over the measured round trip. The window stops
//!    doubling.
//! 3. While the window doubles, a measured excess over 25 ms multiplies the
//!    window by the base over the measured round trip, and the window stops
//!    doubling. Otherwise a filled excess under 25 ms doubles the window.
//! 4. Once the window stops doubling, a measured excess over 50 ms shrinks it
//!    by one step. Otherwise a filled excess under 5 ms grows it by two steps,
//!    and a filled excess under 25 ms grows it by one step. A step is the
//!    bytes that 10 ms carry at the window over the measured round trip, and
//!    4 KiB at most.
//! 5. The window stays between 4 KiB and 4 MiB.
//! 6. An update that goes out after no update waited for a report for more
//!    than 100 ms and more than two base round trips starts over: a window
//!    over 64 KiB drops to 64 KiB, the base is cleared, and the window doubles.
//!
//! Example — a 1 Mbit/s link with a 50 ms round trip, and 2 KB updates: the
//! base is 66 ms, the time of one update on the link included. Three
//! measurements of 106 ms end the doubling and set the 64 KiB window to
//! 64 KiB × 66 / 106 = 39.8 KiB. The next three measure 375 ms: a 309 ms
//! excess over 100 ms and over 33 ms sets the window to 7.0 KiB, and then
//! shrinks it by the bytes 10 ms carry, 0.2 KiB.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use crate::runtime::delivery_queue::PaintedUpdateReport;

/// The window after the first paint report, and the window a start over
/// drops to: 64 KiB.
const INITIAL_WINDOW_BYTE_COUNT: u64 = 64 * 1024;

/// The smallest window: 4 KiB.
const MINIMUM_WINDOW_BYTE_COUNT: u64 = 4 * 1024;

/// The largest window: 4 MiB.
const MAXIMUM_WINDOW_BYTE_COUNT: u64 = 4 * 1024 * 1024;

/// The largest step: 4 KiB.
const MAXIMUM_WINDOW_STEP_BYTE_COUNT: u64 = 4 * 1024;

/// The time on the link whose bytes make one step: 10 ms.
const WINDOW_STEP_DURATION: Duration = Duration::from_millis(10);

/// How many measurements one change of the window reads: 3.
const MEASUREMENT_COUNT_PER_CHANGE: u32 = 3;

/// The shortest round trip a measurement records: 1 ms.
const MINIMUM_ROUND_TRIP_DURATION: Duration = Duration::from_millis(1);

/// The excess under which a filled round trip grows the window, and over
/// which a measured round trip ends the doubling: 25 ms.
const ROUND_TRIP_GROWTH_MARGIN_DURATION: Duration = Duration::from_millis(25);

/// The excess under which a filled round trip grows the window by two steps:
/// 5 ms.
const ROUND_TRIP_FAST_GROWTH_MARGIN_DURATION: Duration = Duration::from_millis(5);

/// The excess over which a measured round trip shrinks the window by one
/// step: 50 ms.
const ROUND_TRIP_SHRINK_MARGIN_DURATION: Duration = Duration::from_millis(50);

/// The excess over which a measured round trip is a latency spike: 100 ms.
const LATENCY_SPIKE_MARGIN_DURATION: Duration = Duration::from_millis(100);

/// The shortest time with no update waiting for a report that starts the
/// window over: 100 ms.
const MINIMUM_IDLE_DURATION: Duration = Duration::from_millis(100);

/// The byte window of one connection, and the updates that wait for a paint
/// report.
#[derive(Debug)]
pub(crate) struct PaintWindow {
    /// How many bytes may be in flight past the last reported update.
    window_byte_count: u64,
    /// Whether the window still doubles.
    is_doubling: bool,
    /// The lowest round trip measured since the last start over, or `None`
    /// before the first measurement.
    base_round_trip: Option<Duration>,
    /// The measurements that the next change reads.
    measurement_batch: MeasurementBatch,
    /// The last update sent when the window last changed. A report of this
    /// update or of an earlier one is no measurement for the next change.
    changed_through_update_sequence: u64,
    /// The updates sent and not yet reported painted, oldest first.
    in_flight_updates: VecDeque<InFlightUpdate>,
    /// The connection's written byte count at the end of the last update the
    /// client reported painted.
    painted_byte_position: u64,
    /// The number of the last update sent, or `0` before the first.
    last_sent_update_sequence: u64,
    /// Whether the client has reported a paint of an update in flight.
    has_paint_report: bool,
    /// When the last update that waited for a report was reported painted, or
    /// `None` before the first report.
    flight_emptied_at: Option<Instant>,
}

/// The measurements since the last change of the window.
#[derive(Debug, Default)]
struct MeasurementBatch {
    /// How many measurements the batch holds.
    measurement_count: u32,
    /// The lowest round trip of the batch.
    lowest_round_trip: Option<Duration>,
    /// The lowest round trip of an update that made the bytes in flight reach
    /// the window.
    lowest_filled_round_trip: Option<Duration>,
}

/// One update sent and not yet reported painted.
#[derive(Debug, Clone)]
struct InFlightUpdate {
    /// The update's number.
    update_sequence: u64,
    /// When its last byte was written.
    sent_at: Instant,
    /// The connection's written byte count after its last byte, the image
    /// transfers that follow it included.
    end_byte_position: u64,
    /// Whether the bytes in flight past the last reported update reached the
    /// window once it went out.
    is_window_filled: bool,
}

/// A paint report that the connection refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaintReportError {
    /// The report names an update that was not sent.
    UnsentUpdate {
        reported_update_sequence: u64,
        last_sent_update_sequence: u64,
    },
}

impl fmt::Display for PaintReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PaintReportError::UnsentUpdate {
                reported_update_sequence,
                last_sent_update_sequence,
            } => write!(
                formatter,
                "the client reported painted update {reported_update_sequence}, and the last \
                 update sent is {last_sent_update_sequence}"
            ),
        }
    }
}

impl std::error::Error for PaintReportError {}

impl PaintWindow {
    /// The window of a connection that has sent nothing: 64 KiB, doubling.
    pub(crate) fn new() -> Self {
        PaintWindow {
            window_byte_count: INITIAL_WINDOW_BYTE_COUNT,
            is_doubling: true,
            base_round_trip: None,
            measurement_batch: MeasurementBatch::default(),
            changed_through_update_sequence: 0,
            in_flight_updates: VecDeque::new(),
            painted_byte_position: 0,
            last_sent_update_sequence: 0,
            has_paint_report: false,
            flight_emptied_at: None,
        }
    }

    /// Whether the next update may go out when the connection has written
    /// `written_byte_count` bytes: no update waits for a paint report, or the
    /// client has reported a paint and the bytes written after the last
    /// reported update are below the window.
    pub(crate) fn can_send_update(&self, written_byte_count: u64) -> bool {
        if self.in_flight_updates.is_empty() {
            return true;
        }
        self.has_paint_report
            && written_byte_count.saturating_sub(self.painted_byte_position)
                < self.window_byte_count
    }

    /// Record that update `update_sequence` went out, and that the
    /// connection's written byte count was `end_byte_position` after it and
    /// the image transfers that follow it. Its first byte was written at
    /// `write_started_at`, and its last at `sent_at`.
    ///
    /// When no update waited for a report since a report at least 100 ms and
    /// two base round trips before `write_started_at`, the window starts over
    /// first.
    pub(crate) fn record_update_sent(
        &mut self,
        update_sequence: u64,
        end_byte_position: u64,
        write_started_at: Instant,
        sent_at: Instant,
    ) {
        if self.in_flight_updates.is_empty() {
            if let Some(flight_emptied_at) = self.flight_emptied_at {
                let idle_duration = write_started_at.saturating_duration_since(flight_emptied_at);
                let is_idle_past_base = self
                    .base_round_trip
                    .is_none_or(|base_round_trip| idle_duration > base_round_trip * 2);
                if idle_duration > MINIMUM_IDLE_DURATION && is_idle_past_base {
                    self.start_window_over();
                }
            }
        }
        let in_flight_byte_count = end_byte_position.saturating_sub(self.painted_byte_position);
        self.in_flight_updates.push_back(InFlightUpdate {
            update_sequence,
            sent_at,
            end_byte_position,
            is_window_filled: in_flight_byte_count >= self.window_byte_count,
        });
        self.last_sent_update_sequence = update_sequence;
    }

    /// Take the client's report that it painted after update
    /// `painted_update_report.update_sequence`: that update and every update
    /// before it leave the flight, and the report measures the round trip of
    /// that update. A report that names no update in flight changes nothing.
    ///
    /// # Errors
    /// [`PaintReportError::UnsentUpdate`] for a report of an update that was
    /// not sent. The window changes nothing then.
    pub(crate) fn record_update_painted(
        &mut self,
        painted_update_report: PaintedUpdateReport,
    ) -> Result<(), PaintReportError> {
        if painted_update_report.update_sequence > self.last_sent_update_sequence {
            return Err(PaintReportError::UnsentUpdate {
                reported_update_sequence: painted_update_report.update_sequence,
                last_sent_update_sequence: self.last_sent_update_sequence,
            });
        }
        let mut painted_update = None;
        while self
            .in_flight_updates
            .front()
            .is_some_and(|in_flight_update| {
                in_flight_update.update_sequence <= painted_update_report.update_sequence
            })
        {
            painted_update = self.in_flight_updates.pop_front();
        }
        let Some(painted_update) = painted_update else {
            return Ok(());
        };
        self.has_paint_report = true;
        self.painted_byte_position = painted_update.end_byte_position;
        if self.in_flight_updates.is_empty() {
            self.flight_emptied_at = Some(painted_update_report.reported_at);
        }
        let round_trip = painted_update_report
            .reported_at
            .saturating_duration_since(painted_update.sent_at)
            .max(MINIMUM_ROUND_TRIP_DURATION);
        self.record_measurement(&painted_update, round_trip);
        Ok(())
    }

    /// Record `round_trip`, the round trip of `painted_update`, and change the
    /// window when the batch holds 3 measurements.
    fn record_measurement(&mut self, painted_update: &InFlightUpdate, round_trip: Duration) {
        let base_round_trip = self.base_round_trip.map_or(round_trip, |base_round_trip| {
            base_round_trip.min(round_trip)
        });
        self.base_round_trip = Some(base_round_trip);
        if painted_update.update_sequence <= self.changed_through_update_sequence {
            return;
        }
        let measurement_batch = &mut self.measurement_batch;
        measurement_batch.measurement_count += 1;
        let measured_round_trip = measurement_batch
            .lowest_round_trip
            .map_or(round_trip, |lowest_round_trip| {
                lowest_round_trip.min(round_trip)
            });
        measurement_batch.lowest_round_trip = Some(measured_round_trip);
        if painted_update.is_window_filled {
            measurement_batch.lowest_filled_round_trip = Some(
                measurement_batch
                    .lowest_filled_round_trip
                    .map_or(round_trip, |lowest_filled_round_trip| {
                        lowest_filled_round_trip.min(round_trip)
                    }),
            );
        }
        if measurement_batch.measurement_count < MEASUREMENT_COUNT_PER_CHANGE {
            return;
        }
        let filled_round_trip = measurement_batch.lowest_filled_round_trip;
        self.measurement_batch = MeasurementBatch::default();
        self.changed_through_update_sequence = self.last_sent_update_sequence;
        self.change_window(base_round_trip, measured_round_trip, filled_round_trip);
    }

    /// Change the window once, from the base round trip `base_round_trip`, the
    /// batch's lowest round trip `measured_round_trip`, and its lowest round
    /// trip of an update that filled the window, `filled_round_trip`.
    fn change_window(
        &mut self,
        base_round_trip: Duration,
        measured_round_trip: Duration,
        filled_round_trip: Option<Duration>,
    ) {
        let measured_excess = measured_round_trip.saturating_sub(base_round_trip);
        let is_filled_excess_below = |margin: Duration| {
            filled_round_trip.is_some_and(|filled_round_trip| {
                filled_round_trip.saturating_sub(base_round_trip) < margin
            })
        };
        if measured_excess > LATENCY_SPIKE_MARGIN_DURATION && measured_excess > base_round_trip / 2
        {
            self.window_byte_count =
                scale_byte_count(self.window_byte_count, base_round_trip, measured_round_trip);
            self.is_doubling = false;
        }
        if self.is_doubling {
            if measured_excess > ROUND_TRIP_GROWTH_MARGIN_DURATION {
                self.window_byte_count =
                    scale_byte_count(self.window_byte_count, base_round_trip, measured_round_trip);
                self.is_doubling = false;
            } else if is_filled_excess_below(ROUND_TRIP_GROWTH_MARGIN_DURATION) {
                self.window_byte_count = self.window_byte_count.saturating_mul(2);
            }
        } else {
            let window_step_byte_count = scale_byte_count(
                self.window_byte_count,
                WINDOW_STEP_DURATION,
                measured_round_trip,
            )
            .min(MAXIMUM_WINDOW_STEP_BYTE_COUNT);
            if measured_excess > ROUND_TRIP_SHRINK_MARGIN_DURATION {
                self.window_byte_count = self
                    .window_byte_count
                    .saturating_sub(window_step_byte_count);
            } else if is_filled_excess_below(ROUND_TRIP_FAST_GROWTH_MARGIN_DURATION) {
                self.window_byte_count += 2 * window_step_byte_count;
            } else if is_filled_excess_below(ROUND_TRIP_GROWTH_MARGIN_DURATION) {
                self.window_byte_count += window_step_byte_count;
            }
        }
        self.window_byte_count = self
            .window_byte_count
            .clamp(MINIMUM_WINDOW_BYTE_COUNT, MAXIMUM_WINDOW_BYTE_COUNT);
        tracing::debug!(
            window_byte_count = self.window_byte_count,
            ?base_round_trip,
            ?measured_round_trip,
            is_doubling = self.is_doubling,
            "paint window changed"
        );
    }

    /// Start the window over: a window over 64 KiB drops to 64 KiB, the base
    /// and the batch are cleared, and the window doubles.
    fn start_window_over(&mut self) {
        self.window_byte_count = self.window_byte_count.min(INITIAL_WINDOW_BYTE_COUNT);
        self.is_doubling = true;
        self.base_round_trip = None;
        self.measurement_batch = MeasurementBatch::default();
    }
}

/// `byte_count` × `numerator` / `denominator`, in whole bytes rounded down.
/// `denominator` is at least [`MINIMUM_ROUND_TRIP_DURATION`].
fn scale_byte_count(byte_count: u64, numerator: Duration, denominator: Duration) -> u64 {
    let scaled_byte_count =
        u128::from(byte_count) * numerator.as_micros() / denominator.as_micros();
    u64::try_from(scaled_byte_count).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
