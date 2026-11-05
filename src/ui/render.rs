//! All drawing code. Reads `UiState`; never touches MQTT.

mod replay;
mod stream;

use std::time::{Duration, Instant, UNIX_EPOCH};

use chrono::{DateTime, Local};
use mate::{
    analytics::PayloadKind,
    config::BrokerConfig,
    diff::DiffLine,
    json_tree::{JsonRow, NodeKind},
    mqtt::ConnectionState,
    preview::{Preview, PreviewKind},
    topics::{Message, TopicRow},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap,
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    CertificateEditMode, Confirmation, DataSource, TestOutcome, TlsStatus, UiState, View,
    analytics::{AnalyticsView, Mode as AnalyticsMode, Row as AnalyticsRow, SortKey},
    help::{self, Block as HelpBlock, HelpTab},
    info::duration_text,
    inspector::{HEX_BYTES_PER_LINE, ViewMode},
    layout::Panel,
    main_view::{clock, history_messages},
    publish::{FIELD_PAYLOAD, FIELD_QOS, FIELD_RECENT, FIELD_RETAIN, FIELD_TOPIC, PublishStage},
    theme,
};
use mate::{certificates::CertificateKind, snake};

/// A topic row keeps at least this much of the preview when the message count needs room.
const MIN_PREVIEW_WIDTH: usize = 3;

/// The review screen shows this many payload lines before summarizing the rest.
const REVIEW_PAYLOAD_LINES: usize = 8;

/// A message younger than this counts as recently updated.
const RECENT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn render(frame: &mut Frame, state: &mut UiState) {
    let area = frame.area();
    state.main.terminal_width = area.width;
    if (state.main.focus == Panel::History && !state.main.layout.show_history)
        || (state.main.focus == Panel::Publish && !state.main.layout.show_publish)
    {
        state.main.focus = Panel::Inspector;
    }
    state.prepare_frame();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(frame, state, rows[0]);
    match state.view {
        View::Main => render_main(frame, state, rows[1]),
        View::Brokers => render_broker_manager(frame, state, rows[1]),
        View::Subscriptions => render_subscription_manager(frame, state, rows[1]),
        View::Certificates => render_certificate_manager(frame, state, rows[1]),
        View::Analytics => render_analytics(frame, state, rows[1]),
        View::Stream => stream::render_stream(frame, state, rows[1]),
        View::Replay => replay::render_replay(frame, state, rows[1]),
    }
    render_shortcuts(frame, state, rows[2]);

    if state.help_open {
        render_help(frame, state, centered_rect(80, 88, area));
    }
    if state.editor.is_some() {
        render_broker_editor(frame, state, centered_rect(72, 70, area));
    }
    if state.log_dialog.is_some() {
        render_log_dialog(frame, state, centered_rect(76, 56, area));
    }
    if state.certificate_editor.is_some() {
        render_certificate_editor(frame, state, centered_rect(72, 50, area));
    }
    if state.confirmation.is_some() && !matches!(state.confirmation, Some(Confirmation::Quit)) {
        render_confirmation(frame, state, centered_rect(60, 32, area));
    }
    if state.publish_overlay() {
        render_publish_editor(frame, state, centered_rect(84, 78, area), false);
    }
    if state
        .subscriptions
        .as_ref()
        .is_some_and(|view| view.editor.is_some())
    {
        render_subscription_editor(frame, state, centered_rect(60, 40, area));
    }
    // The question to quit covers everything else, dialogs included.
    if matches!(state.confirmation, Some(Confirmation::Quit)) {
        render_confirmation(frame, state, centered_rect(60, 32, area));
    }
}

// ---------------------------------------------------------------- helpers

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Shortens `text` to at most `max` terminal columns, ending with an ellipsis when cut.
fn truncate(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + width > max - 1 {
            break;
        }
        result.push(character);
        used += width;
    }
    result.push('…');
    result
}

/// Short form for counts in narrow columns: 999, 1.2k, 15k, 1.5M.
pub(super) fn compact_count(count: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("k", 1_000), ("M", 1_000_000), ("G", 1_000_000_000)];
    if count < 1_000 {
        return count.to_string();
    }
    let mut index = UNITS
        .iter()
        .rposition(|(_, scale)| count >= *scale)
        .unwrap_or(0);
    let rounded = |index: usize| (count + UNITS[index].1 / 2) / UNITS[index].1;
    if rounded(index) >= 1_000 && index + 1 < UNITS.len() {
        index += 1;
    }
    let (unit, scale) = UNITS[index];
    let tenths = (count * 10 + scale / 2) / scale;
    if tenths >= 100 {
        format!("{}{unit}", rounded(index))
    } else if tenths.is_multiple_of(10) {
        format!("{}{unit}", tenths / 10)
    } else {
        format!("{}.{}{unit}", tenths / 10, tenths % 10)
    }
}

pub(super) fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut groups = Vec::new();
    let mut end = digits.len();
    while end > 3 {
        groups.push(&digits[end - 3..end]);
        end -= 3;
    }
    groups.push(&digits[..end]);
    groups.reverse();
    groups.join(",")
}

fn format_rate(rate: f64) -> String {
    if rate < 0.05 {
        "0".to_owned()
    } else if rate < 10.0 {
        format!("{rate:.1}")
    } else {
        thousands(rate.round() as usize)
    }
}

pub(super) fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn panel_block(title: &str, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::pane_border(active))
        .padding(Padding::horizontal(1))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            if active {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::muted()
            },
        )))
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

/// Keeps `selected` inside the window of `height` rows starting at `scroll`.
fn scroll_into_view(scroll: usize, selected: usize, height: usize, total: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let mut scroll = scroll;
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + height {
        scroll = selected + 1 - height;
    }
    scroll.min(total.saturating_sub(height))
}

fn line_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

/// Drops `offset` columns from the left and keeps at most `width` columns.
fn slice_spans(spans: Vec<Span<'static>>, offset: usize, width: usize) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut column = 0;
    let mut remaining = width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let mut text = String::new();
        for character in span.content.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if column >= offset && character_width <= remaining {
                text.push(character);
                remaining -= character_width;
            }
            column += character_width;
        }
        if !text.is_empty() {
            result.push(Span::styled(text, span.style));
        }
    }
    result
}

fn restyle(spans: Vec<Span<'static>>, style: Style) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| Span::styled(span.content, span.style.patch(style)))
        .collect()
}

// ----------------------------------------------------------------- header

struct Segment {
    spans: Vec<Span<'static>>,
    /// The last span may be shortened when space runs out.
    truncatable: bool,
}

impl Segment {
    fn plain(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![Span::styled(text.into(), style)],
            truncatable: false,
        }
    }
}

/// The brokers of several connections, each with its marker, state and the shared totals.
fn multi_header_segments(state: &UiState) -> Vec<Segment> {
    let mut segments: Vec<Segment> = state
        .connections
        .iter()
        .map(|connection| {
            let mut spans = Vec::new();
            if let Some(slot) = state.broker_badge(&connection.broker_id) {
                spans.push(Span::styled(format!("{slot} "), theme::broker_style(slot)));
            }
            let color = theme::state_color(connection.state);
            spans.push(Span::styled(
                if connection.state == ConnectionState::Disconnected {
                    "○ "
                } else {
                    "● "
                },
                Style::default().fg(color),
            ));
            spans.push(Span::styled(connection.broker_name.clone(), theme::bold()));
            if connection.state != ConnectionState::Connected {
                spans.push(Span::styled(
                    format!(" {}", theme::state_label(connection.state)),
                    Style::default().fg(color),
                ));
            }
            Segment {
                spans,
                truncatable: false,
            }
        })
        .collect();
    if state.any_connected() {
        segments.push(Segment::plain(
            format!("↓ {} msg/s", format_rate(state.rate.per_second(state.now))),
            Style::default().fg(theme::ACCENT),
        ));
    }
    let topic_count = state.topics.topic_count();
    if topic_count > 0 {
        let mut text = format!(
            "{} topic{}",
            thousands(topic_count),
            if topic_count == 1 { "" } else { "s" }
        );
        if !state.any_connected() {
            text.push_str(" (cached)");
        }
        segments.push(Segment::plain(text, theme::muted()));
    }
    if state.dropped_messages > 0 {
        segments.push(Segment::plain(
            format!("⚠ {} dropped", thousands(state.dropped_messages as usize)),
            Style::default().fg(theme::WARN),
        ));
    }
    segments
}

fn header_segments(state: &UiState) -> Vec<Segment> {
    if state.connections.len() > 1 {
        return multi_header_segments(state);
    }
    let mut segments = Vec::new();
    let connection = state.connections.first();
    let (glyph, color, name) = match connection {
        Some(connection) => (
            if connection.state == ConnectionState::Disconnected {
                "○"
            } else {
                "●"
            },
            theme::state_color(connection.state),
            connection.broker_name.clone(),
        ),
        None => {
            let wanted = state.selected_broker.as_deref().or(state
                .config
                .general
                .default_broker
                .as_deref());
            // A single profile is the implicit target of `o`, so the header names it too.
            let profile = wanted
                .and_then(|wanted| state.find_profile(wanted))
                .or_else(|| (state.config.brokers.len() == 1).then(|| &state.config.brokers[0]));
            match profile {
                Some(profile) => ("○", theme::MUTED, profile.name.clone()),
                None => {
                    return vec![Segment {
                        spans: vec![
                            Span::styled("○ ", theme::muted()),
                            Span::styled("No broker selected", theme::muted()),
                        ],
                        truncatable: false,
                    }];
                }
            }
        }
    };
    segments.push(Segment {
        spans: vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(name, theme::bold()),
        ],
        truncatable: false,
    });

    match connection {
        Some(connection) => {
            segments.push(Segment::plain(
                theme::state_label(connection.state),
                Style::default().fg(theme::state_color(connection.state)),
            ));
            if connection.state == ConnectionState::Connected {
                segments.push(Segment::plain(connection.protocol, theme::muted()));
                segments.push(if connection.tls {
                    let label = if connection.mutual_tls {
                        "mTLS ✓"
                    } else {
                        "TLS ✓"
                    };
                    Segment::plain(label, Style::default().fg(theme::OK))
                } else {
                    Segment::plain("plain", theme::muted())
                });
                segments.push(if connection.allow_publish {
                    Segment::plain("publish on", Style::default().fg(theme::WARN))
                } else {
                    Segment::plain("read-only", theme::muted())
                });
                let rate = state.rate.per_second(state.now);
                segments.push(Segment::plain(
                    format!("↓ {} msg/s", format_rate(rate)),
                    Style::default().fg(theme::ACCENT),
                ));
            } else if let Some(detail) = &connection.detail {
                segments.push(Segment {
                    spans: vec![
                        Span::styled("Last error: ", theme::muted()),
                        Span::styled(detail.clone(), Style::default().fg(theme::ERROR)),
                    ],
                    truncatable: true,
                });
            }
        }
        None => segments.push(Segment::plain("Disconnected", theme::muted())),
    }

    let topic_count = state.topics.topic_count();
    if topic_count > 0 {
        let connected = state.any_connected();
        let mut text = format!(
            "{} topic{}",
            thousands(topic_count),
            if topic_count == 1 { "" } else { "s" }
        );
        if !connected {
            text.push_str(" (cached)");
        }
        segments.push(Segment::plain(text, theme::muted()));
    }
    if state.dropped_messages > 0 {
        segments.push(Segment::plain(
            format!("⚠ {} dropped", thousands(state.dropped_messages as usize)),
            Style::default().fg(theme::WARN),
        ));
    }
    segments
}

