//! Tests for the outer terminal an attached client owns: the viewer built for
//! it, painting a frame, the cursor-style mapping, and the window title. A fake PTY
//! backend stands in for real children and ratatui's `TestBackend` renders into
//! an in-memory buffer, so painting runs without a terminal. Platform terminal
//! setup and the input reader are TTY-bound; event conversion is tested here,
//! and key decoding is covered in `koshi-input`.

use super::*;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};

use koshi_config::layer::{PartialColorPalette, PartialKeybindingsConfig, PartialThemeConfig};
use koshi_config::types::RgbColor;
use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::ids::{CommandId, PaneId, SessionId};
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::{CommittedRegions, RenderSnapshot};
use koshi_renderer::{ImagePaint, ImageSourceRect};
use koshi_runtime::runtime::bus::EventFilter;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

use koshi_link::config::LoadedConfig;

use crate::tests::VIEWPORT;

fn regions(viewport: Size) -> CommittedRegions {
    CommittedRegions::core(viewport, 0)
}

/// A bootstrapped server driven by `fake`, with its client id and sole pane id.
fn boot(fake: &Arc<FakePtyBackend>) -> (Server, ClientId, PaneId) {
    let backend: Arc<dyn PtyBackend> = fake.clone();
    let (tx, rx) = mpsc::channel();
    let mut server = Server::new(backend, rx, tx);
    let client_id = server
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    let pane_id = fake.spawned_panes()[0];
    (server, client_id, pane_id)
}

/// A client half for `client_id`, subscribed to `server`'s events, built the
/// way the launch builds it — through [`viewer`].
fn test_client(server: &mut Server, client_id: ClientId) -> Client {
    test_client_with(server, client_id, LoadedConfig::default())
}

/// The same, on the config files `loaded` stands for.
fn test_client_with(server: &mut Server, client_id: ClientId, loaded: LoadedConfig) -> Client {
    let events = server.subscribe(client_id, EventFilter::All);
    viewer(
        client_id,
        VIEWPORT,
        events,
        TerminalCleanupGuard::new(),
        loaded,
    )
}

/// The whole rendered screen flattened to a string, for substring assertions.
fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// Queue one Kitty frame and advance every bounded upload slice to completion.
fn write_complete_kitty_frame<W: Write>(
    writer: &mut W,
    cache: &mut KittyImageCache,
    paints: &[ImagePaint],
    cursor: Option<Position>,
) -> io::Result<()> {
    write_kitty_frame(writer, cache, paints, cursor)?;
    while kitty_image_work_pending(cache) {
        advance_kitty_image(writer, cache)?;
    }
    Ok(())
}

/// Return the exact fast zlib stream used by output.
fn zlib_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).expect("zlib input writes");
    encoder.finish().expect("zlib output finishes")
}

/// Return the exact base64 payload for the fast zlib stream used by output.
fn encoded_zlib(bytes: &[u8]) -> String {
    STANDARD.encode(zlib_bytes(bytes))
}

/// Build one one-cell Kitty paint with the supplied identity, pixels, and target.
fn one_cell_image_paint(
    pane_id: PaneId,
    placement_id: u64,
    rgba: [u8; 4],
    target: Rect,
) -> ImagePaint {
    ImagePaint::new(
        pane_id,
        placement_id,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: rgba.to_vec(),
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        target,
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    )
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test writer failed",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test writer failed",
        ))
    }
}

struct FailOnWrite {
    fail_at: usize,
    writes: usize,
    output: Vec<u8>,
}

impl Write for FailOnWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let write_index = self.writes;
        self.writes += 1;
        if write_index == self.fail_at {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed",
            ));
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailingFlushWriter {
    output: Vec<u8>,
}

impl Write for FailingFlushWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test flush failed",
        ))
    }
}

/// The frame this client is owed, as the session composes it.
fn frame(server: &Server, client_id: ClientId) -> RenderSnapshot {
    server.build_snapshot(client_id).expect("snapshot")
}

#[test]
fn the_launch_hands_the_viewer_the_config_files_it_read() {
    // The viewer's settings, colors, and keymap all come from the files the
    // launch read. A launch that built the viewer without them would paint the
    // stock palette over the user's theme and answer the stock keys.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane) = boot(&fake);

    let client = test_client_with(
        &mut server,
        client_id,
        LoadedConfig {
            app: None,
            theme: Some(PartialThemeConfig {
                name: Some("ocean".to_owned()),
                colors: Some(PartialColorPalette {
                    border_focused: Some(RgbColor::new(0xff, 0, 0)),
                    ..PartialColorPalette::default()
                }),
            }),
            keybindings: None,
        },
    );

    assert_eq!(client.config().theme.name, "ocean");
    assert_eq!(
        client.theme().border_focused,
        ratatui::style::Color::Rgb(0xff, 0, 0)
    );
}

#[test]
fn the_launch_hands_the_viewer_the_keymap_file_it_read() {
    // A keymap layer that validates replaces the built-in keybinding settings.
    // A launch that dropped `loaded.keybindings` would leave the stock 500 ms
    // chord timeout in place.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane) = boot(&fake);

    let client = test_client_with(
        &mut server,
        client_id,
        LoadedConfig {
            app: None,
            theme: None,
            keybindings: Some(PartialKeybindingsConfig {
                chord_timeout_ms: Some(1234),
                ..PartialKeybindingsConfig::default()
            }),
        },
    );

    assert_eq!(client.config().keybindings.chord_timeout_ms, 1234);
}

#[test]
fn the_painted_hint_bar_follows_the_clients_mouse_select_state() {
    // The hint bar is painted from the viewer's own keymap, but which label the
    // mouse-select entry wears depends on session state the frame carries. A
    // frame that dropped that link would keep offering "Mouse Select" while
    // selection was already on.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    let snapshot = frame(&server, client_id);

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(Size {
            cols: 120,
            rows: 24,
        }),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");
    assert!(screen_text(&terminal).contains("Mouse Select"));

    server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
    let snapshot = frame(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(Size {
            cols: 120,
            rows: 24,
        }),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    let painted = screen_text(&terminal);
    assert!(painted.contains("Mouse Unselect"), "{painted}");
}

#[test]
fn pty_output_is_painted_to_the_screen() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);

    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"hello".to_vec(),
        },)
        .is_continue());

    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let snapshot = frame(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut None,
    )
    .expect("paint");

    assert!(
        screen_text(&terminal).contains("hello"),
        "the shell's output should appear on the rendered screen"
    );
}

