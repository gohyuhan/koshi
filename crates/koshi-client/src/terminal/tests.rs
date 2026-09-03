//! Tests for the outer terminal an attached client owns: the viewer built for
//! it, painting a frame, the cursor-style mapping, and the window title. A fake PTY
//! backend stands in for real children and ratatui's `TestBackend` renders into
//! an in-memory buffer, so painting runs without a terminal. The crossterm
//! terminal I/O and the input thread's `event::read` are TTY-bound and out of
//! reach here; key decoding is covered in `keys::tests`.

use super::*;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::SystemTime;

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
fn graphics_support_selects_kitty_only_from_kitty_environment() {
    assert_eq!(
        graphics_support_from_environment(Some("xterm-kitty"), false),
        GraphicsSupport::Kitty
    );
    assert_eq!(
        graphics_support_from_environment(Some("screen"), true),
        GraphicsSupport::Kitty
    );
    assert_eq!(
        graphics_support_from_environment(Some("xterm-256color"), false),
        GraphicsSupport::Unsupported
    );
    assert_eq!(
        graphics_support_from_environment(None, false),
        GraphicsSupport::Unsupported
    );
}

#[test]
fn kitty_writer_emits_cropped_rgba_and_restores_cursor() {
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

    write_kitty_frame(&mut output, &[paint], Some(Position::new(8, 9)))
        .expect("Kitty output writes");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[5;4H\x1b_Ga=T,f=32,s=2,v=1,c=2,r=1,C=1,z=-2,q=2,m=0;CQoLDA0ODxA=\x1b\\\x1b[10;9H"
    );
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

    write_kitty_frame(&mut output, &[paint], None).expect("Kitty output writes");

    assert_eq!(
        output,
        b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[3;2H\x1b_Ga=T,f=32,s=1,v=1,X=4,Y=5,c=1,r=1,C=1,z=0,q=2,m=0;AQIDBA==\x1b\\\x1b[?25l"
    );
}

#[test]
fn native_image_write_failure_reaches_the_paint_caller() {
    let fake = Arc::new(FakePtyBackend::new());
    let (mut server, client_id, _pane_id) = boot(&fake);
    let client = test_client(&mut server, client_id);
    let snapshot = frame(&server, client_id);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let mut writer = FailingWriter;

    let error = paint_frame_with_writer(
        &mut writer,
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        ImageRenderMode::Native,
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
fn kitty_writer_hides_an_absent_cursor_after_an_image_write_failure() {
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
        fail_at: 1,
        writes: 0,
        output: Vec::new(),
    };

    let error = write_kitty_frame(&mut writer, &[paint], None)
        .expect_err("a failed image write should reach the caller");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(writer.output, b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?25l");
}

#[test]
fn kitty_writer_flush_failure_reaches_the_paint_caller() {
    let mut writer = FailingFlushWriter { output: Vec::new() };

    let error = write_kitty_frame(&mut writer, &[], None).expect_err("flush failure");

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(writer.output, b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?25l");
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

    let error = write_kitty_image(&mut output, &paint).expect_err("invalid source is rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(output, Vec::<u8>::new());
}

#[test]
fn kitty_writer_splits_rgba_at_the_protocol_chunk_limit() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 1_024,
                height: 1,
                rgba: vec![0; 4_096],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 0,
            y: 0,
            width: 1_024,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 1_024,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &[paint], None).expect("Kitty output writes");

    let first = STANDARD.encode(vec![0; KITTY_IMAGE_CHUNK_BYTES]);
    let second = STANDARD.encode(vec![0; 4_096 - KITTY_IMAGE_CHUNK_BYTES]);
    let expected = format!(
        "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[1;1H\x1b_Ga=T,f=32,s=1024,v=1,c=1024,r=1,C=1,z=0,q=2,m=1;{first}\x1b\\\x1b_Gm=0;{second}\x1b\\\x1b[?25l"
    );
    assert_eq!(output, expected.into_bytes());
}

#[test]
fn kitty_writer_marks_an_exact_chunk_boundary_as_the_final_chunk() {
    let paint = ImagePaint::new(
        PaneId::new(),
        4,
        Arc::new(koshi_terminal::graphics::ImageRecord {
            protocol: koshi_terminal::graphics::GraphicsProtocol::Kitty,
            image: koshi_terminal::graphics::DecodedImage {
                width: 768,
                height: 1,
                rgba: vec![0; KITTY_IMAGE_CHUNK_BYTES],
            },
            action: koshi_terminal::graphics::ImageAction::TransmitAndDisplay,
            display: koshi_terminal::graphics::ImageDisplay::default(),
            anchor: (0, 0),
        }),
        Rect {
            x: 0,
            y: 0,
            width: 768,
            height: 1,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 768,
            height: 1,
        },
        0,
    );
    let mut output = Vec::new();

    write_kitty_frame(&mut output, &[paint], None).expect("Kitty output writes");

    let payload = STANDARD.encode(vec![0; KITTY_IMAGE_CHUNK_BYTES]);
    let expected = format!(
        "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[1;1H\x1b_Ga=T,f=32,s=768,v=1,c=768,r=1,C=1,z=0,q=2,m=0;{payload}\x1b\\\x1b[?25l"
    );
    assert_eq!(output, expected.into_bytes());
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

    paint_frame_with_writer(
        &mut output,
        &mut terminal,
        &client,
        &snapshot,
        &regions(VIEWPORT),
        &ViewerPaint::from_frame(&client, &snapshot),
        ImageRenderMode::Placeholder,
        &mut last_title,
        &mut last_cursor,
    )
    .expect("placeholder paint");

    assert_eq!(output, Vec::<u8>::new());
}