fn render_header(frame: &mut Frame, state: &UiState, area: Rect) {
    let width = area.width as usize;
    let badge = Span::styled(
        format!(" MATE v{} ", env!("CARGO_PKG_VERSION")),
        Style::default()
            .fg(Color::Black)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    let mut used = display_width(&badge.content);
    let mut spans = vec![badge];
    let mut segments = header_segments(state);
    if let Some(status) = state.replay_status() {
        // Right after the broker so it stays visible when the header is narrow.
        segments.insert(
            segments.len().min(1),
            Segment::plain(status, theme::bold().fg(theme::WARN)),
        );
    }
    for segment in segments {
        let separator_width = 3;
        let segment_width = line_width(&segment.spans);
        let remaining = width.saturating_sub(used + separator_width);
        if segment_width <= remaining {
            spans.push(Span::styled(" │ ", theme::subtle()));
            used += separator_width + segment_width;
            spans.extend(segment.spans);
        } else {
            if segment.truncatable && remaining >= 14 {
                let mut shortened = segment.spans;
                let before = line_width(&shortened[..shortened.len() - 1]);
                if let Some(last) = shortened.last_mut() {
                    *last = Span::styled(
                        truncate(&last.content, remaining.saturating_sub(before)),
                        last.style,
                    );
                }
                spans.push(Span::styled(" │ ", theme::subtle()));
                spans.extend(shortened);
            }
            break;
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// -------------------------------------------------------------- shortcuts

fn shortcut_items(state: &UiState) -> Vec<(&'static str, &'static str)> {
    if matches!(state.confirmation, Some(Confirmation::Quit)) {
        return vec![("y/Enter", "Quit"), ("n/Esc", "Stay")];
    }
    if state.log_dialog.is_some() {
        return vec![
            ("Space", "On/off"),
            ("Tab", "Field"),
            ("Enter", "Save"),
            ("Esc", "Cancel"),
        ];
    }
    if let Some(editor) = state
        .publish_editor
        .as_ref()
        .filter(|_| state.publish_typing())
    {
        if editor.file_prompt.is_some() {
            return vec![("Enter", "Load file"), ("Esc", "Cancel")];
        }
        let leave = if editor.docked {
            "Leave panel"
        } else {
            "Cancel"
        };
        let mut items = match editor.stage {
            PublishStage::Review => vec![("y/Ctrl+S", "Publish"), ("n/Esc", "Back")],
            PublishStage::Edit if editor.field == FIELD_RECENT => vec![
                ("↑/↓", "Select"),
                ("Enter", "Load"),
                ("d", "Forget"),
                ("Tab", "Field"),
                ("Esc", leave),
            ],
            PublishStage::Edit if editor.field == FIELD_PAYLOAD => vec![
                ("Ctrl+S", "Send"),
                ("Esc", leave),
                ("Ctrl+J", "New line"),
                ("Ctrl+F", "JSON layout"),
                ("Ctrl+V", "Paste"),
                ("Ctrl+O", "File"),
                ("Ctrl+U", "Clear"),
                ("Enter", "Next field"),
                ("←↑↓→", "Move"),
            ],
            PublishStage::Edit => vec![
                ("Enter", "Next/Review"),
                ("Ctrl+S", "Send"),
                ("Esc", leave),
                ("Tab", "Field"),
                ("←/→", "QoS/Retain"),
            ],
        };
        if editor.stage == PublishStage::Edit && state.publishable_brokers().nth(1).is_some() {
            items.push(("Ctrl+B", "Broker"));
        }
        if editor.docked
            && let Some(position) = items.iter().position(|item| item.0 == "Esc")
        {
            // The panel's way out must survive a narrow bar.
            let leave = items.remove(position);
            items.insert(0, leave);
        }
        return items;
    }
    if let Some(editor) = &state.editor {
        let mut items = vec![
            ("Enter", "Next/Save"),
            ("Esc", "Cancel"),
            ("Tab/↓", "Next field"),
            ("Shift+Tab/↑", "Back"),
        ];
        if editor.field == super::FIELD_PROTOCOL {
            items.insert(2, ("←/→", "Protocol"));
        }
        return items;
    }
    if let Some(editor) = &state.certificate_editor {
        let mut items = vec![("Enter", "Apply"), ("Tab", "Field"), ("1/2/3", "Kind")];
        if matches!(editor.mode, CertificateEditMode::Import) {
            items.push(("!", "Replace"));
        }
        items.push(("Esc", "Cancel"));
        return items;
    }
    if state.confirmation.is_some() {
        return vec![("y", "Confirm"), ("n/Esc", "Cancel")];
    }
    if state.view == View::Subscriptions
        && state
            .subscriptions
            .as_ref()
            .is_some_and(|view| view.editor.is_some())
    {
        let exclude = state
            .subscriptions
            .as_ref()
            .and_then(|view| view.editor.as_ref())
            .is_some_and(|editor| editor.exclude);
        return if exclude {
            vec![("Enter", "Save"), ("Esc", "Cancel")]
        } else {
            vec![
                ("Enter", "Next/Save"),
                ("Tab", "Field"),
                ("0/1/2", "QoS"),
                ("Esc", "Cancel"),
            ]
        };
    }
    if state.help_open && state.help.tab == HelpTab::Snake {
        let running = state
            .help
            .snake
            .as_ref()
            .is_some_and(|snake| snake.is_running());
        return vec![
            ("←↑↓→", "Steer"),
            ("Enter", if running { "Pause" } else { "Start" }),
            ("Tab", "Leave"),
            ("Esc", "Close help"),
        ];
    }
    if state.help_open {
        return vec![
            ("Tab/1-5", "Section"),
            ("↑/↓", "Scroll"),
            ("Esc", "Close help"),
        ];
    }
    match state.view {
        View::Replay => {
            let replay = &state.replay;
            if replay.confirming {
                return vec![("y/Enter", "Start"), ("n/Esc", "Cancel")];
            }
            if replay.typing() {
                return vec![("Enter", "Apply"), ("Esc", "Cancel")];
            }
            return match replay.stage {
                crate::ui::replay::Stage::Files => vec![
                    ("Space", "Mark"),
                    ("Enter", "Load"),
                    ("p", "Folder/file"),
                    ("a", "Mark all"),
                    ("Esc", "Back"),
                    ("q", "Quit"),
                ],
                crate::ui::replay::Stage::Player => vec![
                    ("Space", if replay.is_playing() { "Pause" } else { "Play" }),
                    ("b", "Target"),
                    ("m", "Mode"),
                    ("+/-", "Speed"),
                    ("g", "Gaps"),
                    ("l", "Loop"),
                    ("/", "Filter"),
                    ("r", "Restart"),
                    ("Esc", "Files"),
                ],
            };
        }
        View::Brokers => {
            return vec![
                ("Enter", "Connect"),
                ("Space", "Mark"),
                ("a", "Add"),
                ("e", "Edit"),
                ("d", "Delete"),
                ("t", "Test"),
                ("v", "Certificates"),
                ("s", "Subscriptions"),
                ("w", "Publishing on/off"),
                ("y", "System topics"),
                ("u", "Duplicate"),
                ("*", "Default"),
                ("x", "Disconnect"),
                ("L", "Logging"),
                ("R", "Replay"),
                ("Esc", "Back"),
                ("q", "Quit"),
            ];
        }
        View::Subscriptions => {
            return vec![
                ("Space", "On/off"),
                ("a", "Add"),
                ("x", "Exclude"),
                ("d", "Remove"),
                ("0/1/2", "QoS"),
                ("Esc", "Back"),
            ];
        }
        View::Certificates => {
            return vec![
                ("i", "Import"),
                ("r", "Reference"),
                ("d", "Remove"),
                ("Esc", "Back"),
            ];
        }
        View::Stream => {
            let view = &state.stream_view;
            if view.typing.is_some() {
                return vec![("Enter", "Keep filter"), ("Esc", "Clear"), ("↑/↓", "Move")];
            }
            return vec![
                ("Enter", "Details"),
                ("Space", if view.paused { "Resume" } else { "Pause" }),
                ("/", "Topic"),
                ("f", "Find"),
                ("R", "Retained"),
                ("s", "Show in tree"),
                ("c", "Copy payload"),
                ("C", "Clear stream"),
                (
                    "Esc",
                    if view.filters_active() {
                        "Clear filters"
                    } else {
                        "Back"
                    },
                ),
                ("q", "Quit"),
            ];
        }
        View::Analytics => {
            if state.analytics.filtering {
                return vec![("Enter", "Keep filter"), ("Esc", "Clear"), ("↑/↓", "Move")];
            }
            let mut items = vec![("Tab/1-7", "List"), ("s", "Sort by"), ("r", "Reverse")];
            match state.analytics.mode {
                AnalyticsMode::InvalidJson => items.push(("f", "Filter kind")),
                AnalyticsMode::Branches => items.push(("d", "Depth")),
                _ => {}
            }
            items.extend([
                ("/", "Filter"),
                (
                    "Space",
                    if state.analytics.paused {
                        "Resume"
                    } else {
                        "Pause"
                    },
                ),
                ("Enter", "Show in tree"),
                ("t", "Copy topic"),
                (
                    "Esc",
                    if state.analytics.filter.is_empty() {
                        "Back"
                    } else {
                        "Clear filter"
                    },
                ),
                ("q", "Quit"),
            ]);
            return items;
        }
        View::Main => {}
    }
    if state.main.search.active {
        return vec![("Enter", "Keep filter"), ("Esc", "Clear"), ("↑/↓", "Move")];
    }
    if state.main.inspector.search.active {
        return vec![("Enter", "Done"), ("Esc", "Cancel")];
    }
    if state.main.history_search.active {
        return vec![("Enter", "Keep filter"), ("Esc", "Clear"), ("↑/↓", "Move")];
    }

    let mut items: Vec<(&'static str, &'static str)> = match state.main.focus {
        Panel::Topics => {
            let mut items = vec![
                ("/", "Search"),
                ("Enter", "Open"),
                ("i", "Info"),
                ("A", "Analytics"),
                ("S", "Stream"),
                ("*", "Favorite"),
                ("F", "Favorites only"),
            ];
            if state.main.filter_active() {
                items.push(("Esc", "Clear filter"));
            }
            items.push(("c", "Copy payload"));
            items.push(("t", "Copy topic"));
            items.push(("+/-", "Expand/collapse all"));
            items.push(("Shift+←/→", "Branch"));
            items.push(("m", "Maximize"));
            items.push(("h", "History"));
            items.push(("b", "Brokers"));
            items.push(("o", "Connect"));
            items.push(("L", "Logging"));
            items.push(("R", "Replay"));
            items
        }
        Panel::Inspector => {
            if state.main.inspector.info {
                vec![("↑/↓", "Scroll"), ("i/Esc", "Close info"), ("c", "Copy")]
            } else if state.main.inspector.diff.is_some() {
                vec![("↑/↓", "Scroll"), ("d/Esc", "Close diff")]
            } else {
                let toggle = match state.main.inspector.effective_mode() {
                    ViewMode::Tree => "Raw",
                    ViewMode::Raw => {
                        if state
                            .main
                            .inspector
                            .loaded()
                            .is_some_and(|loaded| loaded.doc.is_some())
                        {
                            "Tree"
                        } else {
                            "Hex"
                        }
                    }
                    ViewMode::Hex => "Text",
                };
                let mut items = Vec::new();
                if state.main.inspector.effective_mode() == ViewMode::Tree {
                    items.push(("Enter", "Expand"));
                }
                items.extend([
                    ("c", "Copy"),
                    ("t", "Copy topic"),
                    ("i", "Info"),
                    ("r", toggle),
                    ("/", "Search"),
                    ("d", "Diff"),
                    ("m", "Maximize"),
                ]);
                items
            }
        }
        Panel::Publish => vec![("P", "Hide panel"), ("b", "Brokers")],
        Panel::History => vec![
            ("Enter", "Inspect"),
            (
                "d",
                if state.main.compare_base.is_some() {
                    "Compare with base"
                } else {
                    "Mark/Compare"
                },
            ),
            ("/", "Search"),
            ("l", "Live"),
            ("c", "Copy"),
            ("m", "Maximize"),
            ("h", "Hide"),
        ],
    };
    if can_publish(state) {
        // Near the front so it survives when the bar is too narrow for every item.
        let from_message = matches!(state.main.focus, Panel::Inspector | Panel::History);
        items.insert(
            items.len().min(2),
            (
                "p",
                if from_message {
                    "Edit & publish"
                } else {
                    "Publish"
                },
            ),
        );
        let resendable = match state.main.focus {
            Panel::History => true,
            Panel::Inspector => state.main.pinned.is_some() && state.main.inspector.diff.is_none(),
            Panel::Topics | Panel::Publish => false,
        };
        items.insert(
            items.len().min(3),
            (
                "P",
                if state.main.layout.show_publish {
                    "Hide publish panel"
                } else {
                    "Publish panel"
                },
            ),
        );
        if resendable {
            items.insert(items.len().min(3), ("Ctrl+S", "Send again"));
        }
    }
    items.push(("Tab", "Focus"));
    items
}

/// Publishing needs an established connection to a broker whose profile allows it.
fn can_publish(state: &UiState) -> bool {
    state.publish_blocker().is_none()
}

/// `[key] label` in the style of the shortcut bar.
fn push_shortcut(spans: &mut Vec<Span<'static>>, key: &str, label: &str) {
    spans.push(Span::styled(
        format!("[{key}]"),
        theme::bold().fg(theme::ACCENT),
    ));
    spans.push(Span::styled(format!(" {label}  "), theme::muted()));
}

/// The keys of the open popup on its last row, in the style of the shortcut bar.
fn popup_footer(frame: &mut Frame, items: &[(&str, &str)], area: Rect) {
    if area.height < 4 || area.width < 14 {
        return;
    }
    let width = area.width as usize - 2;
    let row = Rect::new(area.x + 1, area.y + area.height - 2, width as u16, 1);
    let mut spans = Vec::new();
    let mut used = 0;
    for (key, label) in items {
        let cost = display_width(key) + display_width(label) + 5;
        if used + cost - 2 > width {
            break;
        }
        push_shortcut(&mut spans, key, label);
        used += cost;
    }
    frame.render_widget(Clear, row);
    frame.render_widget(Paragraph::new(Line::from(spans)), row);
}

fn render_shortcuts(frame: &mut Frame, state: &UiState, area: Rect) {
    let width = area.width as usize;
    let notice = state.notice.as_ref();
    let notice_width = notice.map_or(0, |notice| display_width(&notice.text).min(width / 3));
    let budget = width.saturating_sub(if notice_width > 0 {
        notice_width + 2
    } else {
        0
    });

    let items = shortcut_items(state);
    let dialog_open = state.editor.is_some()
        || state.publish_typing()
        || state.certificate_editor.is_some()
        || state.confirmation.is_some();
    let globals: &[(&str, &str)] = match state.view {
        _ if dialog_open || state.help_open => &[],
        View::Main => &[("?", "Help"), ("q", "Quit")],
        View::Analytics | View::Brokers | View::Stream | View::Replay => &[("?", "Help")],
        _ => &[],
    };
    let cost = |(key, label): &(&str, &str)| display_width(key) + display_width(label) + 5;
    let reserved: usize = globals.iter().map(cost).sum::<usize>() + 1;
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    let push = push_shortcut;
    for item in &items {
        if used + cost(item) + reserved > budget {
            break;
        }
        push(&mut spans, item.0, item.1);
        used += cost(item);
    }
    for item in globals {
        if used + cost(item) > budget {
            break;
        }
        push(&mut spans, item.0, item.1);
        used += cost(item);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    if let Some(notice) = notice
        && notice_width > 0
    {
        let color = if notice.error {
            theme::ERROR
        } else {
            theme::WARN
        };
        let rect = Rect::new(
            area.x + (width - notice_width) as u16,
            area.y,
            notice_width as u16,
            1,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate(&notice.text, notice_width),
                Style::default().fg(color),
            )),
            rect,
        );
    }
}

// -------------------------------------------------------------- main view

fn render_main(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let rects = state.main.layout.compute(area, state.main.focus);
    if let Some(rect) = rects.topics {
        render_topics(frame, state, rect);
    }
    if let Some(rect) = rects.inspector {
        render_inspector(frame, state, rect);
    }
    if let Some(rect) = rects.history {
        render_history(frame, state, rect);
    }
    if let Some(rect) = rects.publish {
        render_publish_panel(frame, state, rect);
    }
}

fn render_topics(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let active = state.main.focus == Panel::Topics;
    let block = panel_block("TOPICS", active);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }

    let search_visible = state.main.search.active || state.main.filter_active();
    let list_height = inner.height as usize - usize::from(search_visible && inner.height > 1);
    let list_area = Rect::new(inner.x, inner.y, inner.width, list_height as u16);
    let width = inner.width as usize;

    state.main.topic_viewport = list_height;
    let total = state.main.rows.len();
    state.main.topic_scroll = scroll_into_view(
        state.main.topic_scroll,
        state.main.selected_index,
        list_height,
        total,
    );

    if total == 0 {
        let message = if state.main.favorites_only && state.main.favorites.is_empty() {
            "No favorites yet. Press * on a topic to mark one."
        } else if state.main.filter_active() {
            "No topics match the filter"
        } else if state.any_connected() {
            "Connected; waiting for messages on the subscribed topics"
        } else if state.connections.is_empty() {
            "Not connected. Press o to connect or b to manage brokers."
        } else {
            "No topics yet"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(message, theme::muted())).wrap(Wrap { trim: true }),
            list_area,
        );
    } else {
        let start = state.main.topic_scroll;
        let end = (start + list_height).min(total);
        let lines = {
            let main = &mut state.main;
            let query = main.search.query.trim().to_owned();
            let window = &main.rows[start..end];
            let badge_width = if state.broker_slots.len() > 1 {
                state.broker_slots.len() + 1
            } else {
                0
            };
            let name_budget = window
                .iter()
                .map(|row| row.depth * 2 + 2 + badge_width + display_width(&row.segment))
                .max()
                .unwrap_or(0);
            let name_column = (name_budget + 2).clamp(12, (width * 3 / 5).max(12));
            window
                .iter()
                .enumerate()
                .map(|(offset, row)| {
                    let selected = start + offset == main.selected_index;
                    let message = state.topics.latest_message(&row.path);
                    let preview =
                        message.map(|message| main.previews.get(&row.path, message).clone());
                    topic_line(
                        row,
                        TopicLineContext {
                            expanded: main.expanded.contains(&row.path) || !query.is_empty(),
                            favorite: main.favorites.contains(&row.path),
                            recent: message.is_some_and(|message| {
                                state.now.saturating_duration_since(message.received) < RECENT
                            }),
                            badges: broker_badge_columns(
                                &state.broker_slots,
                                state.topics.topic_brokers(&row.path),
                                row.has_value,
                            ),
                            preview: preview.as_ref(),
                            query: &query,
                            selected,
                            focused: active,
                            name_column,
                            width,
                            count: state.topics.received_in_session(&row.path),
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    if search_visible && inner.height > 1 {
        let mut spans = vec![
            Span::styled("/ ", theme::bold().fg(theme::ACCENT)),
            Span::raw(state.main.search.query.clone()),
        ];
        if state.main.search.active {
            spans.push(Span::styled("▌", theme::bold().fg(theme::ACCENT)));
        }
        spans.push(Span::styled(
            format!("   {} shown", thousands(total)),
            theme::muted(),
        ));
        if state.main.favorites_only {
            spans.push(Span::styled("  [favorites]", theme::bold().fg(theme::WARN)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + list_height as u16, inner.width, 1),
        );
    }
}

struct TopicLineContext<'a> {
    expanded: bool,
    favorite: bool,
    recent: bool,
    /// One column per broker slot, naming the brokers that delivered the topic; empty for one broker.
    badges: Vec<Span<'static>>,
    preview: Option<&'a Preview>,
    query: &'a str,
    selected: bool,
    focused: bool,
    name_column: usize,
    width: usize,
    /// Messages received since the last connection attempt.
    count: u64,
}

/// One digit per broker slot that delivered the topic, `·` for the others, blank for branches.
fn broker_badge_columns(
    slots: &[String],
    brokers: &[std::sync::Arc<str>],
    has_value: bool,
) -> Vec<Span<'static>> {
    if slots.len() < 2 {
        return Vec::new();
    }
    let mut spans: Vec<Span<'static>> = slots
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let slot = index + 1;
            if !has_value {
                Span::raw(" ")
            } else if brokers.iter().any(|broker| **broker == **id) {
                Span::styled(slot.to_string(), theme::broker_style(slot))
            } else {
                Span::styled("·", theme::subtle())
            }
        })
        .collect();
    spans.push(Span::raw(" "));
    spans
}

fn preview_style(kind: PreviewKind) -> Style {
    match kind {
        PreviewKind::Number => Style::default().fg(Color::Yellow),
        PreviewKind::Bool | PreviewKind::Null => Style::default().fg(Color::Magenta),
        PreviewKind::Invalid => Style::default().fg(theme::ERROR),
        PreviewKind::Object | PreviewKind::Array | PreviewKind::Empty | PreviewKind::Binary => {
            theme::muted()
        }
        PreviewKind::Text => Style::default(),
    }
}

fn topic_line(row: &TopicRow, context: TopicLineContext<'_>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw("  ".repeat(row.depth)));
    let invalid = context
        .preview
        .is_some_and(|preview| preview.kind == PreviewKind::Invalid);
    // One marker column: the expansion arrow for branches, the activity dot for leaves.
    // A branch that also carries a value tints its arrow instead.
    spans.push(if row.has_children {
        let color = if invalid {
            theme::ERROR
        } else if row.has_value && context.recent {
            theme::OK
        } else {
            theme::ACCENT
        };
        Span::styled(
            if context.expanded { "▼" } else { "▶" },
            Style::default().fg(color),
        )
    } else if !row.has_value {
        Span::raw(" ")
    } else if invalid {
        Span::styled("!", Style::default().fg(theme::ERROR))
    } else if context.recent {
        Span::styled("●", Style::default().fg(theme::OK))
    } else {
        Span::styled("○", theme::subtle())
    });
    spans.push(Span::raw(" "));
    let badge_width = line_width(&context.badges);
    spans.extend(context.badges.iter().cloned());

    let fixed = row.depth * 2 + 2 + badge_width;
    let favorite_width = if context.favorite { 2 } else { 0 };
    // Names win over previews: only a name wider than the pane itself is cut.
    let segment_budget = context.width.saturating_sub(fixed + favorite_width).max(1);
    let segment = truncate(&row.segment, segment_budget);
    let highlighted = highlight_set(context.query, row, &segment);
    spans.extend(segment_spans(&segment, &highlighted));
    if context.favorite {
        spans.push(Span::styled(" *", theme::bold().fg(theme::WARN)));
    }

    if let Some(preview) = context.preview {
        let used = line_width(&spans);
        let column = context.name_column.max(used + 2);
        let available = context.width.saturating_sub(column);
        if available >= 4 {
            spans.push(Span::raw(" ".repeat(column - used)));
            let count =
                (context.count > 0).then(|| format!("\u{d7}{}", compact_count(context.count)));
            // The count sits right behind the preview; a narrow pane gets a one-column gap.
            let placed = count.as_ref().and_then(|count| {
                let width = display_width(count);
                [2, 1]
                    .into_iter()
                    .find(|gap| width + gap + MIN_PREVIEW_WIDTH <= available)
                    .map(|gap| (count, gap, available - width - gap))
            });
            match placed {
                Some((count, gap, budget)) => {
                    spans.push(Span::styled(
                        truncate(&preview.text, budget),
                        preview_style(preview.kind),
                    ));
                    spans.push(Span::raw(" ".repeat(gap)));
                    spans.push(Span::styled(count.clone(), theme::muted()));
                }
                None => spans.push(Span::styled(
                    truncate(&preview.text, available),
                    preview_style(preview.kind),
                )),
            }
        }
    }

    if context.selected {
        let style = if context.focused {
            theme::selection()
        } else {
            theme::selection_inactive()
        };
        let used = line_width(&spans);
        let mut selected = restyle(spans, style);
        if used < context.width {
            selected.push(Span::styled(" ".repeat(context.width - used), style));
        }
        return Line::from(selected);
    }
    Line::from(spans)
}

/// Character indices of `segment` that match the search query.
fn highlight_set(query: &str, row: &TopicRow, segment: &str) -> Vec<bool> {
    let mut flags = vec![false; segment.chars().count()];
    if query.is_empty() {
        return flags;
    }
    if let Some(positions) = mate::fuzzy::match_positions(query, &row.path) {
        let start = row.path.chars().count() - row.segment.chars().count();
        for position in positions {
            if position >= start && position - start < flags.len() {
                flags[position - start] = true;
            }
        }
    }
    flags
}

fn segment_spans(segment: &str, highlighted: &[bool]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_highlight = false;
    for (index, character) in segment.chars().enumerate() {
        let flag = highlighted.get(index).copied().unwrap_or(false);
        if flag != run_highlight && !run.is_empty() {
            spans.push(styled_run(std::mem::take(&mut run), run_highlight));
        }
        run_highlight = flag;
        run.push(character);
    }
    if !run.is_empty() {
        spans.push(styled_run(run, run_highlight));
    }
    spans
}

fn styled_run(text: String, highlighted: bool) -> Span<'static> {
    if highlighted {
        Span::styled(text, theme::bold().fg(theme::WARN))
    } else {
        Span::raw(text)
    }
}

// -------------------------------------------------------------- inspector

fn source_badge(source: DataSource) -> Span<'static> {
    match source {
        DataSource::Live => Span::styled("LIVE", theme::bold().fg(theme::OK)),
        DataSource::Retained => Span::styled("RETAINED", theme::bold().fg(theme::ACCENT)),
        DataSource::Cached => Span::styled("CACHED", theme::bold().fg(theme::MUTED)),
    }
}

fn render_inspector(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let active = state.main.focus == Panel::Inspector;
    let message_info = state.displayed_message().map(|message| {
        (
            state.data_source(message),
            message.wall,
            message.payload.len(),
            message.qos,
            message.retain,
            message.seq,
        )
    });
    let pinned = state.main.pinned.is_some() && message_info.is_some();

    let mut block = panel_block(
        if state.main.inspector.info {
            "TOPIC METADATA"
        } else {
            "PAYLOAD INSPECTOR"
        },
        active,
    );
    if let Some((source, wall, ..)) = message_info {
        let badge = if pinned {
            Span::styled(
                format!(
                    "HISTORY – {}",
                    clock(wall).split('.').next().unwrap_or_default()
                ),
                theme::bold().fg(theme::WARN),
            )
        } else {
            source_badge(source)
        };
        block =
            block.title(Line::from(vec![Span::raw(" "), badge, Span::raw(" ")]).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height < 2 {
        return;
    }

    let Some(topic) = state.main.selected.clone() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Select a topic to inspect its payload.",
                theme::muted(),
            )),
            inner,
        );
        return;
    };
    if state.main.inspector.info {
        render_info(frame, state, inner);
        return;
    }
    let Some((_, wall, size, qos, retain, seq)) = message_info else {
        let text = if state.topics.contains_path(&topic) {
            "This topic has no message of its own; select a child topic."
        } else {
            "No message for this topic."
        };
        frame.render_widget(
            Paragraph::new(Span::styled(text, theme::muted())).wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };

    let width = inner.width as usize;
    let count = state.topics.received_total(&topic);
    let newer = state.topics.history(&topic).map_or(0, |history| {
        history.iter().filter(|message| message.seq > seq).count()
    });
    let ordinal = count.saturating_sub(newer as u64);
    let mut meta = vec![
        Span::styled(clock(wall), theme::muted()),
        Span::styled(format!(" · {}", format_size(size)), theme::muted()),
        Span::styled(format!(" · QoS {qos}"), theme::muted()),
    ];
    if retain {
        meta.push(Span::styled(" · retained", theme::muted()));
    }
    if let Some(id) = state
        .displayed_message()
        .and_then(|message| message.broker.clone())
        && let Some(slot) = state.broker_badge(&id)
    {
        meta.push(Span::styled(" · ", theme::muted()));
        meta.push(Span::styled(slot.to_string(), theme::broker_style(slot)));
        meta.push(Span::styled(
            format!(" {}", state.broker_name(&id)),
            theme::muted(),
        ));
    }
    meta.push(Span::styled(
        format!(
            " · msg {} of {}",
            thousands(ordinal as usize),
            thousands(count as usize)
        ),
        theme::muted(),
    ));
    let mut header = vec![
        Line::from(vec![
            Span::styled("Topic: ", theme::muted()),
            Span::styled(truncate(&topic, width.saturating_sub(7)), theme::bold()),
        ]),
        Line::from(meta),
    ];
    if let Some(properties) = state
        .displayed_message()
        .and_then(|message| message.properties.clone())
        .filter(|properties| !properties.is_empty())
    {
        header.push(properties_line(&properties, width));
    }

    if let Some(diff) = &state.main.inspector.diff {
        header.push(Line::from(Span::styled(
            truncate(&format!("DIFF {}", diff.title), width),
            theme::bold().fg(theme::WARN),
        )));
    }
    let header_height = header.len() as u16;
    let footer_height = u16::from(inner.height > header_height + 2);
    let body_height = inner.height.saturating_sub(header_height + footer_height) as usize;
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, header_height),
    );
    let body = Rect::new(
        inner.x,
        inner.y + header_height,
        inner.width,
        body_height as u16,
    );
    state.main.inspector.viewport = body_height;

    let lines = inspector_body(state, body_height, width, active);
    frame.render_widget(Paragraph::new(lines), body);

    if footer_height == 1 {
        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(Paragraph::new(inspector_footer(state, width)), footer_area);
    }
}

