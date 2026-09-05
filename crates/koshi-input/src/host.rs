//! Events received from the terminal that contains the Koshi client.
//!
//! [`Parser`] converts terminal input bytes into keys, mouse events, pasted
//! text, focus changes, and the two replies used during capability detection.

use std::ops::{BitOr, BitOrAssign};

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

    /// Return an empty modifier set.
    #[must_use]
    pub const fn empty() -> Self {
        Self::NONE
    }

    /// Return whether every bit in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Return the union of two modifier sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
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
    /// A protocol key that has no Koshi key form.
    Unsupported,
}

/// The physical action represented by one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    /// A key was pressed.
    Press,
    /// A held key repeated.
    Repeat,
    /// A key was released.
    Release,
}

/// One parsed key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Key identity.
    pub code: KeyCode,
    /// Physical action.
    pub kind: KeyEventKind,
    /// Active modifiers.
    pub modifiers: Modifiers,
}

impl KeyEvent {
    /// Build a key press.
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Self {
            code,
            kind: KeyEventKind::Press,
            modifiers,
        }
    }
}

impl From<KeyCode> for KeyEvent {
    fn from(code: KeyCode) -> Self {
        Self::new(code, Modifiers::NONE)
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
    pub kind: MouseEventKind,
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
    pub cols: u16,
    /// Cell rows.
    pub rows: u16,
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
    pub ok: bool,
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
    /// Text wrapped by bracketed-paste markers.
    Paste(String),
    /// The terminal gained focus.
    FocusIn,
    /// The terminal lost focus.
    FocusOut,
    /// A primary device-attributes answer.
    PrimaryDeviceAttributes,
    /// A Kitty graphics answer.
    KittyGraphicsReply(KittyGraphicsReply),
}