#[test]
fn painting_emits_a_changed_cursor_style_and_records_it() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    // The pane asks for a steady bar via DECSCUSR (`CSI 6 SP q`); the first
    // paint sees it differ from the starting `None` and records the new style.
    assert!(server
        .handle_runtime_event(RuntimeEvent::PtyOutput {
            pane_id,
            bytes: b"\x1b[6 q".to_vec(),
        },)
        .is_continue());
    let client = test_client(&mut server, client_id);
    let mut last_cursor = None;
    let snapshot = frame(&server, client_id);
    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut last_cursor,
    )
    .expect("paint");

    assert_eq!(
        last_cursor,
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: false,
        })
    );
}

#[test]
fn painting_a_frame_that_names_no_cursor_style_records_none() {
    // A frame with no focused pane leaves `cursor_style` with nothing to
    // report. The record still follows the frame: a later frame that does name
    // a style counts as a change and is sent again.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = frame(&server, client_id);
    snapshot.client.focused_pane = None;
    let mut last_cursor = Some(CursorStyle::Shaped {
        shape: CursorShape::Block,
        blink: true,
    });

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut String::new(),
        &mut last_cursor,
    )
    .expect("paint");

    assert_eq!(last_cursor, None);
}

#[test]
fn painting_records_the_window_title_it_sent() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("htop".to_string());
    let mut last_title = String::new();

    paint_frame(
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        &mut last_title,
        &mut None,
    )
    .expect("paint");

    assert_eq!(last_title, "quiet-lake | htop");
}

#[test]
fn a_paint_after_a_title_change_records_the_new_title() {
    // The record decides whether `SetTitle` is written at all. It tracks every
    // frame, not only the first one.
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = None;
    let mut last_title = String::new();
    let paint = |terminal: &mut Terminal<TestBackend>,
                 snapshot: &RenderSnapshot,
                 last_title: &mut String| {
        paint_frame(
            terminal,
            &client,
            snapshot,
            &regions(VIEWPORT),
            &ViewerPaint::from_frame(&client, snapshot),
            last_title,
            &mut None,
        )
        .expect("paint");
    };

    paint(&mut terminal, &snapshot, &mut last_title);
    assert_eq!(last_title, "quiet-lake");

    snapshot.session.name = "loud-hill".to_string();
    paint(&mut terminal, &snapshot, &mut last_title);

    assert_eq!(last_title, "loud-hill");
}

#[test]
fn each_pane_cursor_style_maps_to_the_crossterm_command_that_re_emits_it() {
    // koshi copies the focused pane's DECSCUSR style out to the terminal it is
    // itself running in, and crossterm writes these commands as the very same
    // DECSCUSR sequences. So each pair must map to the command whose bytes are
    // the sequence that produced it — `CSI 5 SP q` in, `CSI 5 SP q` out.
    // Nothing else in the suite would catch a swapped arm: a `Bar` sent as
    // `BlinkingUnderScore` renders vim's insert cursor as an underline while
    // every test still passes.
    let shaped = |shape, blink| CursorStyle::Shaped { shape, blink };
    let cases = [
        // A pane that asked for nothing hands the cursor back to the user.
        (CursorStyle::UserDefault, SetCursorStyle::DefaultUserShape),
        (
            shaped(CursorShape::Block, true),
            SetCursorStyle::BlinkingBlock,
        ),
        (
            shaped(CursorShape::Block, false),
            SetCursorStyle::SteadyBlock,
        ),
        (
            shaped(CursorShape::Underline, true),
            SetCursorStyle::BlinkingUnderScore,
        ),
        (
            shaped(CursorShape::Underline, false),
            SetCursorStyle::SteadyUnderScore,
        ),
        (shaped(CursorShape::Bar, true), SetCursorStyle::BlinkingBar),
        (shaped(CursorShape::Bar, false), SetCursorStyle::SteadyBar),
    ];
    for (style, expected) in cases {
        assert_eq!(set_cursor_style(style), expected, "{style:?}");
    }
}

// --- window_title: the outer-terminal title string ---

#[test]
fn window_title_with_no_focused_pane_is_just_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, _pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = None;

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_titled_focused_pane_joins_session_and_title() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("htop".to_string());

    assert_eq!(window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_reads_the_focused_pane_not_the_first_one_listed() {
    // The lookup matches on the pane id. Every other title test lists a single
    // pane; a lookup that took the first entry would pass all of them.
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    let focused = PaneId::new();
    let mut second = snapshot.panes[0].clone();
    second.id = focused;
    second.title = Some("htop".to_string());
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("bash".to_string());
    snapshot.panes.push(second);
    snapshot.client.focused_pane = Some(focused);

    assert_eq!(window_title(&snapshot), "quiet-lake | htop");
}

#[test]
fn window_title_with_an_untitled_focused_pane_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = None;

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_keeps_a_non_ascii_pane_title_whole() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some("日本語 🙂".to_string());

    assert_eq!(window_title(&snapshot), "quiet-lake | 日本語 🙂");
}

#[test]
fn window_title_with_an_empty_pane_title_falls_back_to_the_session_name() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    snapshot.panes[0].id = pane_id;
    snapshot.panes[0].title = Some(String::new());

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

#[test]
fn window_title_with_a_focused_pane_absent_from_the_pane_list_falls_back() {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut snapshot = frame(&server, client_id);
    snapshot.session.name = "quiet-lake".to_string();
    snapshot.client.focused_pane = Some(pane_id);
    // No `PaneSnapshot` carries `pane_id`, so the lookup in `window_title`
    // cannot find a title for it.
    snapshot.panes.clear();

    assert_eq!(window_title(&snapshot), "quiet-lake");
}

