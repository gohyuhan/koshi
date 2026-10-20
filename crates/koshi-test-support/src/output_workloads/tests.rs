//! Tests for the output workload generators: the default seed gives the
//! recorded bytes, and each workload has the shape its doc states.

use super::*;

#[test]
fn every_workload_gives_its_recorded_bytes_for_the_default_seed() {
    let recorded_byte_counts_and_hashes = [
        (OutputWorkload::Plain, 65_664, 0x2c20_0064_28c5_c7a8),
        (OutputWorkload::Sgr256, 65_646, 0x819b_e2de_be59_4c12),
        (OutputWorkload::Truecolor, 65_937, 0xc958_fbb2_1a3a_4313),
        (OutputWorkload::Unicode, 65_706, 0xec1c_cf99_736a_5823),
        (OutputWorkload::Fullscreen, 76_129, 0x86f9_226b_efee_0717),
        (OutputWorkload::Progress, 65_578, 0xa59f_1197_750e_4b97),
    ];
    for (workload, recorded_byte_count, recorded_hash) in recorded_byte_counts_and_hashes {
        let workload_bytes = build_workload_bytes(workload, 64 * 1024, DEFAULT_WORKLOAD_SEED);
        assert_eq!(
            (workload_bytes.len(), compute_fnv1a_hash(&workload_bytes)),
            (recorded_byte_count, recorded_hash),
            "{}",
            workload.get_workload_name()
        );
    }
}

#[test]
fn every_seeded_workload_changes_with_the_seed() {
    for workload in OutputWorkload::ALL
        .into_iter()
        .filter(|workload| *workload != OutputWorkload::Progress)
    {
        let first_bytes = build_workload_bytes(workload, 64 * 1024, 1);
        let second_bytes = build_workload_bytes(workload, 64 * 1024, 2);
        assert_ne!(
            first_bytes,
            second_bytes,
            "{}",
            workload.get_workload_name()
        );
    }
}

#[test]
fn every_workload_reaches_the_minimum_byte_count_and_ends_whole() {
    for workload in OutputWorkload::ALL {
        let workload_bytes = build_workload_bytes(workload, 100_000, DEFAULT_WORKLOAD_SEED);
        assert!(
            workload_bytes.len() >= 100_000,
            "{}",
            workload.get_workload_name()
        );
        assert!(
            std::str::from_utf8(&workload_bytes).is_ok(),
            "{}",
            workload.get_workload_name()
        );
    }
}

#[test]
fn plain_workload_lines_hold_190_printable_ascii_bytes() {
    let workload_bytes = build_workload_bytes(OutputWorkload::Plain, 50_000, 7);
    assert!(workload_bytes.ends_with(b"\r\n"));
    let workload_lines: Vec<&[u8]> = workload_bytes
        .strip_suffix(b"\r\n")
        .expect("the last line ends in CR LF")
        .split(|workload_byte| *workload_byte == b'\n')
        .collect();
    for workload_line in workload_lines {
        let line_text = workload_line.strip_suffix(b"\r").unwrap_or(workload_line);
        assert_eq!(line_text.len(), WORKLOAD_LINE_COLUMN_COUNT);
        assert!(line_text
            .iter()
            .all(|line_byte| (0x20..=0x7e).contains(line_byte)));
    }
}

#[test]
fn progress_workload_rewrites_one_line_after_carriage_return() {
    let workload_bytes = build_workload_bytes(OutputWorkload::Progress, 1, DEFAULT_WORKLOAD_SEED);
    assert_eq!(
        workload_bytes,
        b"\rbuilding [........................................]   0% (0/1000)".to_vec()
    );
    let workload_bytes = build_workload_bytes(OutputWorkload::Progress, 150, DEFAULT_WORKLOAD_SEED);
    assert!(!workload_bytes.contains(&b'\n'));
    assert_eq!(
        workload_bytes
            .iter()
            .filter(|workload_byte| **workload_byte == b'\r')
            .count(),
        3
    );
}

#[test]
fn fullscreen_workload_addresses_every_row_once_per_repaint() {
    let workload_bytes = build_workload_bytes(OutputWorkload::Fullscreen, 1, DEFAULT_WORKLOAD_SEED);
    let workload_text = std::str::from_utf8(&workload_bytes).expect("ASCII");
    assert!(workload_text.starts_with("\x1b[H\x1b[1;1H\x1b[38;5;"));
    assert!(workload_text.ends_with("\x1b[0m"));
    for row_number in 1..=FULLSCREEN_ROW_COUNT {
        assert_eq!(
            workload_text
                .matches(&format!("\x1b[{row_number};1H"))
                .count(),
            1
        );
    }
}