fn render_info(frame: &mut Frame, state: &mut UiState, area: Rect) {
    const LABEL_WIDTH: usize = 19;
    let width = area.width as usize;
    let mut lines = Vec::new();
    for (index, section) in state.info_sections().into_iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            theme::bold().fg(theme::ACCENT),
        )));
        for (label, value) in section.rows {
            lines.push(Line::from(vec![
                Span::styled(format!("  {label:<LABEL_WIDTH$}"), theme::muted()),
                Span::raw(truncate(&value, width.saturating_sub(LABEL_WIDTH + 2))),
            ]));
        }
    }
    let footer = u16::from(area.height > 3);
    let body_height = area.height.saturating_sub(footer) as usize;
    let inspector = &mut state.main.inspector;
    inspector.info_scroll = inspector
        .info_scroll
        .min(lines.len().saturating_sub(body_height));
    inspector.viewport = body_height;
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(inspector.info_scroll)
        .take(body_height)
        .collect();
    frame.render_widget(
        Paragraph::new(visible),
        Rect::new(area.x, area.y, area.width, body_height as u16),
    );
    if footer == 1 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Info ", theme::bold().fg(theme::ACCENT)),
                Span::styled("↑/↓ scroll  i/Esc close", theme::muted()),
            ])),
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        );
    }
}