/// The title `window_title` builds for a session named `session_name` holding
/// one focused pane titled `pane_title`, after that frame has travelled the
/// session-to-client wire and been read back by
/// [`to_snapshot`](crate::attach::paint::to_snapshot).
fn title_off_the_wire(session_name: &str, pane_title: &str) -> String {
    let fake = Arc::new(FakePtyBackend::new());
    let (server, client_id, pane_id) = boot(&fake);
    let mut sent = frame(&server, client_id);
    sent.session.name = session_name.to_string();
    sent.client.focused_pane = Some(pane_id);
    for pane in &mut sent.panes {
        if pane.id == pane_id {
            pane.title = Some(pane_title.to_string());
        }
    }

    let read_back =
        crate::attach::paint::to_snapshot(&koshi_runtime::runtime::frame::wire_frame(&sent));
    window_title(&read_back)
}

#[test]
fn a_window_title_can_carry_no_osc_terminator() {
    // `window_title` is written into the viewer's own terminal verbatim,
    // inside `OSC 0; ... BEL` (crossterm `SetTitle`). A session server this
    // client did not build chooses both halves of that title.
    for hostile in [
        "x\u{7}pwned",          // BEL
        "x\u{1b}]0;pwned\u{7}", // ESC
        "x\u{9c}pwned",         // C1 ST
        "x\u{9b}2J",            // C1 CSI
    ] {
        let from_session_name = title_off_the_wire(hostile, "bash");
        assert!(
            !from_session_name.contains(['\u{7}', '\u{1b}', '\u{9c}', '\u{9b}']),
            "an OSC terminator survived into the window title: {from_session_name:?}"
        );
        let from_pane_title = title_off_the_wire("dev", hostile);
        assert!(
            !from_pane_title.contains(['\u{7}', '\u{1b}', '\u{9c}', '\u{9b}']),
            "an OSC terminator survived into the window title: {from_pane_title:?}"
        );
    }
}

#[test]
fn a_window_title_names_the_session_and_the_pane_the_wire_carried() {
    assert_eq!(title_off_the_wire("dev", "bash"), "dev | bash");
    assert_eq!(title_off_the_wire("dev\u{7}", "\u{1b}bash"), "dev | bash");
}

#[test]
fn a_window_title_is_bounded_by_the_pane_title_cap() {
    let cap = koshi_core::text::MAX_REPORTED_TEXT_BYTES;
    let title = title_off_the_wire("dev", &"a".repeat(100_000));

    assert_eq!(title, format!("dev | {}", "a".repeat(cap)));
}

#[test]
fn kitty_probe_uses_a_300_ms_deadline_and_a_non_storing_query() {
    let mut output = Vec::new();

    write_kitty_graphics_query(&mut output).expect("query writes");

    assert_eq!(TERMINAL_QUERY_TIMEOUT, Duration::from_millis(300));
    assert_eq!(
        output,
        b"\x1b_Gi=4294967295,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c"
    );
}

#[test]
fn either_terminal_stream_opens_the_terminal_device() {
    assert!(!terminal_device_needed(false, false));
    assert!(terminal_device_needed(true, false));
    assert!(terminal_device_needed(false, true));
    assert!(terminal_device_needed(true, true));
}

#[test]
fn redirected_standard_output_skips_the_controlling_terminal_probe() {
    let mut called = false;
    let support = graphics_support_for_output(false, || {
        called = true;
        Ok(GraphicsSupport::Kitty)
    })
    .expect("redirected output selects a supported fallback");

    assert_eq!(support, GraphicsSupport::Unsupported);
    assert!(!called);

    let support = graphics_support_for_output(true, || {
        called = true;
        Ok(GraphicsSupport::Kitty)
    })
    .expect("terminal output accepts the probe result");
    assert_eq!(support, GraphicsSupport::Kitty);
    assert!(called);

    let error = graphics_support_for_output(true, || {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "query write failed",
        ))
    })
    .expect_err("terminal query errors are returned");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn raw_mode_operation_restores_cooked_mode_after_success() {
    let mut calls = Vec::new();

    let value = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Ok(GraphicsSupport::Kitty)
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect("mode cycle succeeds");

    assert_eq!(value, GraphicsSupport::Kitty);
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_stops_when_raw_mode_entry_fails() {
    let mut calls = Vec::new();

    let error = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "raw error"))
        },
        |calls| {
            calls.push("probe");
            Ok(GraphicsSupport::Kitty)
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect_err("the raw-mode error is returned");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls, ["raw"]);
}

#[test]
fn raw_mode_operation_restores_cooked_mode_after_an_operation_error() {
    let mut calls = Vec::new();

    let error = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Err::<GraphicsSupport, _>(io::Error::new(io::ErrorKind::InvalidData, "probe error"))
        },
        |calls| {
            calls.push("cooked");
            Ok(())
        },
    )
    .expect_err("the probe error is returned");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_returns_a_cooked_mode_error() {
    let mut calls = Vec::new();

    let error = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Ok(GraphicsSupport::Kitty)
        },
        |calls| {
            calls.push("cooked");
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cooked error",
            ))
        },
    )
    .expect_err("the cooked-mode error is returned");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn raw_mode_operation_keeps_the_operation_error_when_restore_also_fails() {
    let mut calls = Vec::new();

    let error = with_raw_mode(
        &mut calls,
        |calls| {
            calls.push("raw");
            Ok(())
        },
        |calls| {
            calls.push("probe");
            Err::<GraphicsSupport, _>(io::Error::new(io::ErrorKind::InvalidData, "probe error"))
        },
        |calls| {
            calls.push("cooked");
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cooked error",
            ))
        },
    )
    .expect_err("the probe error is returned");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "probe error");
    assert_eq!(calls, ["raw", "probe", "cooked"]);
}

#[test]
fn kitty_probe_filter_leaves_keys_and_other_replies_buffered() {
    use koshi_input::host::{KeyCode, KittyGraphicsReply};

    assert!(is_kitty_probe_event(&Event::KittyGraphicsReply(
        KittyGraphicsReply {
            image_id: u32::MAX,
            ok: true,
        }
    )));
    assert!(!is_kitty_probe_event(&Event::KittyGraphicsReply(
        KittyGraphicsReply {
            image_id: 31,
            ok: true,
        }
    )));
    assert!(!is_kitty_probe_event(&Event::Key(
        KeyCode::Char('x').into()
    )));
    assert!(is_kitty_probe_event(&Event::PrimaryDeviceAttributes));
}

