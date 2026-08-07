//! Colour palette and reusable styles.
//!
//! The palette is a warm-charcoal dark theme: near-black background, muted grey
//! text and a single amber accent used for focus, selection and key hints.

use ratatui::style::{Color, Modifier, Style};

pub struct Theme;

impl Theme {
    // ── palette ──────────────────────────────────────────────────────────
    pub const BG: Color = Color::Rgb(0x1a, 0x1b, 0x1e);
    /// Background of the selected row.
    pub const SURFACE: Color = Color::Rgb(0x2c, 0x2f, 0x36);
    /// Background of the selected row in an unfocused pane. Subordinate to
    /// `SURFACE`, but still readable — it is the only cue marking the entry
    /// an ancestor column was opened through.
    pub const SURFACE_DIM: Color = Color::Rgb(0x26, 0x29, 0x30);
    pub const BORDER: Color = Color::Rgb(0x45, 0x48, 0x50);
    pub const BORDER_FOCUS: Color = Color::Rgb(0x6d, 0x72, 0x7d);

    pub const FG: Color = Color::Rgb(0xc8, 0xcb, 0xd0);
    pub const DIM: Color = Color::Rgb(0x6b, 0x70, 0x79);
    pub const FAINT: Color = Color::Rgb(0x4e, 0x52, 0x5a);

    pub const ACCENT: Color = Color::Rgb(0xe8, 0xa3, 0x3d);
    pub const GREEN: Color = Color::Rgb(0x8f, 0xbf, 0x6b);
    pub const RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);
    pub const PURPLE: Color = Color::Rgb(0xb2, 0x8d, 0xd8);

    // ── styles ───────────────────────────────────────────────────────────
    pub fn base() -> Style {
        Style::new().bg(Self::BG).fg(Self::FG)
    }

    pub fn dim() -> Style {
        Style::new().fg(Self::DIM)
    }

    pub fn faint() -> Style {
        Style::new().fg(Self::FAINT)
    }

    pub fn accent() -> Style {
        Style::new().fg(Self::ACCENT)
    }

    pub fn accent_bold() -> Style {
        Style::new().fg(Self::ACCENT).add_modifier(Modifier::BOLD)
    }

    pub fn error() -> Style {
        Style::new().fg(Self::RED)
    }

    pub fn border(focused: bool) -> Style {
        Style::new().fg(if focused {
            Self::BORDER_FOCUS
        } else {
            Self::BORDER
        })
    }

    /// Pane title: amber when the pane has focus, muted otherwise.
    pub fn title(focused: bool) -> Style {
        if focused {
            Style::new().fg(Self::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Self::DIM).add_modifier(Modifier::BOLD)
        }
    }

    /// Selected row: a background bar only.
    ///
    /// ratatui paints `highlight_style` over the row after it renders, so
    /// setting `fg` here would flatten every row's own colour — amber
    /// directories, the green default branch, purple tags — into one grey.
    /// Only the background changes, and the row keeps its meaning.
    pub fn selection(focused: bool) -> Style {
        if focused {
            Style::new().bg(Self::SURFACE).add_modifier(Modifier::BOLD)
        } else {
            Style::new().bg(Self::SURFACE_DIM)
        }
    }

    /// Inverted chip used for key hints and the active tab.
    pub fn chip() -> Style {
        Style::new()
            .bg(Self::ACCENT)
            .fg(Self::BG)
            .add_modifier(Modifier::BOLD)
    }

    /// Colour for a directory / common-prefix entry.
    pub fn directory() -> Style {
        Style::new().fg(Self::ACCENT)
    }

    pub fn file() -> Style {
        Style::new().fg(Self::FG)
    }

    /// Syntax colour for a JSON token in the preview pane.
    pub fn json(tok: crate::app::JsonTok) -> Style {
        use crate::app::JsonTok::*;
        match tok {
            Key => Style::new().fg(Self::ACCENT),
            Str => Style::new().fg(Self::GREEN),
            Num => Style::new().fg(Self::PURPLE),
            Bool => Style::new().fg(Self::PURPLE).add_modifier(Modifier::BOLD),
            Null => Style::new().fg(Self::FAINT).add_modifier(Modifier::ITALIC),
            Punct => Style::new().fg(Self::DIM),
        }
    }
}
