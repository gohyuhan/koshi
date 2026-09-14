use std::hint::black_box;
use std::time::{Duration, Instant};

use koshi_input::host::Parser;

const EVENT_COUNT_PER_CASE: usize = 200_000;
const MEASUREMENT_RUN_COUNT: usize = 10;

fn main() {
    benchmark_parser("ascii", b"a");
    benchmark_parser("kitty-key", b"\x1b[97;5u");
    benchmark_parser("sgr-mouse", b"\x1b[<0;11;4M");
}

fn benchmark_parser(case_name: &str, input_bytes: &[u8]) {
    let _ = measure_parser(input_bytes);
    let mut parser_duration_samples: Vec<(Duration, usize)> = (1..MEASUREMENT_RUN_COUNT)
        .map(|_| measure_parser(input_bytes))
        .collect();
    parser_duration_samples.sort_unstable_by_key(|duration_sample| duration_sample.0);
    let (elapsed_duration, parsed_event_count) =
        parser_duration_samples[parser_duration_samples.len() / 2];
    let nanoseconds_per_event = elapsed_duration.as_nanos() / parsed_event_count as u128;
    println!("{case_name}: {parsed_event_count} events, {nanoseconds_per_event} ns/event");
}

fn measure_parser(input_bytes: &[u8]) -> (Duration, usize) {
    let mut parser = Parser::default();
    let start_instant = Instant::now();
    let mut parsed_event_count = 0usize;
    for _ in 0..EVENT_COUNT_PER_CASE {
        parser.process_input_bytes(black_box(input_bytes));
        while parser.remove_next_pending_event().is_some() {
            parsed_event_count += 1;
        }
    }
    (start_instant.elapsed(), parsed_event_count)
}