#[test]
fn terminal_modes_are_enabled_after_entering_the_alternate_screen() {
    let mut output = Vec::new();

    enable_terminal_modes(&mut output).expect("terminal modes write");

    assert_eq!(
        output,
        b"\x1b[?1049h\x1b[>7u\x1b[?1003h\x1b[?1006h\x1b[?2004h"
    );
}

#[test]
fn terminal_cleanup_reverses_modes_and_deletes_kitty_images() {
    let mut output = Vec::new();
    let claimed = AtomicBool::new(false);

    write_terminal_cleanup(&mut output, GraphicsSupport::Kitty, &claimed)
        .expect("terminal cleanup writes");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(claimed.load(Ordering::Acquire));
}

#[test]
fn terminal_cleanup_attempts_mode_resets_after_image_delete_fails() {
    let mut writer = FailOnWrite {
        fail_at: 0,
        writes: 0,
        output: Vec::new(),
    };
    let claimed = AtomicBool::new(false);

    let error = write_terminal_cleanup(&mut writer, GraphicsSupport::Kitty, &claimed)
        .expect_err("the failed delete is returned");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        writer.output,
        b"\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
}

#[test]
fn terminal_application_modes_are_restored_once_across_cleanup_paths() {
    let mut output = Vec::new();
    let active = AtomicBool::new(true);
    let image_claimed = AtomicBool::new(false);

    restore_application_modes(&mut output, GraphicsSupport::Kitty, &active, &image_claimed)
        .expect("the panic cleanup writes");
    restore_application_modes(&mut output, GraphicsSupport::Kitty, &active, &image_claimed)
        .expect("the unwind cleanup is already complete");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[<1u\x1b[?1049l\x1b[?25h\x1b[0 q"
    );
    assert!(!active.load(Ordering::Acquire));
    assert!(image_claimed.load(Ordering::Acquire));
}

#[test]
fn host_resize_and_paste_events_keep_their_exact_values() {
    let client_id = ClientId::new();
    let resize = terminal_runtime_event(
        client_id,
        Event::WindowResized(WindowSize {
            cols: 101,
            rows: 37,
            pixel_width: Some(1_010),
            pixel_height: Some(740),
        }),
    );
    let Some(RuntimeEvent::Resize {
        client_id: actual_client_id,
        size,
        pane_area,
    }) = resize
    else {
        panic!("expected the exact resize event, got {resize:?}");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(
        size,
        Size {
            cols: 101,
            rows: 37,
        }
    );
    assert_eq!(pane_area, Some(core_pane_area(size)));

    let paste = terminal_runtime_event(client_id, Event::Paste("hello 🐈".to_string()));
    let Some(RuntimeEvent::HostPaste {
        client_id: actual_client_id,
        text,
    }) = paste
    else {
        panic!("expected the exact paste event, got {paste:?}");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(text, "hello 🐈");
}

#[test]
fn kitty_writer_uploads_full_rgba_places_its_crop_and_restores_cursor() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 2,
                height: 2,
                rgba: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 3,
            y: 4,
            width: 2,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 1,
            width: 2,
            height: 1,
        },
        -2,
    );
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    write_complete_kitty_frame(&mut output, &mut cache, &[paint], Some(Position::new(8, 9)))
        .expect("Kitty output writes");

    let payload = encoded_zlib(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    assert_eq!(
        output,
        format!("\x1b_Ga=t,f=32,s=2,v=2,I=1,q=2,o=z,m=0;{payload}\x1b\\\x1b[5;4H\x1b_Ga=p,I=1,p=1,x=0,y=1,w=2,h=1,c=2,r=1,C=1,z=-2,q=2;\x1b\\\x1b[10;9H").into_bytes()
    );
}

#[test]
fn kitty_writer_emits_sixel_and_iterm_rgba_through_kitty() {
    let payload = encoded_zlib(&[1, 2, 3, 4]);
    let expected = format!(
        "\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=0;{payload}\x1b\\\
         \x1b[3;2H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\
         \x1b[?25l"
    )
    .into_bytes();
    for protocol in [
        koshi_terminal::graphics::GraphicsProtocol::Sixel,
        koshi_terminal::graphics::GraphicsProtocol::Iterm2,
    ] {
        let paint = ImagePaint::new(
            PaneId::new(),
            4,
            Arc::new(koshi_terminal::graphics::ImageRecord {
                protocol,
                image: koshi_terminal::graphics::DecodedImage {
                    width: 1,
                    height: 1,
                    rgba: vec![1, 2, 3, 4],
                },
                action: koshi_terminal::graphics::ImageAction::Display,
                display: koshi_terminal::graphics::ImageDisplay::default(),
                anchor: (0, 0),
            }),
            Rect::new(1, 2, 1, 1),
            ImageSourceRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            0,
        );
        let mut output = Vec::new();
        let mut cache = KittyImageCache::default();

        write_complete_kitty_frame(&mut output, &mut cache, &[paint], None)
            .expect("decoded image output writes");

        assert_eq!(output, expected, "{protocol:?}");
    }
}

#[test]
fn kitty_writer_reuses_an_unchanged_placement_without_output() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 1,
            y: 2,
            width: 1,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut cache = KittyImageCache::default();
    let mut first = Vec::new();
    write_complete_kitty_frame(&mut first, &mut cache, std::slice::from_ref(&paint), None)
        .expect("the first Kitty frame writes");
    let mut second = Vec::new();

    write_kitty_frame(&mut second, &mut cache, &[paint], None)
        .expect("the repeated Kitty frame writes");

    assert_eq!(second, Vec::<u8>::new());
}

#[test]
fn kitty_writer_places_each_image_once_as_one_frame_finishes_uploading() {
    let pane_id = PaneId::new();
    let record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let first = ImagePaint::new(
        pane_id,
        1,
        Arc::clone(&record),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let second = ImagePaint::new(
        pane_id,
        2,
        record,
        Rect::new(1, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    write_complete_kitty_frame(&mut output, &mut cache, &[first, second], None)
        .expect("both Kitty images finish");

    let payload = encoded_zlib(&[1, 2, 3, 4]);
    assert_eq!(
        output,
        format!(
            "\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=0;{payload}\x1b\\\
             \x1b[1;1H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\
             \x1b[?25l\
             \x1b_Ga=t,f=32,s=1,v=1,I=2,q=2,o=z,m=0;{payload}\x1b\\\
             \x1b[1;2H\x1b_Ga=p,I=2,p=2,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\
             \x1b[?25l"
        )
        .into_bytes()
    );
}

#[test]
fn kitty_writer_uploads_shared_content_once_for_two_placements() {
    let pane_id = PaneId::new();
    let record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let first = ImagePaint::new(
        pane_id,
        1,
        Arc::clone(&record),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut second = ImagePaint::new(
        pane_id,
        2,
        record,
        Rect::new(1, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    second.content_id = first.content_id;
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    write_complete_kitty_frame(&mut output, &mut cache, &[first, second], None)
        .expect("both Kitty placements finish");

    let payload = encoded_zlib(&[1, 2, 3, 4]);
    assert_eq!(
        output,
        format!(
            "\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=0;{payload}\x1b\\\
             \x1b[1;1H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\
             \x1b[1;2H\x1b_Ga=p,I=1,p=2,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\
             \x1b[?25l"
        )
        .into_bytes()
    );
    assert_eq!(cache.images.len(), 1);
    assert_eq!(cache.placements.len(), 2);
}

#[test]
fn kitty_writer_rejects_one_content_identity_for_different_records() {
    let pane_id = PaneId::new();
    let record = |rgba| {
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba,
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        })
    };
    let paint = |placement_id, rgba| {
        ImagePaint::new(
            pane_id,
            placement_id,
            record(rgba),
            Rect::new(placement_id as u16 - 1, 0, 1, 1),
            ImageSourceRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            0,
        )
    };
    let first = paint(1, vec![1, 2, 3, 4]);
    let mut second = paint(2, vec![5, 6, 7, 8]);
    second.content_id = first.content_id;
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    let error = write_kitty_frame(&mut output, &mut cache, &[first, second], None)
        .expect_err("one content identity cannot name different records");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "image content identity names different pixel records"
    );
    assert_eq!(output, Vec::<u8>::new());
    assert!(cache.images.is_empty());
    assert!(cache.placements.is_empty());
}

#[test]
fn kitty_writer_deletes_one_shared_placement_without_deleting_its_pixels() {
    let pane_id = PaneId::new();
    let record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let first = ImagePaint::new(
        pane_id,
        1,
        Arc::clone(&record),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut second = ImagePaint::new(
        pane_id,
        2,
        record,
        Rect::new(1, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    second.content_id = first.content_id;
    let mut cache = KittyImageCache::default();
    write_complete_kitty_frame(&mut Vec::new(), &mut cache, &[first.clone(), second], None)
        .expect("both placements finish");
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &mut cache, &[first], None).expect("the smaller frame writes");

    assert_eq!(output, b"\x1b_Ga=d,d=n,I=1,p=2,q=2;\x1b\\");
    assert_eq!(cache.images.len(), 1);
    assert_eq!(cache.placements.len(), 1);
}

#[test]
fn kitty_writer_replaces_an_unsent_upload_with_the_newest_frame() {
    let pane_id = PaneId::new();
    let mut first = ImagePaint::new(
        pane_id,
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    first.content_id = 10;
    let mut second = first.clone();
    second.content_id = 11;
    second.record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![5, 6, 7, 8],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let mut cache = KittyImageCache::default();
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &mut cache, std::slice::from_ref(&first), None)
        .expect("the first upload is queued");
    write_kitty_frame(&mut output, &mut cache, std::slice::from_ref(&second), None)
        .expect("the newest upload replaces the unsent one");

    assert_eq!(output, Vec::<u8>::new());
    let upload = cache.upload.as_ref().expect("the newest upload is queued");
    assert_eq!(upload.content_id, 11);
    assert!(Arc::ptr_eq(&upload.record, &second.record));
    assert_eq!(upload.image_number, 2);
    assert!(!upload.transmission_started);
}

#[test]
fn kitty_writer_places_a_started_upload_at_the_newest_frame_position() {
    let pane_id = PaneId::new();
    let record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let first = ImagePaint::new(
        pane_id,
        4,
        Arc::clone(&record),
        Rect::new(1, 2, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let second = ImagePaint::new(
        pane_id,
        4,
        record,
        Rect::new(7, 8, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut cache = KittyImageCache::default();
    write_kitty_frame(&mut Vec::new(), &mut cache, &[first], None)
        .expect("the first frame queues its upload");
    let payload = zlib_bytes(&[1, 2, 3, 4]);
    let upload = cache.upload.as_mut().expect("the upload is queued");
    upload.input_offset = 4;
    upload.compressed = payload.clone();
    upload.compression_complete = true;
    upload.transmission_started = true;
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &mut cache, &[second], None)
        .expect("the newer frame keeps the started upload");
    advance_kitty_image(&mut output, &mut cache)
        .expect("the started upload finishes at the newer position");

    assert_eq!(
        output,
        format!(
            "\x1b_Gq=2,m=0;{}\x1b\\\
             \x1b[9;8H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\
             \x1b[?25l",
            STANDARD.encode(payload)
        )
        .into_bytes()
    );
    assert_eq!(cache.upload.as_ref().map(|upload| upload.content_id), None);
}

#[test]
fn kitty_writer_removes_a_cached_image_during_another_upload() {
    let pane_id = PaneId::new();
    let cached = one_cell_image_paint(pane_id, 1, [1, 2, 3, 4], Rect::new(0, 0, 1, 1));
    let uploading = one_cell_image_paint(pane_id, 2, [5, 6, 7, 8], Rect::new(1, 0, 1, 1));
    let mut cache = KittyImageCache::default();
    write_complete_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        std::slice::from_ref(&cached),
        None,
    )
    .expect("the cached image finishes");
    write_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        &[uploading.clone(), cached],
        None,
    )
    .expect("the second image is queued");
    cache
        .upload
        .as_mut()
        .expect("the second image is uploading")
        .transmission_started = true;
    let mut output = Vec::new();

    write_kitty_frame(
        &mut output,
        &mut cache,
        std::slice::from_ref(&uploading),
        None,
    )
    .expect("the cached image is removed");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=N,I=2,q=2;\x1b\\\x1b_Ga=d,d=N,I=1,q=2;\x1b\\"
    );
    let upload = cache.upload.as_ref().expect("the required image restarts");
    assert_eq!(upload.content_id, uploading.content_id);
    assert_eq!(upload.image_number, 3);
    assert!(!upload.transmission_started);
}

#[test]
fn kitty_writer_moves_a_cached_image_during_another_upload() {
    let pane_id = PaneId::new();
    let cached = one_cell_image_paint(pane_id, 1, [1, 2, 3, 4], Rect::new(0, 0, 1, 1));
    let uploading = one_cell_image_paint(pane_id, 2, [5, 6, 7, 8], Rect::new(1, 0, 1, 1));
    let mut moved = cached.clone();
    moved.target = Rect::new(7, 8, 1, 1);
    let mut cache = KittyImageCache::default();
    write_complete_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        std::slice::from_ref(&cached),
        None,
    )
    .expect("the cached image finishes");
    write_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        &[uploading.clone(), cached],
        None,
    )
    .expect("the second image is queued");
    cache
        .upload
        .as_mut()
        .expect("the second image is uploading")
        .transmission_started = true;
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &mut cache, &[uploading.clone(), moved], None)
        .expect("the cached image moves");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=N,I=2,q=2;\x1b\\\x1b[9;8H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l"
    );
    let upload = cache.upload.as_ref().expect("the required image restarts");
    assert_eq!(upload.content_id, uploading.content_id);
    assert_eq!(upload.image_number, 3);
    assert!(!upload.transmission_started);
}

#[test]
fn kitty_writer_invalidates_cache_when_an_upload_abort_write_fails() {
    let pane_id = PaneId::new();
    let cached = one_cell_image_paint(pane_id, 1, [1, 2, 3, 4], Rect::new(0, 0, 1, 1));
    let uploading = one_cell_image_paint(pane_id, 2, [5, 6, 7, 8], Rect::new(1, 0, 1, 1));
    let mut moved = cached.clone();
    moved.target = Rect::new(7, 8, 1, 1);
    let mut cache = KittyImageCache::default();
    write_complete_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        std::slice::from_ref(&cached),
        None,
    )
    .expect("the cached image finishes");
    write_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        &[uploading.clone(), cached],
        None,
    )
    .expect("the second image is queued");
    cache
        .upload
        .as_mut()
        .expect("the second image is uploading")
        .transmission_started = true;

    let error = write_kitty_frame(&mut FailingWriter, &mut cache, &[uploading, moved], None)
        .expect_err("the abort write fails");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(cache.images.is_empty());
    assert!(cache.placements.is_empty());
    assert!(cache.upload.is_none());
    assert!(cache.needs_reset);
    assert_eq!(cache.next_image_number, 1);
    assert_eq!(cache.next_placement_id, 1);
}

#[test]
fn kitty_writer_aborts_a_started_upload_replaced_by_a_new_record() {
    let pane_id = PaneId::new();
    let mut value = 1u32;
    let rgba = (0..(256 * 256 * 4))
        .map(|_| {
            value = value.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            value.to_be_bytes()[0]
        })
        .collect();
    let first = ImagePaint::new(
        pane_id,
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 256,
                height: 256,
                rgba,
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(1, 2, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 256,
            height: 256,
        },
        0,
    );
    let mut second = ImagePaint::new(
        pane_id,
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Sixel,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![5, 6, 7, 8],
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(7, 8, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    second.content_id = 5;
    let mut cache = KittyImageCache::default();
    let mut output = Vec::new();
    write_kitty_frame(&mut output, &mut cache, &[first], None)
        .expect("the first frame queues its upload");
    advance_kitty_image(&mut output, &mut cache).expect("the first upload sends one slice");
    let upload = cache
        .upload
        .as_ref()
        .expect("the large first upload remains open after one slice");
    assert!(upload.transmission_started);
    output.clear();

    write_kitty_frame(&mut output, &mut cache, std::slice::from_ref(&second), None)
        .expect("the replacement frame aborts the old upload");

    assert_eq!(output, KITTY_DELETE_ALL);
    let upload = cache
        .upload
        .as_ref()
        .expect("the replacement upload starts");
    assert_eq!(upload.content_id, 5);
    assert!(Arc::ptr_eq(&upload.record, &second.record));
    assert_eq!(upload.image_number, 1);
    assert!(!upload.transmission_started);
    assert!(!cache.needs_reset);
}

#[test]
fn kitty_writer_deletes_pixel_data_when_a_placement_leaves_the_frame() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut cache = KittyImageCache::default();
    write_complete_kitty_frame(&mut Vec::new(), &mut cache, &[paint], None)
        .expect("the image frame writes");
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &mut cache, &[], None)
        .expect("the frame without the image writes");

    assert_eq!(output, b"\x1b_Ga=d,d=N,I=1,q=2;\x1b\\");
    assert!(cache.images.is_empty());
}

#[test]
fn kitty_writer_replaces_pixels_but_keeps_the_placement_identity() {
    let pane_id = PaneId::new();
    let mut first = ImagePaint::new(
        pane_id,
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    first.content_id = 1;
    let mut second = first.clone();
    second.content_id = 2;
    second.record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![5, 6, 7, 8],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let mut cache = KittyImageCache::default();
    write_complete_kitty_frame(&mut Vec::new(), &mut cache, &[first], None)
        .expect("the first image frame writes");
    let mut output = Vec::new();

    write_complete_kitty_frame(&mut output, &mut cache, &[second], None)
        .expect("the replacement image frame writes");

    let payload = encoded_zlib(&[5, 6, 7, 8]);
    assert_eq!(
        output,
        format!("\x1b_Ga=d,d=N,I=1,q=2;\x1b\\\x1b_Ga=t,f=32,s=1,v=1,I=2,q=2,o=z,m=0;{payload}\x1b\\\x1b[1;1H\x1b_Ga=p,I=2,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l").into_bytes()
    );
    let cached = cache.images.get(&2).expect("the replacement is cached");
    assert_eq!(cached.image_number, 2);
    assert_eq!(cache.placements[&(pane_id, 4)].id, 1);
    assert_eq!(cache.placements[&(pane_id, 4)].content_id, 2);
}

#[test]
fn kitty_writer_resets_and_reuploads_when_nonzero_ids_are_exhausted() {
    let record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        },
        action: koshi_terminal::graphics::ImageAction::Display,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let first = ImagePaint::new(
        PaneId::new(),
        1,
        Arc::clone(&record),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let second = ImagePaint::new(
        PaneId::new(),
        2,
        record,
        Rect::new(1, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut cache = KittyImageCache {
        next_image_number: u32::MAX,
        next_placement_id: u32::MAX,
        ..KittyImageCache::default()
    };
    write_complete_kitty_frame(
        &mut Vec::new(),
        &mut cache,
        std::slice::from_ref(&first),
        None,
    )
    .expect("the last available ids are used");
    let mut output = Vec::new();

    write_complete_kitty_frame(&mut output, &mut cache, &[first, second], None)
        .expect("the exhausted cache resets and paints both images");

    assert!(output.starts_with(b"\x1b_Ga=d,d=A,q=2;\x1b\\"));
    assert_eq!(cache.images.len(), 2);
    assert_eq!(cache.next_image_number, 3);
    assert_eq!(cache.next_placement_id, 3);
}

#[test]
fn kitty_writer_emits_first_cell_offsets_and_hides_an_absent_cursor() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay {
                cell_offset_x: Some(4),
                cell_offset_y: Some(5),
                ..koshi_terminal::graphics::ImageDisplay::default()
            },
            anchor: (0, 0),
        }),
        Rect {
            x: 1,
            y: 2,
            width: 1,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    write_complete_kitty_frame(&mut output, &mut cache, &[paint], None)
        .expect("Kitty output writes");

    let payload = encoded_zlib(&[1, 2, 3, 4]);
    assert_eq!(
        output,
        format!("\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=0;{payload}\x1b\\\x1b[3;2H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=1,h=1,X=4,Y=5,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l").into_bytes()
    );
}

#[test]
fn native_image_write_failure_reaches_the_paint_caller() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
    });
    let client = test_client(&mut server, client_id);
    let snapshot = frame(&server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut writer = FailingWriter;
    let mut images = KittyImageCache::default();
    let committed_regions = regions(VIEWPORT);
    let paints = image_paints(&snapshot, &committed_regions, Rect::new(0, 0, 80, 24));
    write_complete_kitty_frame(&mut Vec::new(), &mut images, &paints, None)
        .expect("the terminal cache is primed");
    let placement = images
        .placements
        .values_mut()
        .next()
        .expect("the image has one cached placement");
    placement.target.x = placement.target.x.checked_add(1).expect("one spare column");

    let error = paint_frame_with_writer(
        &mut writer,
        &mut terminal,
        &client,
        &snapshot,
        &committed_regions,
        &ViewerPaint::from_frame(&client, &snapshot),
        ImageRenderMode::Native,
        &mut images,
        &mut String::new(),
        &mut None,
    )
    .expect_err("a failed native image writer rejects the frame");

    match error {
        PaintError::Image(error) => assert_eq!(error.kind(), io::ErrorKind::BrokenPipe),
        PaintError::Backend(error) => panic!("unexpected backend error: {error:?}"),
    }
}

#[test]
fn kitty_writer_clears_cached_ids_after_an_image_write_failure() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 1,
            y: 2,
            width: 1,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut writer = FailOnWrite {
        fail_at: 0,
        writes: 0,
        output: Vec::new(),
    };
    let mut cache = KittyImageCache::default();

    write_kitty_frame(&mut Vec::new(), &mut cache, &[paint], None).expect("the upload is queued");
    let error = advance_kitty_image(&mut writer, &mut cache)
        .expect_err("a failed image write should reach the caller");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(writer.output, Vec::<u8>::new());
    assert!(cache.images.is_empty());
    assert!(cache.needs_reset);
}

