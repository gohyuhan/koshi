//! Events received from the terminal that contains the Koshi client.
//!
//! [`Parser`] converts terminal input bytes into keys, mouse events, pasted
//! text, focus changes, and terminal capability replies. For example,
//! `ESC [ 1 ; 5 C` becomes a Right key with Control held.

use std::ops::{BitOr, BitOrAssign};

use koshi_core::key::KeyModifierFlags;

pub use koshi_core::key::KeyEventKind;

mod parser;

pub use parser::Parser;

/// Modifier keys reported with one key or mouse event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifier key.
    pub const NONE: Self = Self(0);
    /// Shift.
    pub const SHIFT: Self = Self(1 << 0);
    /// Alt or Option.
    pub const ALT: Self = Self(1 << 1);
    /// Control.
    pub const CONTROL: Self = Self(1 << 2);
    /// Super, Command, or Windows.
    pub const SUPER: Self = Self(1 << 3);
    /// Hyper.
    pub const HYPER: Self = Self(1 << 4);
    /// Meta.
    pub const META: Self = Self(1 << 5);
    /// Caps Lock, reported as a held state rather than a press.
    pub const CAPS_LOCK: Self = Self(1 << 6);
    /// Num Lock, reported as a held state rather than a press.
    pub const NUM_LOCK: Self = Self(1 << 7);

    /// Return an empty modifier set.
    #[must_use]
    pub const fn empty() -> Self {
        Self::NONE
    }

    /// Return whether every bit in `required_modifiers` is set.
    #[must_use]
    pub const fn has_all_modifiers(self, required_modifiers: Self) -> bool {
        self.0 & required_modifiers.0 == required_modifiers.0
    }

    /// Return the union of two modifier sets.
    #[must_use]
    pub const fn combine_modifiers(self, additional_modifiers: Self) -> Self {
        Self(self.0 | additional_modifiers.0)
    }

    /// Return these modifiers as the stored bitmap a complete keyboard event
    /// carries. The two sets use the same bit for the same modifier.
    #[must_use]
    pub const fn to_key_modifier_flags(self) -> KeyModifierFlags {
        KeyModifierFlags::from_bits(self.0)
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, right_modifiers: Self) -> Self::Output {
        self.combine_modifiers(right_modifiers)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, right_modifiers: Self) {
        self.0 |= right_modifiers.0;
    }
}

/// A key identity that Koshi can bind or send to a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A Unicode character.
    Char(char),
    /// Return or Enter.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Escape.
    Escape,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Shift+Tab.
    BackTab,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete.
    Delete,
    /// Function key number.
    Function(u8),
    /// A reported key with no Koshi key form, holding the codepoint the
    /// terminal sent. Left Shift is `57441`, Menu is `57363`, and `0` means
    /// the event carries only text.
    Codepoint(u32),
}

/// One parsed key event, holding everything the terminal reported about it.
///
/// The terminal reports the alternatives and the text only when the Kitty
/// keyboard protocol enhancements that carry them are active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// Key identity.
    pub code: KeyCode,
    /// Physical action.
    pub key_event_kind: KeyEventKind,
    /// Active modifiers.
    pub modifiers: Modifiers,
    /// The character this key produces with Shift, when reported.
    pub shifted_key: Option<char>,
    /// The character this key produces on the base layout, when reported.
    pub base_layout_key: Option<char>,
    /// The text this key produced, empty when none was reported.
    pub associated_text: String,
}

impl KeyEvent {
    /// Build a key press with no alternatives and no text.
    #[must_use]
    pub fn from_key_code_and_modifiers(code: KeyCode, modifiers: Modifiers) -> Self {
        Self {
            code,
            key_event_kind: KeyEventKind::Press,
            modifiers,
            shifted_key: None,
            base_layout_key: None,
            associated_text: String::new(),
        }
    }
}

impl From<KeyCode> for KeyEvent {
    fn from(code: KeyCode) -> Self {
        Self::from_key_code_and_modifiers(code, Modifiers::NONE)
    }
}

/// A mouse button supported by Koshi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Middle button.
    Middle,
    /// Right button.
    Right,
}

/// The action represented by one mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    /// A button was pressed.
    Down(MouseButton),
    /// A button was released.
    Up(MouseButton),
    /// The pointer moved with a button held.
    Drag(MouseButton),
    /// The pointer moved with no button held.
    Moved,
    /// The wheel moved up.
    ScrollUp,
    /// The wheel moved down.
    ScrollDown,
    /// The wheel moved left.
    ScrollLeft,
    /// The wheel moved right.
    ScrollRight,
}

/// One mouse event with zero-based cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Mouse action.
    pub mouse_event_kind: MouseEventKind,
    /// Zero-based column.
    pub column: u16,
    /// Zero-based row.
    pub row: u16,
    /// Active modifiers.
    pub modifiers: Modifiers,
}

/// The host terminal window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    /// Cell columns.
    pub column_count: u16,
    /// Cell row count.
    pub row_count: u16,
    /// Pixel width when the platform reports it.
    pub pixel_width: Option<u16>,
    /// Pixel height when the platform reports it.
    pub pixel_height: Option<u16>,
}

/// A Kitty graphics answer for one image id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyGraphicsReply {
    /// Image id copied from the query.
    pub image_id: u32,
    /// Whether the terminal returned `OK`.
    pub is_successful: bool,
}

/// The result of one XTSMGRAPHICS item query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphicAttributeReply {
    /// The terminal's color-register count, or the reported error.
    Palette(Result<u32, GraphicAttributeError>),
    /// The terminal's Sixel geometry in pixels, or the reported error.
    Geometry(Result<(u32, u32), GraphicAttributeError>),
}

/// The status reported by an XTSMGRAPHICS reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicAttributeError {
    /// The terminal rejected the requested item number.
    InvalidItem,
    /// The terminal rejected the requested action.
    InvalidAction,
    /// The terminal could not complete the request.
    Failure,
}

/// One complete host-terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Keyboard input.
    Key(KeyEvent),
    /// Mouse input.
    Mouse(MouseEvent),
    /// Window-size change.
    WindowResized(WindowSize),
    /// Measured pixel dimensions of one terminal cell.
    CellSize(koshi_core::geometry::PixelCellSize),
    /// Text wrapped by bracketed-paste markers.
    Paste(String),
    /// The terminal gained focus.
    FocusIn,
    /// The terminal lost focus.
    FocusOut,
    /// A primary device-attributes answer and its numeric parameters.
    PrimaryDeviceAttributes(Vec<u32>),
    /// An iTerm2 inline-image capability answer and its raw feature string.
    TerminalFeatures(Vec<u8>),
    /// A Sixel graphics-attribute answer.
    SixelGraphicsAttributeReply(GraphicAttributeReply),
    /// A Kitty graphics answer.
    KittyGraphicsReply(KittyGraphicsReply),
    /// The Kitty keyboard enhancements the terminal reports as active, as the
    /// flag bits of its `CSI ? flags u` answer.
    KeyboardEnhancementFlags(u8),
}
