//! Semantic styles. Only the 16 ANSI colors are used so every terminal renders them.

use mate::mqtt::ConnectionState;
use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::Gray;
pub const SUBTLE: Color = Color::DarkGray;

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn subtle() -> Style {
    Style::default().fg(SUBTLE)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn selection() -> Style {
    Style::default().fg(Color::Black).bg(ACCENT)
}

/// Selected row in a pane that is not focused.
pub fn selection_inactive() -> Style {
    Style::default().fg(Color::White).bg(SUBTLE)
}

pub fn pane_border(active: bool) -> Style {
    Style::default().fg(if active { ACCENT } else { SUBTLE })
}

/// The marker of a broker slot (1-based); slots repeat their colors after nine brokers.
pub fn broker_style(slot: usize) -> Style {
    const COLORS: [Color; 9] = [
        Color::Magenta,
        Color::Yellow,
        Color::Blue,
        Color::LightRed,
        Color::LightCyan,
        Color::LightGreen,
        Color::LightMagenta,
        Color::LightYellow,
        Color::LightBlue,
    ];
    Style::default()
        .fg(COLORS[slot.saturating_sub(1) % COLORS.len()])
        .add_modifier(Modifier::BOLD)
}

pub fn state_color(state: ConnectionState) -> Color {
    match state {
        ConnectionState::Connected => OK,
        ConnectionState::Connecting | ConnectionState::Reconnecting => WARN,
        ConnectionState::Disconnected => MUTED,
        ConnectionState::Error => ERROR,
    }
}

pub fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected => "Connected",
        ConnectionState::Connecting => "Connecting",
        ConnectionState::Reconnecting => "Reconnecting",
        ConnectionState::Disconnected => "Disconnected",
        ConnectionState::Error => "Error",
    }
}
