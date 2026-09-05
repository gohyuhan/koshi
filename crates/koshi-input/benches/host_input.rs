use std::hint::black_box;
use std::time::{Duration, Instant};

use koshi_input::host::Parser;

const EVENTS_PER_CASE: usize = 200_000;
const RUNS: usize = 10;

fn main() {
    bench("ascii", b"a");
    bench("kitty-key", b"\x1b[97;5u");
    bench("sgr-mouse", b"\x1b[<0;11;4M");
}

fn bench(name: &str, event: &[u8]) {
    let _ = measure(event);
    let mut samples: Vec<(Duration, usize)> = (1..RUNS).map(|_| measure(event)).collect();
    samples.sort_unstable_by_key(|sample| sample.0);
    let (elapsed, parsed) = samples[samples.len() / 2];
    let nanos = elapsed.as_nanos() / parsed as u128;
    println!("{name}: {parsed} events, {nanos} ns/event");
}

fn measure(event: &[u8]) -> (Duration, usize) {
    let mut parser = Parser::default();
    let start = Instant::now();
    let mut parsed = 0usize;
    for _ in 0..EVENTS_PER_CASE {
        parser.push(black_box(event));
        while parser.pop().is_some() {
            parsed += 1;
        }
    }
    (start.elapsed(), parsed)
}
