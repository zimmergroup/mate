//! Drawing of the stream view.

use mate::stream::{StreamEntry, StreamKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{
    UiState, clock, fit_left, highlight_json_line, line_width, panel_block, restyle, slice_spans,
    theme, thousands, truncate,
};
use crate::ui::stream::{DetailLine, list_preview};

const INDENT: &str = "    ";

fn event_or_message_line(
    entry: &StreamEntry,
    expanded: bool,
    selected: bool,
    width: usize,
    broker_slot: Option<usize>,
) -> Line<'static> {
    let marker = match (&entry.kind, expanded) {
        (StreamKind::Message(_), true) => "\u{25be} ",
        (StreamKind::Message(_), false) => "\u{25b8} ",
        (StreamKind::Event { .. }, _) => "  ",
    };
    let mut spans = vec![
        Span::styled(marker, theme::muted()),
        Span::styled(format!("{} ", clock(entry.wall)), theme::muted()),
    ];
    match &entry.kind {
        StreamKind::Message(message) => {
            let flags = format!(
                "Q{}{}{} ",
                message.qos,
                if message.retain { 'R' } else { ' ' },
                if message.dup { 'D' } else { ' ' }
            );
            let flag_style = if message.retain {
                Style::default().fg(theme::WARN)
            } else {
                theme::muted()
            };
            spans.push(Span::styled(flags, flag_style));
            if let Some(slot) = broker_slot {
                spans.push(Span::styled(format!("{slot} "), theme::broker_style(slot)));
            }
            let rest = width.saturating_sub(line_width(&spans));
            let topic_width = (rest * 40 / 100).clamp(12, 48).min(rest);
            spans.push(Span::styled(
                format!("{} ", fit_left(&message.topic, topic_width)),
                Style::default().fg(theme::ACCENT),
            ));
            spans.push(Span::raw(list_preview(message)));
        }
        StreamKind::Event { text, error } => {
            let style = if *error {
                theme::bold().fg(theme::ERROR)
            } else {
                theme::bold().fg(theme::WARN)
            };
            spans.push(Span::styled(format!("\u{25cf} {text}"), style));
        }
    }
    let mut spans = slice_spans(spans, 0, width);
    if selected {
        let used = line_width(&spans);
        spans = restyle(spans, theme::selection());
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), theme::selection()));
        }
    }
    Line::from(spans)
}

fn detail_line(line: &DetailLine, width: usize) -> Line<'static> {
    let room = width.saturating_sub(INDENT.len());
    match line {
        DetailLine::Info(text) => Line::from(Span::styled(
            format!("{INDENT}{}", truncate(text, room)),
            theme::muted(),
        )),
        DetailLine::Property(text) => Line::from(vec![
            Span::raw(INDENT),
            Span::styled("\u{2022} ", theme::muted()),
            Span::styled(
                truncate(text, room.saturating_sub(2)),
                Style::default().fg(theme::ACCENT),
            ),
        ]),
        DetailLine::Json(text) => {
            let mut spans = vec![Span::raw(INDENT)];
            spans.extend(slice_spans(highlight_json_line(text).spans, 0, room));
            Line::from(spans)
        }
        DetailLine::Text(text) => {
            Line::from(Span::raw(format!("{INDENT}{}", truncate(text, room))))
        }
        DetailLine::Note(text) => Line::from(Span::styled(
            format!("{INDENT}{}", truncate(text, room)),
            theme::muted(),
        )),
    }
}

pub(super) fn render_stream(frame: &mut Frame, state: &mut UiState, area: Rect) {
    let view = &state.stream_view;
    let mut title = "STREAM".to_owned();
    if view.paused {
        title.push_str(" \u{b7} PAUSED");
    } else if view.follow {
        title.push_str(" \u{b7} FOLLOW");
    }
    let block = panel_block(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // One line for the active filters above the list and one footer line below it.
    if inner.height < 3 || inner.width < 20 {
        return;
    }
    let width = inner.width as usize;
    let list_height = inner.height as usize - 2;
    state.stream_view.viewport = list_height;
    let (view, log, slots) = (&mut state.stream_view, &state.stream, &state.broker_slots);
    view.fit_window(log, list_height);

    let caret = |field| {
        if view.typing == Some(field) {
            "\u{258c}"
        } else {
            ""
        }
    };
    let mut filters = Vec::new();
    if !view.topic_filter.is_empty() || view.typing == Some(crate::ui::stream::Field::Topic) {
        filters.push(format!(
            "topic /{}{}",
            view.topic_filter,
            caret(crate::ui::stream::Field::Topic)
        ));
    }
    if !view.text_filter.is_empty() || view.typing == Some(crate::ui::stream::Field::Text) {
        filters.push(format!(
            "find {}{}",
            view.text_filter,
            caret(crate::ui::stream::Field::Text)
        ));
    }
    if view.retained_only {
        filters.push("retained only".to_owned());
    }
    if !view.show_events {
        filters.push("events hidden".to_owned());
    }
    let note = if filters.is_empty() {
        Line::from(Span::styled(
            truncate(
                "Newest last. / topic  f find in payload  R retained only  Enter details  Space pause",
                width,
            ),
            theme::muted(),
        ))
    } else {
        Line::from(Span::styled(
            truncate(&filters.join("  \u{b7}  "), width),
            theme::bold().fg(theme::WARN),
        ))
    };

    let mut lines = Vec::with_capacity(list_height);
    let shown = view.shown().len();
    if shown == 0 {
        let text = if log.is_empty() {
            "No messages yet."
        } else {
            "Nothing matches the filters."
        };
        lines.push(Line::from(Span::styled(text, theme::muted())));
    }
    let ids: Vec<u64> = view.shown().iter().skip(view.top).copied().collect();
    for (offset, id) in ids.into_iter().enumerate() {
        if lines.len() >= list_height {
            break;
        }
        let Some(entry) = log.get(id) else {
            continue;
        };
        let expanded = view.expanded.contains(&id);
        let selected = view.top + offset == view.selection;
        let broker_slot = match &entry.kind {
            StreamKind::Message(message) if slots.len() > 1 => message
                .broker
                .as_deref()
                .and_then(|id| slots.iter().position(|known| known == id))
                .map(|index| index + 1),
            _ => None,
        };
        lines.push(event_or_message_line(
            entry,
            expanded,
            selected,
            width,
            broker_slot,
        ));
        if expanded {
            for detail in view.details(log, id).to_vec() {
                if lines.len() >= list_height {
                    break;
                }
                lines.push(detail_line(&detail, width));
            }
        }
    }

    let mut footer = format!("{} of {} entries", thousands(shown), thousands(log.len()));
    if log.dropped() > 0 {
        footer.push_str(&format!(
            " \u{b7} {} older dropped",
            thousands(log.dropped() as usize)
        ));
    }
    if view.paused && view.waiting() > 0 {
        footer.push_str(&format!(
            " \u{b7} +{} waiting (Space)",
            thousands(view.waiting())
        ));
    } else if !view.follow && !view.paused {
        footer.push_str(" \u{b7} End follows the newest");
    }

    frame.render_widget(
        Paragraph::new(note),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y + 1, inner.width, list_height as u16),
    );
    frame.render_widget(
        Paragraph::new(Span::styled(truncate(&footer, width), theme::muted())),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}