/// One line with the MQTT 5 properties, user properties first; `i` shows them all.
fn properties_line(properties: &mate::topics::MessageProperties, width: usize) -> Line<'static> {
    let mut parts: Vec<String> = properties
        .user_properties
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if let Some(content_type) = &properties.content_type {
        parts.push(format!("type {content_type}"));
    }
    if let Some(response) = &properties.response_topic {
        parts.push(format!("reply {response}"));
    }
    if let Some(seconds) = properties.message_expiry_secs {
        parts.push(format!("expires {seconds}s"));
    }
    if properties.correlation_data.is_some() {
        parts.push("correlation".to_owned());
    }
    let label = "MQTT 5  ";
    let text = truncate(&parts.join("  \u{b7}  "), width.saturating_sub(label.len()));
    Line::from(vec![
        Span::styled(label, theme::bold().fg(theme::ACCENT)),
        Span::styled(text, theme::muted()),
    ])
}

fn inspector_footer(state: &UiState, width: usize) -> Line<'static> {
    let inspector = &state.main.inspector;
    if inspector.diff.is_some() {
        return Line::from(vec![
            Span::styled("Diff ", theme::bold().fg(theme::WARN)),
            Span::styled("↑/↓ scroll  d/Esc close", theme::muted()),
        ]);
    }
    if inspector.search.active || !inspector.search.query.is_empty() {
        let mut spans = vec![
            Span::styled("/ ", theme::bold().fg(theme::ACCENT)),
            Span::raw(inspector.search.query.clone()),
        ];
        if inspector.search.active {
            spans.push(Span::styled("▌", theme::bold().fg(theme::ACCENT)));
        }
        let matches = inspector.search.match_count();
        spans.push(Span::styled(
            if matches == 0 {
                "   no matches".to_owned()
            } else {
                format!("   {}/{} (n/N)", inspector.search.current + 1, matches)
            },
            theme::muted(),
        ));
        return Line::from(spans);
    }
    let mode = match inspector.effective_mode() {
        ViewMode::Tree => "Tree",
        ViewMode::Raw => "Raw",
        ViewMode::Hex => "Hex",
    };
    let mut spans = vec![Span::styled(
        format!("{mode} "),
        theme::bold().fg(theme::ACCENT),
    )];
    if inspector.diff.is_none() {
        if let Some(row) = inspector
            .loaded()
            .and(inspector.cursor_row())
            .filter(|_| inspector.effective_mode() == ViewMode::Tree)
        {
            spans.push(Span::styled(
                truncate(&row.path, width.saturating_sub(mode.len() + 2)),
                theme::muted(),
            ));
        } else if let Some(loaded) = inspector.loaded()
            && let Some(error) = &loaded.parse_error
            && inspector.effective_mode() != ViewMode::Hex
        {
            spans.push(Span::styled(
                truncate(
                    &format!("not valid JSON: {error}"),
                    width.saturating_sub(mode.len() + 2),
                ),
                theme::muted(),
            ));
        } else if inspector.scroll_x > 0 {
            spans.push(Span::styled(
                format!("column {}", inspector.scroll_x),
                theme::muted(),
            ));
        }
    }
    Line::from(spans)
}

fn inspector_body(
    state: &mut UiState,
    height: usize,
    width: usize,
    focused: bool,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    if let Some(diff) = &mut state.main.inspector.diff {
        return match &diff.lines {
            Err(error) => vec![Line::from(Span::styled(
                error.clone(),
                Style::default().fg(theme::ERROR),
            ))],
            Ok(lines) => {
                diff.scroll = diff.scroll.min(lines.len().saturating_sub(height));
                let scroll_x = state.main.inspector.scroll_x;
                lines
                    .iter()
                    .skip(diff.scroll)
                    .take(height)
                    .map(|line| {
                        let (prefix, text, style) = match line {
                            DiffLine::Same(text) => ("  ", text, theme::muted()),
                            DiffLine::Removed(text) => {
                                ("- ", text, Style::default().fg(theme::ERROR))
                            }
                            DiffLine::Added(text) => ("+ ", text, Style::default().fg(theme::OK)),
                        };
                        Line::from(slice_spans(
                            vec![Span::styled(format!("{prefix}{text}"), style)],
                            scroll_x,
                            width,
                        ))
                    })
                    .collect()
            }
        };
    }

    let mode = state.main.inspector.effective_mode();
    match mode {
        ViewMode::Tree => tree_lines(state, height, width, focused),
        ViewMode::Raw => raw_lines(state, height, width, focused),
        ViewMode::Hex => hex_lines(state, height, width),
    }
}

fn node_value_style(kind: NodeKind) -> Style {
    match kind {
        NodeKind::String => Style::default().fg(Color::Green),
        NodeKind::Number => Style::default().fg(Color::Yellow),
        NodeKind::Bool | NodeKind::Null => Style::default().fg(Color::Magenta),
        NodeKind::Object | NodeKind::Array | NodeKind::More => theme::muted(),
    }
}

fn tree_row_spans(row: &JsonRow, value_column: usize, matched: bool) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ".repeat(row.depth))];
    spans.push(if row.expandable {
        Span::styled(
            if row.expanded { "▼ " } else { "▶ " },
            Style::default().fg(theme::ACCENT),
        )
    } else {
        Span::raw("  ")
    });
    let label_style = if matched {
        theme::bold().fg(theme::WARN)
    } else if row.label.starts_with('[') || row.kind == NodeKind::More {
        theme::muted()
    } else {
        Style::default().fg(Color::Cyan)
    };
    spans.push(Span::styled(row.label.clone(), label_style));
    let used = line_width(&spans);
    // An open container's children are listed below it, so a summary would be noise.
    if !row.expanded {
        spans.push(Span::raw(
            " ".repeat(value_column.saturating_sub(used).max(2)),
        ));
        spans.push(Span::styled(row.value.clone(), node_value_style(row.kind)));
    }
    spans
}

fn tree_lines(
    state: &mut UiState,
    height: usize,
    width: usize,
    focused: bool,
) -> Vec<Line<'static>> {
    let inspector = &mut state.main.inspector;
    let total = inspector.rows().len();
    if total == 0 {
        return vec![Line::from(Span::styled("No JSON content.", theme::muted()))];
    }
    let cursor = inspector.cursor_index();
    inspector.scroll = scroll_into_view(inspector.scroll, cursor, height, total);
    let start = inspector.scroll;
    let end = (start + height).min(total);
    let rows = &inspector.rows()[start..end];
    let widest = rows
        .iter()
        .map(|row| row.depth * 2 + 2 + display_width(&row.label))
        .max()
        .unwrap_or(0);
    let value_column = (widest + 2).min(width / 2).max(8);
    let scroll_x = inspector.scroll_x;
    rows.iter()
        .enumerate()
        .map(|(offset, row)| {
            let matched = inspector.search.pointer_matches.contains(&row.pointer);
            let mut spans = tree_row_spans(row, value_column, matched);
            let selected = start + offset == cursor;
            if selected {
                let style = if focused {
                    theme::selection()
                } else {
                    theme::selection_inactive()
                };
                let used = line_width(&spans);
                spans = restyle(spans, style);
                if used < scroll_x + width {
                    spans.push(Span::styled(" ".repeat(scroll_x + width - used), style));
                }
            }
            Line::from(slice_spans(spans, scroll_x, width))
        })
        .collect()
}

fn raw_lines(
    state: &mut UiState,
    height: usize,
    width: usize,
    focused: bool,
) -> Vec<Line<'static>> {
    let inspector = &mut state.main.inspector;
    let total = inspector.line_count();
    inspector.scroll = inspector.scroll.min(total.saturating_sub(height));
    let (start, scroll_x) = (inspector.scroll, inspector.scroll_x);
    let Some(loaded) = inspector.loaded() else {
        return Vec::new();
    };
    if loaded.text_lines.is_empty() {
        return vec![Line::from(Span::styled("(empty payload)", theme::muted()))];
    }
    let is_json = loaded.doc.is_some();
    let current_match = inspector
        .search
        .line_matches
        .get(inspector.search.current)
        .copied();
    loaded
        .text_lines
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, text)| {
            let mut spans = if is_json {
                highlight_json_line(text).spans
            } else {
                vec![Span::raw(text.clone())]
            };
            if current_match == Some(index) && focused {
                spans = restyle(spans, theme::selection_inactive());
            }
            Line::from(slice_spans(spans, scroll_x, width))
        })
        .collect()
}

fn hex_lines(state: &mut UiState, height: usize, width: usize) -> Vec<Line<'static>> {
    let inspector = &mut state.main.inspector;
    let total = inspector.line_count();
    inspector.scroll = inspector.scroll.min(total.saturating_sub(height));
    let (start, scroll_x) = (inspector.scroll, inspector.scroll_x);
    let Some(loaded) = inspector.loaded() else {
        return Vec::new();
    };
    loaded
        .payload
        .chunks(HEX_BYTES_PER_LINE)
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, chunk)| {
            let mut hex = String::new();
            for position in 0..HEX_BYTES_PER_LINE {
                match chunk.get(position) {
                    Some(byte) => hex.push_str(&format!("{byte:02x} ")),
                    None => hex.push_str("   "),
                }
                if position == 7 {
                    hex.push(' ');
                }
            }
            let ascii: String = chunk
                .iter()
                .map(|byte| {
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        char::from(*byte)
                    } else {
                        '.'
                    }
                })
                .collect();
            Line::from(slice_spans(
                vec![
                    Span::styled(
                        format!("{:08x}  ", index * HEX_BYTES_PER_LINE),
                        theme::muted(),
                    ),
                    Span::styled(hex, Style::default().fg(Color::Yellow)),
                    Span::styled(format!(" |{ascii}|"), theme::muted()),
                ],
                scroll_x,
                width,
            ))
        })
        .collect()
}

/// Colors one line of pretty-printed JSON; strings never span lines in that format.
fn highlight_json_line(line: &str) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        let style = match chars[index] {
            '"' => {
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    index += if chars[index] == '\\' { 2 } else { 1 };
                }
                index = (index + 1).min(chars.len());
                let is_key = chars[index..].iter().find(|c| !c.is_whitespace()) == Some(&':');
                Style::default().fg(if is_key { Color::Cyan } else { Color::Green })
            }
            '-' | '0'..='9' => {
                while index < chars.len()
                    && matches!(chars[index], '-' | '+' | '.' | 'e' | 'E' | '0'..='9')
                {
                    index += 1;
                }
                Style::default().fg(Color::Yellow)
            }
            character if character.is_ascii_alphabetic() => {
                while index < chars.len() && chars[index].is_ascii_alphabetic() {
                    index += 1;
                }
                Style::default().fg(Color::Magenta)
            }
            _ => {
                while index < chars.len()
                    && !matches!(chars[index], '"' | '-' | '0'..='9')
                    && !chars[index].is_ascii_alphabetic()
                {
                    index += 1;
                }
                theme::subtle()
            }
        };
        spans.push(Span::styled(
            chars[start..index].iter().collect::<String>(),
            style,
        ));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------- history

fn render_history(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let active = state.main.focus == Panel::History;
    let since_connect = state
        .main
        .selected
        .as_deref()
        .map_or(0, |topic| state.topics.received_in_session(topic));
    let query = state.main.history_search.query.clone();
    let mut title = if since_connect > 0 {
        format!("HISTORY \u{b7} {}", compact_count(since_connect))
    } else {
        "HISTORY".to_owned()
    };
    if !query.is_empty() || state.main.history_search.active {
        let caret = if state.main.history_search.active {
            "\u{258c}"
        } else {
            ""
        };
        title.push_str(&format!(" \u{b7} /{query}{caret}"));
    }
    let block = panel_block(&title, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let height = inner.height as usize;
    state.main.history_viewport = height;

    let Some(topic) = state.main.selected.clone() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Select a topic to see its messages.",
                theme::muted(),
            ))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };
    let messages = history_messages(&state.topics, &topic, &query);
    if messages.is_empty() {
        let text = if state
            .topics
            .history(&topic)
            .is_some_and(|history| !history.is_empty())
        {
            format!("No message matches \"{query}\".")
        } else {
            "No messages for this topic.".to_owned()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(text, theme::muted())).wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let total = messages.len();
    let cursor = messages
        .iter()
        .position(|message| Some(message.seq) == state.main.history_cursor)
        .unwrap_or(0);
    let scroll = scroll_into_view(state.main.history_scroll, cursor, height, total);
    state.main.history_scroll = scroll;
    let lines: Vec<Line<'static>> = messages
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(index, message)| {
            history_line(
                message,
                message
                    .broker
                    .as_deref()
                    .and_then(|id| state.broker_badge(id)),
                index == cursor,
                state.main.pinned == Some(message.seq),
                state.main.compare_base == Some(message.seq),
                active,
                width,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn history_line(
    message: &Message,
    broker_slot: Option<usize>,
    cursor: bool,
    pinned: bool,
    compare_base: bool,
    focused: bool,
    width: usize,
) -> Line<'static> {
    let marker = if pinned {
        "▶"
    } else if compare_base {
        "◆"
    } else {
        " "
    };
    let change = if message.changed { "●" } else { " " };
    let label = if width >= 42 {
        if message.changed {
            "Changed  "
        } else {
            "Unchanged"
        }
    } else if width >= 34 {
        if message.changed { "chg" } else { "   " }
    } else {
        ""
    };
    let retain = if message.retain { " R" } else { "" };
    let mut text = format!("{} {change}", clock(message.wall));
    if !label.is_empty() {
        text.push(' ');
        text.push_str(label);
    }
    text.push_str(&format!(
        " {:>7} Q{}{retain}",
        format_size(message.payload.len()),
        message.qos
    ));
    let mut spans = vec![Span::raw(format!("{marker} "))];
    if let Some(slot) = broker_slot {
        spans.push(Span::styled(format!("{slot} "), theme::broker_style(slot)));
    }
    let room = width.saturating_sub(line_width(&spans));
    spans.push(Span::styled(
        truncate(&text, room),
        if message.changed {
            Style::default()
        } else {
            theme::muted()
        },
    ));
    if cursor {
        let style = if focused {
            theme::selection()
        } else {
            theme::selection_inactive()
        };
        let used = line_width(&spans);
        spans = restyle(spans, style);
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), style));
        }
    }
    Line::from(spans)
}

