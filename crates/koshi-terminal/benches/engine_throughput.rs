//! Engine throughput on the six output workloads. Each run feeds 4 MiB of one
//! workload, generated from `DEFAULT_WORKLOAD_SEED`, to a fresh 200×50 engine
//! in 8 KiB chunks. The program prints one line per workload with the median
//! rate of 5 runs, after one run that is not counted. 1 MB is 1 000 000 bytes.
//!
//! Example — `cargo bench -p koshi-terminal --bench engine_throughput` prints
//! `plain: 4194432 bytes, 152.5 MB/s` for a run whose median is 27.5 ms.

use std::hint::black_box;
use std::time::{Duration, Instant};

use koshi_core::process::PtySize;
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::output_workloads::{
    build_workload_bytes, OutputWorkload, DEFAULT_WORKLOAD_SEED,
};

const WORKLOAD_BYTE_COUNT: usize = 4 * 1024 * 1024;
const READ_CHUNK_BYTE_COUNT: usize = 8 * 1024;
const MEASUREMENT_RUN_COUNT: usize = 5;
const ENGINE_PTY_SIZE: PtySize = PtySize {
    column_count: 200,
    row_count: 50,
};

fn main() {
    for output_workload in OutputWorkload::ALL {
        benchmark_workload(output_workload);
    }
}

fn benchmark_workload(output_workload: OutputWorkload) {
    let workload_bytes =
        build_workload_bytes(output_workload, WORKLOAD_BYTE_COUNT, DEFAULT_WORKLOAD_SEED);
    let _ = measure_engine(&workload_bytes);
    let mut run_durations: Vec<Duration> = (0..MEASUREMENT_RUN_COUNT)
        .map(|_| measure_engine(&workload_bytes))
        .collect();
    run_durations.sort_unstable();
    let median_duration = run_durations[MEASUREMENT_RUN_COUNT / 2];
    let megabytes_per_second =
        workload_bytes.len() as f64 / median_duration.as_secs_f64() / 1_000_000.0;
    println!(
        "{}: {} bytes, {megabytes_per_second:.1} MB/s",
        output_workload.get_workload_name(),
        workload_bytes.len()
    );
}

fn measure_engine(workload_bytes: &[u8]) -> Duration {
    let mut terminal_engine = TerminalEngine::from_pty_size(ENGINE_PTY_SIZE);
    let start_instant = Instant::now();
    for read_chunk in workload_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
        black_box(terminal_engine.process_pty_output(black_box(read_chunk)));
    }
    start_instant.elapsed()
}