#[test]
fn kitty_writer_flush_failure_reaches_the_paint_caller() {
    let mut writer = FailingFlushWriter { output: Vec::new() };
    let mut cache = KittyImageCache::default();
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    write_kitty_frame(&mut Vec::new(), &mut cache, &[paint], None).expect("the upload is queued");

    let error = advance_kitty_image(&mut writer, &mut cache).expect_err("flush failure");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(writer
        .output
        .starts_with(b"\x1b_Ga=t,f=32,s=1,v=1,I=1,q=2,o=z,m=0;"));
    assert!(cache.needs_reset);
}

#[test]
fn kitty_writer_rejects_a_source_rectangle_outside_rgba_pixels() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![255, 0, 0, 255],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        ImageSourceRect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    let error = write_kitty_frame(&mut output, &mut cache, &[paint], None)
        .expect_err("invalid source is rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(output, Vec::<u8>::new());
    assert!(cache.images.is_empty());
}

#[test]
fn kitty_writer_splits_zlib_bytes_at_the_protocol_chunk_limit() {
    let mut state = 0x1234_5678u32;
    let rgba: Vec<u8> = (0..16_384)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 4_096,
                height: 1,
                rgba: rgba.clone(),
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 0,
            y: 0,
            width: 4_096,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 4_096,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    write_complete_kitty_frame(&mut output, &mut cache, &[paint], None)
        .expect("Kitty output writes");

    let compressed = zlib_bytes(&rgba);
    assert!(compressed.len() > KITTY_IMAGE_CHUNK_BYTES);
    let mut expected = Vec::new();
    let chunk_count = compressed.chunks(KITTY_IMAGE_CHUNK_BYTES).count();
    for (index, bytes) in compressed.chunks(KITTY_IMAGE_CHUNK_BYTES).enumerate() {
        let more = u8::from(index + 1 < chunk_count);
        if index == 0 {
            write!(expected, "\x1b_Ga=t,f=32,s=4096,v=1,I=1,q=2,o=z,m={more};")
                .expect("first header writes");
        } else {
            write!(expected, "\x1b_Gq=2,m={more};").expect("continuation header writes");
        }
        expected.extend_from_slice(STANDARD.encode(bytes).as_bytes());
        expected.extend_from_slice(b"\x1b\\");
    }
    expected.extend_from_slice(
        b"\x1b[1;1H\x1b_Ga=p,I=1,p=1,x=0,y=0,w=4096,h=1,c=4096,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l",
    );
    assert_eq!(output, expected);
}