// -------------------------------------------------------- broker manager

fn tls_cell(status: Option<&TlsStatus>) -> Cell<'static> {
    match status {
        None | Some(TlsStatus::Off) => Cell::from(Span::styled("–", theme::muted())),
        Some(TlsStatus::Valid { mutual }) => Cell::from(Span::styled(
            if *mutual { "✓ mTLS" } else { "✓" },
            Style::default().fg(theme::OK),
        )),
        Some(TlsStatus::Invalid(_)) => {
            Cell::from(Span::styled("!", Style::default().fg(theme::ERROR)))
        }
    }
}

fn status_cell(state: &UiState, profile: &BrokerConfig) -> Cell<'static> {
    if let Some(connection) = state.connection_of(&profile.id) {
        return Cell::from(Span::styled(
            theme::state_label(connection.state),
            Style::default().fg(theme::state_color(connection.state)),
        ));
    }
    match state.test_results.get(&profile.id) {
        Some(TestOutcome::Running) => {
            Cell::from(Span::styled("Testing…", Style::default().fg(theme::WARN)))
        }
        Some(TestOutcome::Passed) => {
            Cell::from(Span::styled("Test OK", Style::default().fg(theme::OK)))
        }
        Some(TestOutcome::Failed(_)) => Cell::from(Span::styled(
            "Test failed",
            Style::default().fg(theme::ERROR),
        )),
        None => Cell::from(Span::styled("Disconnected", theme::muted())),
    }
}

fn render_broker_manager(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let show_details = area.height >= 17;
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if show_details {
            vec![Constraint::Min(5), Constraint::Length(10)]
        } else {
            vec![Constraint::Min(5)]
        })
        .split(area);

    let header = Row::new(
        ["", "NAME", "HOST", "TLS", "STATUS"]
            .map(|title| Cell::from(Span::styled(title, theme::bold().fg(theme::MUTED)))),
    );
    let rows: Vec<Row> = state
        .config
        .brokers
        .iter()
        .map(|profile| {
            Row::new(vec![
                Cell::from(Span::styled(
                    if state.marked_brokers.contains(&profile.id) {
                        "✓"
                    } else {
                        " "
                    },
                    Style::default().fg(theme::OK),
                )),
                Cell::from(
                    if state.config.general.default_broker.as_deref() == Some(&profile.id) {
                        format!("{} (default)", profile.name)
                    } else {
                        profile.name.clone()
                    },
                ),
                Cell::from(format!("{}:{}", profile.host, profile.port)),
                tls_cell(state.broker_tls.get(&profile.id)),
                status_cell(state, profile),
            ])
        })
        .collect();
    let empty = rows.is_empty();
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(28),
            Constraint::Percentage(38),
            Constraint::Length(8),
            Constraint::Min(12),
        ],
    )
    .header(header)
    .row_highlight_style(theme::selection())
    .block(panel_block("BROKER CONNECTIONS", true));
    let mut table_state = TableState::default();
    if !empty {
        table_state.select(Some(
            state.broker_selection.min(state.config.brokers.len() - 1),
        ));
    }
    frame.render_stateful_widget(table, parts[0], &mut table_state);
    if empty {
        let inner = panel_block("", true).inner(parts[0]);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No broker profiles yet. Press a to add one.",
                theme::muted(),
            )),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }

    if show_details {
        render_broker_details(frame, state, parts[1]);
    }
}

fn render_broker_details(frame: &mut Frame, state: &UiState, area: Rect) {
    let block = panel_block("DETAILS", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(profile) = state.config.brokers.get(state.broker_selection) else {
        return;
    };
    let label = |text: &str| Span::styled(format!("{text:<16}"), theme::muted());
    let mut lines = vec![
        Line::from(vec![
            label("Profile"),
            Span::styled(profile.name.clone(), theme::bold()),
            Span::styled(format!("  ({})", profile.id), theme::muted()),
        ]),
        Line::from(vec![
            label("Endpoint"),
            Span::raw(format!(
                "{}:{}  ·  {}  ·  client ID {}",
                profile.host,
                profile.port,
                protocol_name(&profile.protocol),
                if profile.unique_client_id {
                    format!("{}-<random>", profile.client_id)
                } else {
                    profile.client_id.clone()
                }
            )),
        ]),
        Line::from(vec![
            label("Authentication"),
            Span::raw(match &profile.auth.username {
                Some(user) => format!("user {user}, password from the OS keyring"),
                None => "none".to_owned(),
            }),
        ]),
    ];
    lines.push(Line::from(vec![
        label("TLS"),
        match state.broker_tls.get(&profile.id) {
            None | Some(TlsStatus::Off) => Span::styled("disabled", theme::muted()),
            Some(TlsStatus::Valid { mutual }) => Span::styled(
                if *mutual {
                    "certificates valid, client certificate configured"
                } else {
                    "enabled, certificates valid"
                },
                Style::default().fg(theme::OK),
            ),
            Some(TlsStatus::Invalid(error)) => {
                Span::styled(error.clone(), Style::default().fg(theme::ERROR))
            }
        },
    ]));
    lines.push(Line::from(vec![
        label("Subscriptions"),
        Span::raw(if profile.subscriptions.is_empty() {
            "none".to_owned()
        } else {
            profile
                .subscriptions
                .iter()
                .map(|subscription| {
                    if subscription.enabled {
                        format!("{} (QoS {})", subscription.topic, subscription.qos)
                    } else {
                        format!("{} (off)", subscription.topic)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }),
    ]));
    lines.push(Line::from(vec![
        label("Publishing"),
        if profile.allow_publish {
            Span::styled("allowed (w to forbid)", Style::default().fg(theme::WARN))
        } else {
            Span::styled("read-only (w to allow)", theme::muted())
        },
    ]));
    lines.push(Line::from(vec![
        label("Auto-connect"),
        if profile.auto_connect {
            Span::styled("on (mate broker edit --auto-connect false)", theme::muted())
        } else {
            Span::styled(
                "off (mate --connect, or mate broker edit --auto-connect true)",
                theme::muted(),
            )
        },
    ]));
    lines.push(Line::from(vec![
        label("System topics"),
        if profile.show_system_topics {
            Span::styled(
                "shown, $SYS/# (y to hide)",
                Style::default().fg(theme::WARN),
            )
        } else {
            Span::styled("hidden (y to show)", theme::muted())
        },
    ]));
    if let Some(connection) = state.connection_of(&profile.id) {
        let mut text = format!(
            "{}  ·  {} msg/s  ·  {} topics  ·  {} dropped",
            theme::state_label(connection.state),
            format_rate(state.rate.per_second(state.now)),
            thousands(state.topics.topic_count()),
            state.dropped_messages
        );
        if let Some(detail) = &connection.detail {
            text.push_str(&format!("  ·  {detail}"));
        }
        lines.push(Line::from(vec![
            label("Connection"),
            Span::styled(
                text,
                Style::default().fg(theme::state_color(connection.state)),
            ),
        ]));
    }
    match state.test_results.get(&profile.id) {
        Some(TestOutcome::Passed) => lines.push(Line::from(vec![
            label("Last test"),
            Span::styled("connected successfully", Style::default().fg(theme::OK)),
        ])),
        Some(TestOutcome::Failed(error)) => lines.push(Line::from(vec![
            label("Last test"),
            Span::styled(error.clone(), Style::default().fg(theme::ERROR)),
        ])),
        Some(TestOutcome::Running) => lines.push(Line::from(vec![
            label("Last test"),
            Span::styled("running…", Style::default().fg(theme::WARN)),
        ])),
        None => {}
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn protocol_name(protocol: &str) -> &'static str {
    super::protocol_label(protocol)
}

fn render_subscription_manager(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let Some(profile) = state.subscriptions_profile() else {
        return;
    };
    let selection = state
        .subscriptions
        .as_ref()
        .map_or(0, |view| view.selection);
    let live = state.has_connection(&profile.id);
    let block = panel_block(&format!("SUBSCRIPTIONS · {}", profile.name), true);
    let inner = block.inner(area);
    let mut lines = Vec::new();
    if profile.subscriptions.is_empty() {
        lines.push(Line::from(Span::styled(
            "No subscriptions. Press a to add a topic filter, x to exclude topics.",
            theme::muted(),
        )));
    }
    for (index, subscription) in profile.subscriptions.iter().enumerate() {
        let style = if index == selection {
            theme::selection()
        } else if subscription.enabled {
            Style::default()
        } else {
            theme::muted()
        };
        let kind = if subscription.exclude {
            "exclude".to_owned()
        } else {
            format!("QoS {}", subscription.qos)
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{} {:<48} {kind}",
                if subscription.enabled { "[x]" } else { "[ ]" },
                truncate(&subscription.topic, 48),
            ),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if live {
            "Changes apply immediately to the running connection."
        } else {
            "Changes apply the next time this broker is connected."
        },
        theme::muted(),
    )));
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_subscription_editor(frame: &mut Frame, state: &UiState, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(editor) = state
        .subscriptions
        .as_ref()
        .and_then(|view| view.editor.as_ref())
    else {
        return;
    };
    let qos = editor.qos.to_string();
    let mut fields = vec![
        (
            if editor.exclude {
                "Exclude filter"
            } else {
                "Topic filter"
            },
            editor.topic.as_str(),
        ),
        ("QoS (0/1/2)", qos.as_str()),
    ];
    if editor.exclude {
        fields.truncate(1);
    }
    let mut lines = Vec::new();
    for (index, (label, value)) in fields.iter().enumerate() {
        let current = editor.field == index;
        lines.push(Line::from(vec![
            Span::styled(
                if current { "> " } else { "  " },
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(format!("{label:<16}"), theme::muted()),
            Span::styled(
                (*value).to_owned(),
                if current {
                    theme::bold()
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                if current && index == 0 { "▌" } else { "" },
                Style::default().fg(theme::ACCENT),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Wildcards: + one level, # the rest (last level only).",
        theme::muted(),
    )));
    if editor.exclude {
        lines.push(Line::from(Span::styled(
            "Messages matching this filter are dropped after the broker sent them, e.g. +/hmi/# or plant/line-1/#.",
            theme::muted(),
        )));
    }
    if let Some(error) = &editor.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::ERROR),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel_block(
                if editor.exclude {
                    "ADD EXCLUSION"
                } else {
                    "ADD SUBSCRIPTION"
                },
                true,
            )),
        area,
    );
    popup_footer(frame, &shortcut_items(state), area);
}

fn render_certificate_manager(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let block = panel_block("CERTIFICATES", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if state.certificates.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No certificates configured. Press i to import or r to reference a file.",
                theme::muted(),
            )),
            inner,
        );
        return;
    }
    let mut lines = Vec::new();
    for (index, certificate) in state.certificates.iter().enumerate() {
        let origin = if certificate.imported {
            "imported"
        } else {
            "external"
        };
        let style = if index == state.certificate_selection {
            theme::selection()
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{:<14} {origin:<9} {}",
                certificate.kind.filename(),
                certificate.path.display()
            ),
            style,
        )));
        if let Some(subject) = &certificate.subject {
            lines.push(Line::from(Span::styled(
                format!("  subject: {subject}"),
                theme::muted(),
            )));
        }
        if let Some(expiry) = &certificate.not_after {
            let expired = if certificate.expired {
                " (expired)"
            } else {
                ""
            };
            lines.push(Line::from(Span::styled(
                format!("  expires: {expiry}{expired}"),
                if certificate.expired {
                    Style::default().fg(theme::ERROR)
                } else {
                    theme::muted()
                },
            )));
        }
        if let Some(error) = &certificate.error {
            lines.push(Line::from(Span::styled(
                format!("  error: {error}"),
                Style::default().fg(theme::ERROR),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

// -------------------------------------------------------------- analytics

fn fit_left(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{text}{}", " ".repeat(padding))
}

fn fit_right(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{}{text}", " ".repeat(padding))
}

fn format_hz(rate: f64) -> String {
    if rate >= 100.0 {
        format!("{rate:.0}/s")
    } else if rate >= 10.0 {
        format!("{rate:.1}/s")
    } else {
        format!("{rate:.2}/s")
    }
}

fn format_factor(factor: f64) -> String {
    if factor >= 10.0 {
        format!("{factor:.0}×")
    } else {
        format!("{factor:.1}×")
    }
}

fn kind_style(kind: PayloadKind) -> Style {
    match kind {
        PayloadKind::BrokenJson => Style::default().fg(theme::ERROR),
        PayloadKind::Binary => Style::default().fg(theme::WARN),
        _ => theme::muted(),
    }
}

/// A fixed-width column of an analytics list; the topic (and detail) fill the rest.
struct AnalyticsColumn {
    title: &'static str,
    width: usize,
    left: bool,
    sort: Option<SortKey>,
}

const fn column(title: &'static str, width: usize, sort: Option<SortKey>) -> AnalyticsColumn {
    AnalyticsColumn {
        title,
        width,
        left: false,
        sort,
    }
}

fn analytics_columns(mode: AnalyticsMode) -> &'static [AnalyticsColumn] {
    match mode {
        AnalyticsMode::Frequency => {
            const COLUMNS: &[AnalyticsColumn] = &[
                column("COUNT", 8, Some(SortKey::Count)),
                column("RATE", 9, Some(SortKey::Rate)),
                column("INTERVAL", 9, None),
                column("LAST", 9, None),
            ];
            COLUMNS
        }
        AnalyticsMode::InvalidJson => {
            const COLUMNS: &[AnalyticsColumn] = &[
                AnalyticsColumn {
                    title: "KIND",
                    width: 7,
                    left: true,
                    sort: None,
                },
                column("COUNT", 8, Some(SortKey::Count)),
                column("SIZE", 9, Some(SortKey::Size)),
            ];
            COLUMNS
        }
        AnalyticsMode::Quiet => {
            const COLUMNS: &[AnalyticsColumn] = &[
                column("SILENT", 9, Some(SortKey::Age)),
                column("USUAL", 9, None),
                column("OVERDUE", 9, Some(SortKey::Overdue)),
            ];
            COLUMNS
        }
        AnalyticsMode::Jitter => {
            const COLUMNS: &[AnalyticsColumn] = &[
                column("JITTER", 9, Some(SortKey::Jitter)),
                column("MEAN", 9, None),
                column("MIN", 9, None),
                column("MAX", 9, Some(SortKey::MaxInterval)),
            ];
            COLUMNS
        }
        AnalyticsMode::Largest => {
            const COLUMNS: &[AnalyticsColumn] = &[
                column("SIZE", 9, Some(SortKey::Size)),
                column("PEAK", 9, Some(SortKey::Peak)),
                column("COUNT", 8, Some(SortKey::Count)),
            ];
            COLUMNS
        }
        AnalyticsMode::Retained => {
            const COLUMNS: &[AnalyticsColumn] = &[
                column("SIZE", 9, Some(SortKey::Size)),
                column("AGE", 9, Some(SortKey::Age)),
            ];
            COLUMNS
        }
        AnalyticsMode::Branches => {
            const COLUMNS: &[AnalyticsColumn] = &[
                column("COUNT", 8, Some(SortKey::Count)),
                column("RATE", 9, Some(SortKey::Rate)),
                column("TOPICS", 7, Some(SortKey::Topics)),
                column("LAST", 9, None),
            ];
            COLUMNS
        }
    }
}

fn analytics_cells(mode: AnalyticsMode, row: &AnalyticsRow, now: Instant) -> Vec<String> {
    let dash = |text: Option<String>| text.unwrap_or_else(|| "-".to_owned());
    let age = duration_text(now.saturating_duration_since(row.received));
    let interval = |value: Option<Duration>| dash(value.map(duration_text));
    match mode {
        AnalyticsMode::Frequency => vec![
            compact_count(row.count),
            dash(row.rate.map(format_hz)),
            interval(row.interval),
            age,
        ],
        AnalyticsMode::InvalidJson => vec![
            row.kind.label().to_owned(),
            compact_count(row.count),
            format_size(row.size),
        ],
        AnalyticsMode::Quiet => vec![
            age,
            interval(row.interval),
            dash(row.overdue.map(format_factor)),
        ],
        AnalyticsMode::Jitter => vec![
            dash(row.jitter.map(|jitter| format!("{:.0}%", jitter * 100.0))),
            interval(row.interval),
            interval(row.min_interval),
            interval(row.max_interval),
        ],
        AnalyticsMode::Largest => vec![
            format_size(row.size),
            format_size(row.peak),
            compact_count(row.count),
        ],
        AnalyticsMode::Retained => vec![format_size(row.size), age],
        AnalyticsMode::Branches => vec![
            compact_count(row.count),
            dash(row.rate.map(format_hz)),
            thousands(row.topics),
            age,
        ],
    }
}

fn analytics_note(view: &AnalyticsView) -> String {
    let sort = view.sort.label();
    let order = if view.descending {
        "descending"
    } else {
        "ascending"
    };
    let what = match view.mode {
        AnalyticsMode::Frequency => "Counted since the last connection attempt.".to_owned(),
        AnalyticsMode::InvalidJson => format!(
            "Latest payload; filter: {}.",
            view.kind_filter.map_or("all kinds", |kind| match kind {
                PayloadKind::BrokenJson => "broken JSON only",
                PayloadKind::Text => "plain text only",
                PayloadKind::Binary => "binary only",
                PayloadKind::Empty => "empty only",
                PayloadKind::Json => "JSON",
            })
        ),
        AnalyticsMode::Quiet => "OVERDUE = silence divided by the usual interval.".to_owned(),
        AnalyticsMode::Jitter => format!(
            "JITTER = spread of the intervals relative to their mean; needs {}+ messages in the history.",
            mate::analytics::MIN_JITTER_INTERVALS + 1
        ),
        AnalyticsMode::Largest => {
            "SIZE = latest payload, PEAK = largest in the history.".to_owned()
        }
        AnalyticsMode::Retained => {
            "Topics whose latest message carried the retain flag.".to_owned()
        }
        AnalyticsMode::Branches => format!(
            "Grouped by the first {} topic level{} (d changes it).",
            view.depth,
            if view.depth == 1 { "" } else { "s" }
        ),
    };
    format!("Sorted by {sort}, {order}. {what}")
}

fn render_analytics(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let title = if state.analytics.paused {
        "ANALYTICS \u{b7} PAUSED"
    } else {
        "ANALYTICS"
    };
    let block = panel_block(title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Tabs, note and header above the list, one footer line below it.
    let list_height = (inner.height as usize).saturating_sub(4);
    state.analytics.viewport = list_height.max(1);
    let view = &state.analytics;
    let width = inner.width as usize;
    let mode = view.mode;
    let reference = view.reference(state.now);

    let tabs: Vec<Span<'static>> = AnalyticsMode::ALL
        .iter()
        .map(|tab| {
            if *tab == mode {
                Span::styled(
                    format!("[{}]", tab.label()),
                    theme::bold().fg(theme::ACCENT),
                )
            } else {
                Span::styled(format!(" {} ", tab.label()), theme::muted())
            }
        })
        .collect();

    let columns = analytics_columns(mode);
    let with_detail = matches!(mode, AnalyticsMode::InvalidJson | AnalyticsMode::Retained);
    let mark = |sort: Option<SortKey>| match (sort == Some(view.sort), view.descending) {
        (true, true) => "▼",
        (true, false) => "▲",
        _ => "",
    };
    let mut header = String::new();
    for column in columns {
        let title = format!("{}{}", column.title, mark(column.sort));
        header.push_str(&if column.left {
            fit_left(&title, column.width)
        } else {
            fit_right(&title, column.width)
        });
        header.push(' ');
    }
    header.push_str(if with_detail {
        " TOPIC / DETAIL"
    } else {
        " TOPIC"
    });

    let start = scroll_into_view(view.scroll, view.selection, list_height, view.rows.len());
    let mut lines = vec![
        Line::from(tabs),
        Line::from(Span::styled(
            truncate(&analytics_note(view), width),
            theme::muted(),
        )),
        Line::from(Span::styled(
            truncate(&header, width),
            theme::bold().fg(theme::MUTED),
        )),
    ];
    let fixed: usize = columns.iter().map(|column| column.width + 1).sum::<usize>() + 1;
    let rest = width.saturating_sub(fixed);
    if view.rows.is_empty() {
        let text = match mode {
            AnalyticsMode::Frequency => {
                "No topic received a message since the last connection attempt."
            }
            AnalyticsMode::InvalidJson => "No topic with a payload that is not valid JSON.",
            AnalyticsMode::Jitter => "No topic has enough messages in its history yet.",
            AnalyticsMode::Retained => "No topic whose latest message was retained.",
            _ => "No topics yet.",
        };
        lines.push(Line::from(Span::styled(text, theme::muted())));
    }
    for (index, row) in view.rows.iter().enumerate().skip(start).take(list_height) {
        let selected = index == view.selection;
        let base = if selected {
            theme::selection()
        } else {
            Style::default()
        };
        let mut spans = Vec::new();
        for (position, (column, cell)) in columns
            .iter()
            .zip(analytics_cells(mode, row, reference))
            .enumerate()
        {
            let text = if column.left {
                fit_left(&cell, column.width)
            } else {
                fit_right(&cell, column.width)
            };
            let style = if position == 0 && mode == AnalyticsMode::InvalidJson && !selected {
                kind_style(row.kind)
            } else {
                base
            };
            spans.push(Span::styled(format!("{text} "), style));
        }
        spans.push(Span::styled(" ", base));
        if with_detail {
            let topic_width = (rest * 45 / 100).max(10).min(rest);
            spans.push(Span::styled(fit_left(&row.topic, topic_width), base));
            spans.push(Span::styled(
                format!(
                    " {}",
                    truncate(&row.detail, rest.saturating_sub(topic_width + 1))
                ),
                if selected { base } else { theme::muted() },
            ));
        } else {
            spans.push(Span::styled(truncate(&row.topic, rest), base));
        }
        if selected {
            let used = line_width(&spans);
            if used < width {
                spans.push(Span::styled(" ".repeat(width - used), base));
            }
        }
        lines.push(Line::from(spans));
    }

    let unit = match (mode == AnalyticsMode::Branches, view.matching == 1) {
        (true, true) => "branch",
        (true, false) => "branches",
        (false, true) => "topic",
        (false, false) => "topics",
    };
    let mut footer = String::new();
    if view.filtering || !view.filter.is_empty() {
        let caret = if view.filtering { "\u{258c}" } else { "" };
        footer.push_str(&format!("/{}{caret} \u{b7} ", view.filter));
    }
    footer.push_str(&format!("{} {unit}", thousands(view.matching)));
    if view.matching > view.rows.len() {
        footer.push_str(&format!(
            ", showing the first {}",
            thousands(view.rows.len())
        ));
    }
    if mode == AnalyticsMode::Frequency && view.idle > 0 {
        footer.push_str(&format!(
            "; {} cached topics without messages in this session hidden",
            thousands(view.idle)
        ));
    }
    let footer_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1.min(inner.height),
    );
    let list_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    state.analytics.scroll = start;
    frame.render_widget(Paragraph::new(lines), list_area);
    frame.render_widget(
        Paragraph::new(Span::styled(truncate(&footer, width), theme::muted())),
        footer_area,
    );
}

// ----------------------------------------------------------------- modals

/// Words are kept whole; a word wider than the line is cut.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        if !current.is_empty() && display_width(&current) + 1 + display_width(&word) > width {
            lines.push(std::mem::take(&mut current));
        }
        while display_width(&word) > width {
            let mut head = String::new();
            for character in word.chars() {
                let cell = UnicodeWidthChar::width(character).unwrap_or(0);
                if display_width(&head) + cell > width {
                    break;
                }
                head.push(character);
            }
            if head.is_empty() {
                break;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            word = word[head.len()..].to_owned();
            lines.push(head);
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&word);
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

const HELP_KEY_WIDTH: usize = 20;

fn help_entry(lines: &mut Vec<Line<'static>>, keys: &str, text: &str, width: usize) {
    let body = width.saturating_sub(HELP_KEY_WIDTH).max(10);
    for (index, part) in wrap_words(text, body).into_iter().enumerate() {
        let lead = if index == 0 {
            Span::styled(
                fit_left(keys, HELP_KEY_WIDTH),
                theme::bold().fg(theme::ACCENT),
            )
        } else {
            Span::raw(" ".repeat(HELP_KEY_WIDTH))
        };
        lines.push(Line::from(vec![lead, Span::raw(part)]));
    }
}

fn changelog_lines(lines: &mut Vec<Line<'static>>, width: usize) {
    let mut after_heading = false;
    for line in help::CHANGELOG.lines() {
        let line = line.replace('`', "");
        if let Some(title) = line.strip_prefix("## ") {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                title.to_owned(),
                theme::bold().fg(theme::ACCENT),
            )));
            after_heading = true;
            continue;
        }
        if let Some(title) = line.strip_prefix("### ") {
            if !after_heading {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(title.to_owned(), theme::bold())));
        } else if let Some(item) = line.strip_prefix("- ") {
            for (index, part) in wrap_words(item, width.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                let bullet = if index == 0 { "• " } else { "  " };
                lines.push(Line::from(format!("{bullet}{part}")));
            }
        } else if line.starts_with("# ") || line.trim().is_empty() {
            continue;
        } else {
            lines.extend(wrap_words(&line, width).into_iter().map(Line::from));
        }
        after_heading = false;
    }
}

fn about_lines(state: &UiState, lines: &mut Vec<Line<'static>>, width: usize) {
    lines.push(Line::from(Span::styled(
        "MATE - MQTT Administration & Terminal Environment",
        theme::bold().fg(theme::ACCENT),
    )));
    lines.push(Line::default());
    let version = env!("CARGO_PKG_VERSION");
    let build = if version.contains('-') {
        format!("{version} (development build)")
    } else {
        version.to_owned()
    };
    let root = state.broker_store.root();
    let rows = [
        ("Version", build),
        ("License", env!("CARGO_PKG_LICENSE").to_owned()),
        ("Project", env!("CARGO_PKG_REPOSITORY").to_owned()),
        ("Config folder", root.display().to_string()),
        (
            "UI settings",
            root.join("ui-state.toml").display().to_string(),
        ),
        (
            "Broker profiles",
            root.join("brokers").display().to_string(),
        ),
        (
            "Passwords",
            "Kept in the operating system's keyring, never in these files".to_owned(),
        ),
        (
            "Built with",
            "ratatui, crossterm, rumqttc (MQTT 3.1.1 and 5), rustls, tokio".to_owned(),
        ),
    ];
    for (label, value) in rows {
        help_entry(lines, label, &value, width);
    }
}