#[test]
fn kitty_writer_limits_one_compression_step_to_256_kib_of_rgba() {
    assert_eq!(KITTY_COMPRESSION_INPUT_BYTES_PER_STEP, 262_144);
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 16_384,
                height: 32,
                rgba: vec![0; 2_097_152],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();
    let mut cache = KittyImageCache::default();

    write_kitty_frame(&mut output, &mut cache, &[paint], None).expect("the upload is queued");
    advance_kitty_image(&mut output, &mut cache).expect("one bounded slice writes");

    let upload = cache
        .upload
        .as_ref()
        .expect("more compression input is pending");
    assert_eq!(upload.input_offset, KITTY_COMPRESSION_INPUT_BYTES_PER_STEP);
    assert!(!upload.compression_complete);
    assert!(kitty_image_work_pending(&cache));
}

#[test]
fn kitty_writer_limits_one_output_step_to_sixteen_chunks() {
    assert_eq!(KITTY_IMAGE_CHUNKS_PER_STEP, 16);
    let record = Arc::new(koshi_terminal::graphics::ImageRecord {
        protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
        image: koshi_terminal::graphics::DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        },
        action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
        display: koshi_terminal::graphics::ImageDisplay::default(),
        anchor: (0, 0),
    });
    let mut upload = KittyUpload {
        content_id: 9,
        record,
        image_number: 7,
        compressor: Compress::new(Compression::fast(), true),
        input_offset: 4,
        compressed: vec![0x5a; KITTY_IMAGE_CHUNK_BYTES * (KITTY_IMAGE_CHUNKS_PER_STEP + 1)],
        compressed_offset: 0,
        compression_complete: true,
        transmission_started: false,
    };
    let mut output = Vec::new();

    write_kitty_upload_chunks(&mut output, &mut upload).expect("one output slice writes");

    assert_eq!(
        upload.compressed_offset,
        KITTY_IMAGE_CHUNK_BYTES * KITTY_IMAGE_CHUNKS_PER_STEP
    );
    assert!(upload.transmission_started);
    assert!(!kitty_upload_complete(&upload));
    assert_eq!(
        output
            .windows(b"\x1b_G".len())
            .filter(|bytes| *bytes == b"\x1b_G")
            .count(),
        KITTY_IMAGE_CHUNKS_PER_STEP
    );
    assert!(output.starts_with(b"\x1b_Ga=t,f=32,s=1,v=1,I=7,q=2,o=z,m=1;"));
    assert!(!output.windows(b"m=0;".len()).any(|bytes| bytes == b"m=0;"));
}