/// The playing field of the hidden snake tab, two characters per cell.
fn snake_lines(state: &UiState, lines: &mut Vec<Line<'static>>, width: usize, height: usize) {
    let Some(game) = &state.help.snake else {
        return;
    };
    let columns = snake::WIDTH as usize;
    let rows = snake::HEIGHT as usize;
    let field_width = columns * 2 + 2;
    if width < field_width || height < rows + 5 {
        lines.push(Line::from(Span::styled(
            "Snake needs a larger window.",
            theme::muted(),
        )));
        return;
    }
    let pad = " ".repeat((width - field_width) / 2);
    let frame_style = theme::subtle();
    lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled("Snake", theme::bold()),
        Span::styled(
            format!("   Score {}   Best {}", game.score(), game.best()),
            theme::muted(),
        ),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(format!("┌{}┐", "──".repeat(columns)), frame_style),
    ]));
    for y in 0..snake::HEIGHT {
        let mut spans = vec![Span::raw(pad.clone()), Span::styled("│", frame_style)];
        for x in 0..snake::WIDTH {
            let cell = (x, y);
            spans.push(if cell == game.head() {
                Span::styled("██", theme::bold().fg(theme::ACCENT))
            } else if game.contains(cell) {
                Span::styled("██", Style::default().fg(theme::OK))
            } else if cell == game.food() {
                Span::styled("● ", Style::default().fg(theme::ERROR))
            } else {
                Span::raw("  ")
            });
        }
        spans.push(Span::styled("│", frame_style));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(format!("└{}┘", "──".repeat(columns)), frame_style),
    ]));
    let status = match game.phase() {
        snake::Phase::Ready => "Press an arrow key or Enter to start",
        snake::Phase::Running => "",
        snake::Phase::Paused => "Paused: Enter continues",
        snake::Phase::Crashed => "Crashed. Enter plays again",
        snake::Phase::Won => "You filled the whole field! Enter plays again",
    };
    lines.push(Line::from(vec![
        Span::raw(pad),
        Span::styled(status, Style::default().fg(theme::WARN)),
    ]));
}

/// The lines of the current tab plus the line index of each section title.
fn help_lines(
    state: &UiState,
    width: usize,
    height: usize,
) -> (Vec<Line<'static>>, Vec<(&'static str, usize)>) {
    let mut lines = Vec::new();
    let mut anchors = Vec::new();
    match state.help.tab {
        HelpTab::Changelog => changelog_lines(&mut lines, width),
        HelpTab::About => about_lines(state, &mut lines, width),
        HelpTab::Snake => snake_lines(state, &mut lines, width, height),
        tab => {
            for block in help::blocks(tab) {
                match block {
                    HelpBlock::Section(title) => {
                        if !lines.is_empty() {
                            lines.push(Line::default());
                        }
                        anchors.push((*title, lines.len()));
                        lines.push(Line::from(Span::styled(*title, theme::bold())));
                    }
                    HelpBlock::Entry(keys, text) => help_entry(&mut lines, keys, text, width),
                    HelpBlock::Text(text) => {
                        lines.extend(wrap_words(text, width).into_iter().map(Line::from));
                    }
                }
            }
        }
    }
    (lines, anchors)
}

fn render_help(frame: &mut Frame, state: &mut UiState, area: Rect) {
    frame.render_widget(Clear, area);
    let block = panel_block("HELP", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let width = inner.width as usize;
    let height = inner.height as usize - 3;
    let (lines, anchors) = help_lines(state, width, height);
    let help = &mut state.help;
    help.viewport = height;
    help.max_scroll = lines.len().saturating_sub(height);
    if let Some(title) = help.jump.take()
        && let Some((_, index)) = anchors.iter().find(|(name, _)| *name == title)
    {
        help.scroll = *index;
    }
    help.scroll = help.scroll.min(help.max_scroll);

    let tabs: Vec<Span<'static>> = HelpTab::ALL
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let label = format!("{} {}", index + 1, tab.label());
            if *tab == help.tab {
                Span::styled(format!("[{label}]"), theme::bold().fg(theme::ACCENT))
            } else {
                Span::styled(format!(" {label} "), theme::muted())
            }
        })
        .collect();
    let mut tabs = tabs;
    if help.tab == HelpTab::Snake {
        tabs.push(Span::styled(
            format!("[6 {}]", HelpTab::Snake.label()),
            theme::bold().fg(theme::ACCENT),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(tabs)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if help.max_scroll > 0 && inner.width > 8 {
        let position = format!("{}%", help.scroll * 100 / help.max_scroll);
        frame.render_widget(
            Paragraph::new(Span::styled(fit_right(&position, 5), theme::muted())),
            Rect::new(inner.x + inner.width - 5, inner.y, 5, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled("─".repeat(width), theme::subtle())),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    let visible: Vec<Line<'static>> = lines.into_iter().skip(help.scroll).take(height).collect();
    frame.render_widget(
        Paragraph::new(visible),
        Rect::new(inner.x, inner.y + 2, inner.width, height as u16),
    );
    popup_footer(frame, &shortcut_items(state), area);
}

fn render_log_dialog(frame: &mut Frame, state: &UiState, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(dialog) = &state.log_dialog else {
        return;
    };
    let accent = Style::default().fg(theme::ACCENT);
    let current = |field: usize| dialog.field == field;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            if current(FIELD_LOG_ENABLED) {
                "> "
            } else {
                "  "
            },
            accent,
        ),
        Span::styled(format!("{:<12}", "Logging"), theme::muted()),
        Span::styled(
            if dialog.enabled { "[x] on" } else { "[ ] off" },
            if current(FIELD_LOG_ENABLED) {
                theme::bold()
            } else {
                Style::default()
            },
        ),
        Span::styled(
            if current(FIELD_LOG_ENABLED) {
                "   (Space switches)"
            } else {
                ""
            },
            theme::muted(),
        ),
    ])];
    let mut directory = vec![
        Span::styled(
            if current(FIELD_LOG_DIRECTORY) {
                "> "
            } else {
                "  "
            },
            accent,
        ),
        Span::styled(format!("{:<12}", "Directory"), theme::muted()),
    ];
    if current(FIELD_LOG_DIRECTORY) {
        let at = super::publish::byte_at(&dialog.directory, dialog.cursor);
        let (before, after) = dialog.directory.split_at(at);
        let mut rest = after.chars();
        let under = rest.next();
        directory.push(Span::styled(before.to_owned(), theme::bold()));
        directory.push(Span::styled(
            under.map_or(" ".to_owned(), String::from),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        directory.push(Span::styled(rest.collect::<String>(), theme::bold()));
    } else if dialog.directory.trim().is_empty() {
        directory.push(Span::styled(
            "(default: logs in the config folder)",
            theme::muted(),
        ));
    } else {
        directory.push(Span::raw(dialog.directory.clone()));
    }
    lines.push(Line::from(directory));
    lines.push(Line::from(vec![
        Span::styled(if current(FIELD_LOG_SIZE) { "> " } else { "  " }, accent),
        Span::styled(format!("{:<12}", "Max. size"), theme::muted()),
        Span::styled(
            format!("{} MB", dialog.size),
            if current(FIELD_LOG_SIZE) {
                theme::bold()
            } else {
                Style::default()
            },
        ),
        Span::styled(
            if current(FIELD_LOG_SIZE) {
                "\u{258c}"
            } else {
                ""
            },
            accent,
        ),
        Span::styled("  per file, then the next block starts", theme::muted()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "One file per broker, day and block (the number counts up within a day):",
        theme::muted(),
    )));
    let width = (area.width as usize).saturating_sub(6);
    let files = state.log_files_preview(dialog);
    if files.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (select a broker first)",
            theme::muted(),
        )));
    }
    for (name, path) in files {
        let text = format!("  {name}  \u{2192}  {}", path.display());
        lines.push(Line::from(Span::raw(truncate(&text, width))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Format: UTC time | T0   I: topic [MQTT 5 user properties] payload (see [log_format])",
        theme::muted(),
    )));
    lines.push(Line::from(Span::styled(
        "Incoming messages only; payloads with control characters or binary are escaped (\\:\u{2026}, 0x\u{2026}).",
        theme::muted(),
    )));
    if let Some(error) = &dialog.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::ERROR),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel_block("LOGGING (.mqttlog)", true)),
        area,
    );
    popup_footer(frame, &shortcut_items(state), area);
}

fn render_broker_editor(frame: &mut Frame, state: &UiState, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(editor) = &state.editor else {
        return;
    };
    let masked_password = "*".repeat(editor.password.chars().count());
    let password_label = if editor.editing_id.is_some() {
        "Password (blank=keep)"
    } else {
        "Password"
    };
    let protocol_text = format!(
        "{}  (←/→ to change)",
        super::protocol_label(&editor.protocol)
    );
    let editing = editor.editing_id.is_some();
    let subscription_text = if editing {
        "(manage with s in the broker menu)"
    } else {
        editor.subscription.as_str()
    };
    let fields = [
        ("Name", editor.name.as_str()),
        ("Hostname", editor.host.as_str()),
        ("Port", editor.port.as_str()),
        ("Protocol", protocol_text.as_str()),
        ("Subscription", subscription_text),
        ("Username", editor.username.as_str()),
        (password_label, masked_password.as_str()),
    ];
    let mut lines = Vec::new();
    for (index, (label, value)) in fields.iter().enumerate() {
        let current = editor.field == index;
        let inert = index == 3 || (editing && index == 4);
        lines.push(Line::from(vec![
            Span::styled(
                if current { "> " } else { "  " },
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(format!("{label:<22}"), theme::muted()),
            Span::styled(
                (*value).to_owned(),
                if editing && index == 4 {
                    theme::muted()
                } else if current {
                    theme::bold()
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                if current && !inert { "▌" } else { "" },
                Style::default().fg(theme::ACCENT),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "The password is stored in the OS keyring, never in the profile file.",
        theme::muted(),
    )));
    if let Some(error) = &editor.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::ERROR),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel_block(
                if editor.editing_id.is_some() {
                    "EDIT BROKER"
                } else {
                    "ADD BROKER"
                },
                true,
            )),
        area,
    );
    popup_footer(frame, &shortcut_items(state), area);
}

/// The path field with a visible cursor while it is focused.
fn certificate_path_line(editor: &super::CertificateEditor) -> Line<'static> {
    let focused = editor.field == 1;
    let chars: Vec<char> = editor.path.chars().collect();
    let mut spans = vec![Span::raw(format!(
        "Path: {}",
        if focused { "> " } else { "  " }
    ))];
    if focused {
        let at = editor.cursor.min(chars.len());
        spans.push(Span::raw(chars[..at].iter().collect::<String>()));
        spans.push(Span::styled(
            chars.get(at).copied().unwrap_or(' ').to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        spans.push(Span::raw(chars.iter().skip(at + 1).collect::<String>()));
    } else {
        spans.push(Span::raw(chars.iter().collect::<String>()));
    }
    Line::from(spans)
}

fn render_certificate_editor(frame: &mut Frame, state: &UiState, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(editor) = &state.certificate_editor else {
        return;
    };
    let action = match editor.mode {
        CertificateEditMode::Import => "Import certificate file",
        CertificateEditMode::Reference => "Reference external certificate",
    };
    let kind = match editor.kind {
        CertificateKind::Ca => "CA certificate",
        CertificateKind::ClientCertificate => "Client certificate",
        CertificateKind::ClientPrivateKey => "Client private key",
    };
    let mut lines = vec![
        Line::from(Span::styled(action, theme::bold())),
        Line::from("1 CA   2 Client cert   3 Client key"),
        Line::from(format!("Kind: {kind}")),
        certificate_path_line(editor),
    ];
    if matches!(editor.mode, CertificateEditMode::Import) {
        lines.push(Line::from(format!(
            "Replace existing: {}  (toggle with !)",
            if editor.replace { "yes" } else { "no" }
        )));
    }
    if let Some(error) = &editor.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::ERROR),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel_block("CERTIFICATE", true)),
        area,
    );
    popup_footer(frame, &shortcut_items(state), area);
}

fn render_publish_panel(frame: &mut Frame, state: &UiState, area: Rect) {
    if state
        .publish_editor
        .as_ref()
        .is_some_and(|editor| editor.docked)
        && !state.connections.is_empty()
    {
        render_publish_editor(frame, state, area, true);
        return;
    }
    let active = state.main.focus == Panel::Publish;
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Connect to a broker that allows publishing (b, then w). P hides this panel.",
            theme::muted(),
        ))
        .wrap(Wrap { trim: true })
        .block(panel_block("PUBLISH", active)),
        area,
    );
}

/// `docked` draws the panel's edit form; otherwise the dialog, or its review screen.
fn render_publish_editor(frame: &mut Frame, state: &UiState, area: Rect, docked: bool) {
    frame.render_widget(Clear, area);
    let Some(editor) = &state.publish_editor else {
        return;
    };
    let Some(connection) = state.connection_of(&editor.broker_id) else {
        return;
    };
    let mut broker_line = vec![Span::styled("Broker   ", theme::muted())];
    if let Some(slot) = state.broker_badge(&connection.broker_id) {
        broker_line.push(Span::styled(format!("{slot} "), theme::broker_style(slot)));
    }
    broker_line.push(Span::styled(connection.broker_name.clone(), theme::bold()));
    if state.connections.len() > 1 && !docked {
        broker_line.push(Span::styled("  (Ctrl+B changes)", theme::muted()));
    }
    let mut lines = vec![Line::from(broker_line)];
    if docked {
        // The panel title names the broker; the rows save space.
        lines.clear();
    }
    if editor.stage == PublishStage::Review && !docked {
        lines.push(Line::from(vec![
            Span::styled("Topic    ", theme::muted()),
            Span::raw(editor.topic.trim().to_owned()),
        ]));
        let payload_lines: Vec<&str> = editor.payload.split('\n').collect();
        lines.push(Line::from(vec![
            Span::styled("Payload  ", theme::muted()),
            Span::raw(format!(
                "{} bytes, {} line{}",
                editor.payload.len(),
                payload_lines.len(),
                if payload_lines.len() == 1 { "" } else { "s" }
            )),
        ]));
        let shown = payload_lines.len().min(REVIEW_PAYLOAD_LINES);
        let highlight = super::publish::looks_like_json(&editor.payload);
        let room = (area.width as usize).saturating_sub(8);
        for line in &payload_lines[..shown] {
            if highlight && line.chars().count() <= MAX_HIGHLIGHT_CHARS {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(slice_chars(highlight_json_line(line).spans, 0, room));
                lines.push(Line::from(spans));
            } else {
                lines.push(Line::from(Span::raw(format!("  {}", truncate(line, room)))));
            }
        }
        if payload_lines.len() > shown {
            lines.push(Line::from(Span::styled(
                format!("  \u{2026} {} more lines", payload_lines.len() - shown),
                theme::muted(),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled("Delivery ", theme::muted()),
            Span::raw(format!(
                "QoS {}, {}",
                editor.qos,
                if editor.retain {
                    "retained"
                } else {
                    "not retained"
                }
            )),
        ]));
        lines.push(Line::from(""));
        for warning in editor.warnings() {
            lines.push(Line::from(Span::styled(
                format!("! {warning}"),
                theme::bold().fg(theme::WARN),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "Send this to {}?",
            connection.broker_name
        )));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(panel_block("REVIEW PUBLISH", true)),
            area,
        );
        popup_footer(frame, &shortcut_items(state), area);
        return;
    }

    let qos = (0..=2)
        .map(|level| {
            if level == editor.qos {
                format!("[{level}]")
            } else {
                format!(" {level} ")
            }
        })
        .collect::<Vec<_>>()
        .join("");
    let rows = [
        (FIELD_TOPIC, "Topic", editor.topic.clone()),
        (FIELD_PAYLOAD, "Payload", String::new()),
        (FIELD_QOS, "QoS", qos),
        (
            FIELD_RETAIN,
            "Retain",
            if editor.retain {
                "[x] yes".to_owned()
            } else {
                "[ ] no".to_owned()
            },
        ),
    ];
    let entries = state.publish_history_of(&editor.broker_id);
    let inner_height = (area.height as usize).saturating_sub(2);
    let fixed = if docked { 7 } else { 8 } + usize::from(editor.error.is_some());
    let reserved = if entries.is_empty() { 0 } else { 3 };
    let payload_rows = editor
        .line_count()
        .clamp(3, 12)
        .min(inner_height.saturating_sub(fixed + reserved).max(3));
    for (field, label, value) in rows {
        let current = editor.field == field;
        if field == FIELD_PAYLOAD {
            lines.extend(payload_editor_lines(
                editor,
                current,
                payload_rows,
                (area.width as usize).saturating_sub(4),
            ));
            continue;
        }
        if field == FIELD_TOPIC {
            lines.push(topic_editor_line(editor, current));
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                if current { "> " } else { "  " },
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(format!("{label:<9}"), theme::muted()),
            Span::styled(
                value,
                if current {
                    theme::bold()
                } else {
                    Style::default()
                },
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(match &editor.file_prompt {
        Some(path) => Line::from(vec![
            Span::styled("Load file: ", theme::bold().fg(theme::ACCENT)),
            Span::raw(path.clone()),
            Span::styled("\u{258c}", Style::default().fg(theme::ACCENT)),
        ]),
        None => Line::from(""),
    });
    let (cursor_line, cursor_col) = editor.line_col();
    lines.push(Line::from(Span::styled(
        if editor.field == FIELD_PAYLOAD {
            format!(
                "{} bytes \u{b7} line {}/{} col {}",
                editor.payload.len(),
                cursor_line + 1,
                editor.line_count(),
                cursor_col + 1
            )
        } else {
            format!(
                "Payload {} bytes, {} lines",
                editor.payload.len(),
                editor.line_count()
            )
        },
        theme::muted(),
    )));
    if let Some(error) = &editor.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::ERROR),
        )));
    }
    if !entries.is_empty() {
        lines.push(Line::from(""));
        let focused = editor.field == FIELD_RECENT;
        lines.push(Line::from(Span::styled(
            if focused {
                "Recent messages \u{2014} Enter loads one for editing, d forgets it"
            } else {
                "Recent messages \u{2014} \u{2193} to choose one"
            },
            theme::bold(),
        )));
        let width = area.width.saturating_sub(4) as usize;
        let room = (area.height as usize)
            .saturating_sub(lines.len() + 3)
            .clamp(1, 8);
        let scroll = (editor.history_selection + 1).saturating_sub(room);
        for (index, entry) in entries.iter().enumerate().skip(scroll).take(room) {
            let when =
                DateTime::<Local>::from(UNIX_EPOCH + std::time::Duration::from_secs(entry.sent_at))
                    .format("%m-%d %H:%M");
            let flags = format!("Q{}{}", entry.qos, if entry.retain { " R" } else { "" });
            let topic_width = (width / 3).clamp(10, 36);
            let payload = entry.payload.lines().next().unwrap_or("");
            let text = format!(
                "{when} {flags:<4} {:<topic_width$} {payload}",
                truncate(&entry.topic, topic_width)
            );
            let selected = focused && index == editor.history_selection;
            lines.push(Line::from(Span::styled(
                format!(
                    "{} {}",
                    if selected { ">" } else { " " },
                    truncate(&text, width.saturating_sub(2))
                ),
                if selected {
                    theme::selection()
                } else {
                    theme::muted()
                },
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel_block(
                &if docked {
                    format!("PUBLISH \u{b7} {}", connection.broker_name)
                } else {
                    "PUBLISH".to_owned()
                },
                !docked || state.main.focus == Panel::Publish,
            )),
        area,
    );
    if !docked {
        popup_footer(frame, &shortcut_items(state), area);
    }
}

/// Longer lines are drawn plain: coloring needs the whole line and the screen is redrawn often.
const MAX_HIGHLIGHT_CHARS: usize = 4096;
use super::logging::{
    FIELD_DIRECTORY as FIELD_LOG_DIRECTORY, FIELD_ENABLED as FIELD_LOG_ENABLED,
    FIELD_SIZE as FIELD_LOG_SIZE,
};

/// Keeps `take` characters after skipping `skip`, with their styles.
fn slice_chars(spans: Vec<Span<'static>>, skip: usize, take: usize) -> Vec<Span<'static>> {
    let mut seen = 0;
    let mut result = Vec::new();
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let start = skip.saturating_sub(seen).min(chars.len());
        let end = (skip + take).saturating_sub(seen).min(chars.len());
        if start < end {
            result.push(Span::styled(
                chars[start..end].iter().collect::<String>(),
                span.style,
            ));
        }
        seen += chars.len();
    }
    result
}

/// Shows the cursor on the character at `at`, or as a trailing blank behind the text.
fn mark_cursor(spans: Vec<Span<'static>>, at: usize) -> Vec<Span<'static>> {
    let mut seen = 0;
    let mut placed = false;
    let mut result = Vec::new();
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        if !placed && at >= seen && at < seen + chars.len() {
            let index = at - seen;
            if index > 0 {
                result.push(Span::styled(
                    chars[..index].iter().collect::<String>(),
                    span.style,
                ));
            }
            result.push(Span::styled(
                chars[index].to_string(),
                span.style.add_modifier(Modifier::REVERSED),
            ));
            if index + 1 < chars.len() {
                result.push(Span::styled(
                    chars[index + 1..].iter().collect::<String>(),
                    span.style,
                ));
            }
            placed = true;
        } else {
            result.push(span);
        }
        seen += chars.len();
    }
    if !placed {
        result.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    result
}

/// The payload rows of the publish dialog: several lines, a visible cursor and scrolling.
fn payload_editor_lines(
    editor: &super::publish::PublishEditor,
    focused: bool,
    rows: usize,
    width: usize,
) -> Vec<Line<'static>> {
    const INDENT: usize = 11;
    let (cursor_line, cursor_col) = editor.line_col();
    let text_width = width.saturating_sub(INDENT).max(1);
    let first = if focused {
        (cursor_line + 1).saturating_sub(rows)
    } else {
        0
    };
    let hscroll = if focused {
        (cursor_col + 1).saturating_sub(text_width)
    } else {
        0
    };
    let source: Vec<&str> = editor.payload.split('\n').collect();
    let highlight = super::publish::looks_like_json(&editor.payload);
    let mut result = Vec::with_capacity(rows);
    for row in 0..rows {
        let index = first + row;
        let prefix = if row == 0 {
            vec![
                Span::styled(
                    if focused { "> " } else { "  " },
                    Style::default().fg(theme::ACCENT),
                ),
                Span::styled(format!("{:<9}", "Payload"), theme::muted()),
            ]
        } else {
            vec![Span::raw(" ".repeat(INDENT))]
        };
        let mut spans = prefix;
        let line = source.get(index).copied().unwrap_or("");
        if highlight && !line.is_empty() && line.chars().count() <= MAX_HIGHLIGHT_CHARS {
            let mut colored = slice_chars(highlight_json_line(line).spans, hscroll, text_width);
            if focused && index == cursor_line {
                colored = mark_cursor(colored, cursor_col - hscroll);
            }
            spans.extend(colored);
            result.push(Line::from(spans));
            continue;
        }
        let content: Vec<char> = line.chars().skip(hscroll).take(text_width).collect();
        let style = if focused {
            theme::bold()
        } else {
            Style::default()
        };
        if focused && index == cursor_line {
            let at = cursor_col - hscroll;
            let before: String = content.iter().take(at).collect();
            let under = content.get(at).copied().unwrap_or(' ');
            let after: String = content.iter().skip(at + 1).collect();
            spans.push(Span::styled(before, style));
            spans.push(Span::styled(
                under.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::styled(after, style));
        } else if index == 0 && editor.payload.is_empty() {
            spans.push(Span::styled("(empty)", theme::muted()));
        } else {
            spans.push(Span::styled(content.into_iter().collect::<String>(), style));
        }
        result.push(Line::from(spans));
    }
    result
}

fn topic_editor_line(editor: &super::publish::PublishEditor, focused: bool) -> Line<'static> {
    let chars: Vec<char> = editor.topic.chars().collect();
    let mut spans = vec![
        Span::styled(
            if focused { "> " } else { "  " },
            Style::default().fg(theme::ACCENT),
        ),
        Span::styled(format!("{:<9}", "Topic"), theme::muted()),
    ];
    let style = if focused {
        theme::bold()
    } else {
        Style::default()
    };
    if focused {
        let at = editor.topic_cursor.min(chars.len());
        spans.push(Span::styled(chars[..at].iter().collect::<String>(), style));
        spans.push(Span::styled(
            chars.get(at).copied().unwrap_or(' ').to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        spans.push(Span::styled(
            chars.iter().skip(at + 1).collect::<String>(),
            style,
        ));
    } else {
        spans.push(Span::styled(chars.iter().collect::<String>(), style));
    }
    Line::from(spans)
}

fn render_confirmation(frame: &mut Frame, state: &UiState, area: Rect) {
    frame.render_widget(Clear, area);
    let message = match &state.confirmation {
        Some(Confirmation::RemoveBroker(id)) => format!(
            "Remove broker {id} and its imported certificates?\nExternal certificate files will not be removed."
        ),
        Some(Confirmation::Quit) => {
            "Quit MATE?\nThe broker connection is closed and the data received so far is discarded."
                .to_owned()
        }
        Some(Confirmation::RemoveCertificate(kind)) => format!(
            "Remove {} configuration? Imported file will be removed; external file will be kept.",
            kind.filename()
        ),
        None => return,
    };
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" CONFIRM ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme::WARN)),
        ),
        area,
    );
    popup_footer(frame, &shortcut_items(state), area);
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_are_abbreviated_and_rounded() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (1_234, "1.2k"),
            (9_949, "9.9k"),
            (9_950, "10k"),
            (15_499, "15k"),
            (999_499, "999k"),
            (999_500, "1M"),
            (1_500_000, "1.5M"),
            (42_000_000, "42M"),
            (2_500_000_000, "2.5G"),
        ];
        for (count, expected) in cases {
            assert_eq!(super::compact_count(count), expected, "{count}");
        }
    }

    use super::*;

    #[test]
    fn truncate_respects_display_width_and_marks_cuts() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("PRESS1_TriggerService", 10), "PRESS1_Tr…");
        assert_eq!(truncate("anything", 0), "");
        assert_eq!(display_width(&truncate("日本語のトピック", 7)), 7);
    }

    #[test]
    fn numbers_are_formatted_for_the_header() {
        assert_eq!(thousands(1284), "1,284");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(format_rate(234.4), "234");
        assert_eq!(format_rate(0.0), "0");
        assert_eq!(format_size(2048), "2.0 KiB");
    }

    #[test]
    fn slicing_scrolls_horizontally_by_display_columns() {
        let spans = vec![Span::raw("abcdef"), Span::styled("ghij", theme::muted())];
        let sliced = slice_spans(spans, 4, 4);
        let text: String = sliced.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "efgh");
    }

    #[test]
    fn json_highlighting_distinguishes_keys_strings_numbers_and_keywords() {
        let line = highlight_json_line(r#"  "a\"b": "x", "n": -1.5e3, "t": true"#);
        let colored: Vec<_> = line
            .spans
            .iter()
            .filter_map(|span| match span.style.fg {
                Some(color) if color != theme::SUBTLE => Some((span.content.as_ref(), color)),
                _ => None,
            })
            .collect();
        assert_eq!(
            colored,
            vec![
                (r#""a\"b""#, Color::Cyan),
                (r#""x""#, Color::Green),
                (r#""n""#, Color::Cyan),
                ("-1.5e3", Color::Yellow),
                (r#""t""#, Color::Cyan),
                ("true", Color::Magenta),
            ]
        );
    }

    #[test]
    fn scrolling_keeps_the_selection_inside_the_window() {
        assert_eq!(scroll_into_view(0, 25, 10, 100), 16);
        assert_eq!(scroll_into_view(20, 5, 10, 100), 5);
        assert_eq!(scroll_into_view(90, 95, 10, 96), 86);
        assert_eq!(scroll_into_view(0, 0, 0, 5), 0);
    }
}