#[test]
#[ignore = "release performance benchmark"]
fn benchmark_kitty_upload_slices() {
    const WIDTH: u32 = 2_048;
    const HEIGHT: u32 = 1_338;
    const RUNS: usize = 6;

    let mut state = 0x1234_5678u32;
    let rgba: Vec<u8> = (0..u64::from(WIDTH) * u64::from(HEIGHT) * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let paint = ImagePaint::new(
        PaneId::new(),
        1,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: WIDTH,
                height: HEIGHT,
                rgba,
            },
            action: koshi_terminal::graphics::ImageAction::Display,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect::new(0, 0, 1, 1),
        ImageSourceRect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        },
        0,
    );
    let mut totals = Vec::with_capacity(RUNS - 1);
    let mut longest_steps = Vec::with_capacity(RUNS - 1);

    for run in 0..RUNS {
        let mut cache = KittyImageCache::default();
        write_kitty_frame(
            &mut io::sink(),
            &mut cache,
            std::slice::from_ref(&paint),
            None,
        )
        .expect("the benchmark image is queued");
        let started = Instant::now();
        let mut longest_step = Duration::ZERO;
        while kitty_image_work_pending(&cache) {
            let step_started = Instant::now();
            advance_kitty_image(&mut io::sink(), &mut cache).expect("the benchmark image advances");
            longest_step = longest_step.max(step_started.elapsed());
        }
        if run != 0 {
            totals.push(started.elapsed());
            longest_steps.push(longest_step);
        }
    }

    totals.sort_unstable();
    longest_steps.sort_unstable();
    println!(
        "Kitty 10.45 MiB upload: median total {:?}, median longest step {:?}",
        totals[totals.len() / 2],
        longest_steps[longest_steps.len() / 2]
    );
}

#[test]
fn unsupported_paint_writes_no_terminal_image_output() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, pane_id) = boot(&fake);
    let _ = server.handle_runtime_event(RuntimeEvent::PtyOutput {
        pane_id,
        bytes: b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
    });
    let client = test_client(&mut server, client_id);
    let snapshot = frame(&server, client_id);
    assert_eq!(snapshot.panes[0].image_placements.len(), 1);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut output = Vec::new();
    let mut last_title = window_title(&snapshot);
    let mut last_cursor = cursor_style(&snapshot);
    let mut images = KittyImageCache::default();

    paint_frame_with_writer(
        &mut output,
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        ImageRenderMode::Placeholder,
        &mut images,
        &mut last_title,
        &mut last_cursor,
    )
    .expect("placeholder paint");

    assert_eq!(output, Vec::<u8>::new());
}

#[test]
fn image_cleanup_has_one_owner() {
    let claimed = AtomicBool::new(false);

    assert!(claim_image_cleanup(&claimed));
    assert!(!claim_image_cleanup(&claimed));
}
