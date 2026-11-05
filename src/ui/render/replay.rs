//! Drawing of the replay menu.

use mate::replay::Pacing;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{UiState, format_size, panel_block, theme, thousands, truncate};
use crate::ui::replay::{Stage, describe_pacing};

fn stamp(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|time| time.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn hms(milliseconds: f64) -> String {
    let seconds = (milliseconds / 1000.0).max(0.0).round() as u64;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<9}"), theme::muted())
}

pub(super) fn render_replay(frame: &mut Frame, state: &UiState, area: Rect) {
    match state.replay.stage {
        Stage::Files => render_files(frame, state, area),
        Stage::Player => render_player(frame, state, area),
    }
}

fn render_files(frame: &mut Frame, state: &UiState, area: Rect) {
    let replay = &state.replay;
    let block = panel_block("REPLAY \u{b7} choose recordings", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 4 || inner.width < 20 {
        return;
    }
    let width = inner.width as usize;
    let mut lines = vec![
        Line::from(vec![
            label("Folder"),
            Span::raw(truncate(
                &replay.directory.display().to_string(),
                width.saturating_sub(9),
            )),
        ]),
        Line::from(""),
    ];
    let room = (inner.height as usize).saturating_sub(5).max(1);
    let top = (replay.selection + 1).saturating_sub(room);
    for (index, file) in replay.files.iter().enumerate().skip(top).take(room) {
        let marked = replay.marked.contains(&file.path);
        let size = format_size(file.bytes as usize);
        let name_room = width.saturating_sub(14);
        let text = format!(
            "{} {:<name_room$} {size:>9}",
            if marked { "\u{2713}" } else { " " },
            truncate(&file.name, name_room),
        );
        lines.push(Line::from(Span::styled(
            truncate(&text, width),
            if index == replay.selection {
                theme::selection()
            } else if marked {
                Style::default().fg(theme::OK)
            } else {
                Style::default()
            },
        )));
    }
    lines.push(Line::from(""));
    if let Some(prompt) = &replay.path_prompt {
        lines.push(Line::from(vec![
            Span::styled("Folder or file: ", theme::bold().fg(theme::ACCENT)),
            Span::raw(prompt.clone()),
            Span::styled("\u{258c}", Style::default().fg(theme::ACCENT)),
        ]));
    } else if let Some(error) = &replay.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::WARN),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Space marks several files; Enter loads the marked ones (or the highlighted one).",
            theme::muted(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_player(frame: &mut Frame, state: &UiState, area: Rect) {
    let replay = &state.replay;
    let Some(player) = replay.player.as_ref() else {
        return;
    };
    let status = if player.is_playing() {
        "PLAYING"
    } else if player.is_finished() {
        "FINISHED"
    } else {
        "PAUSED"
    };
    let block = panel_block(
        &format!("REPLAY \u{b7} {} \u{b7} {status}", replay.source),
        true,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 6 || inner.width < 30 {
        return;
    }
    let width = inner.width as usize;
    let settings = player.settings();
    let recording = player.recording();
    let (done, total) = player.progress();

    let mut source = format!(
        "{} message{} from {} file{}",
        thousands(recording.messages.len()),
        if recording.messages.len() == 1 {
            ""
        } else {
            "s"
        },
        recording.files,
        if recording.files == 1 { "" } else { "s" },
    );
    if let Some((from, to)) = recording.span() {
        source.push_str(&format!(
            "  \u{b7}  {} \u{2192} {}  ({})",
            stamp(from),
            stamp(to),
            hms((to - from) as f64)
        ));
    }
    let mut lines = vec![Line::from(vec![
        label("Source"),
        Span::raw(truncate(&source, width.saturating_sub(9))),
    ])];
    let mut left_out = Vec::new();
    if player.unsendable() > 0 {
        left_out.push(format!(
            "{} not sendable ($SYS, wildcards, too large)",
            thousands(player.unsendable())
        ));
    }
    if player.filtered_out() > 0 {
        left_out.push(format!(
            "{} outside the filter",
            thousands(player.filtered_out())
        ));
    }
    if recording.skipped_lines > 0 {
        left_out.push(format!(
            "{} lines not in the log format",
            thousands(recording.skipped_lines)
        ));
    }
    if !left_out.is_empty() {
        lines.push(Line::from(vec![
            label(""),
            Span::styled(
                truncate(
                    &format!("left out: {}", left_out.join(", ")),
                    width.saturating_sub(9),
                ),
                theme::muted(),
            ),
        ]));
    }

    let mut target = vec![label("Target")];
    match state.replay_target_name() {
        Some(name) => {
            let id = replay.target.as_deref().unwrap_or_default();
            if let Some(slot) = state.broker_badge(id) {
                target.push(Span::styled(format!("{slot} "), theme::broker_style(slot)));
            }
            target.push(Span::styled(name, theme::bold()));
            target.push(Span::styled("  (b changes)", theme::muted()));
        }
        None => target.push(Span::styled(
            "none: press b to choose a connected broker that allows publishing",
            Style::default().fg(theme::WARN),
        )),
    }
    lines.push(Line::from(target));

    let gaps = match settings.gap_cap_ms() {
        Some(cap) => format!("capped at {} s", cap / 1000),
        None => "kept as recorded".to_owned(),
    };
    let filter = if replay.filter_input.is_some() {
        format!(
            "{}\u{258c}",
            replay.filter_input.as_deref().unwrap_or_default()
        )
    } else if settings.filter.is_empty() {
        "none".to_owned()
    } else {
        settings.filter.clone()
    };
    lines.push(Line::from(vec![
        label("Pacing"),
        Span::raw(describe_pacing(settings)),
        Span::styled("  (m mode, +/- speed)", theme::muted()),
    ]));
    let pause_note = if settings.pacing == Pacing::Timestamps {
        format!("pauses {gaps} (g)   ")
    } else {
        String::new()
    };
    lines.push(Line::from(vec![
        label("Options"),
        Span::raw(format!(
            "{pause_note}loop {} (l)   QoS {} (0/1/2)   filter {filter} (/)",
            if settings.looping { "on" } else { "off" },
            settings.qos
        )),
    ]));
    lines.push(Line::from(""));

    let bar_width = width.saturating_sub(2).clamp(10, 60);
    let filled = if total == 0 {
        0
    } else {
        bar_width * done / total
    };
    let (elapsed_ms, total_ms) = player.timeline_ms();
    lines.push(Line::from(vec![
        Span::styled(
            "\u{2588}".repeat(filled),
            Style::default().fg(theme::ACCENT),
        ),
        Span::styled("\u{2591}".repeat(bar_width - filled), theme::subtle()),
    ]));
    let mut progress = format!(
        "{} / {} messages  \u{b7}  {} / {}",
        thousands(done),
        thousands(total),
        hms(elapsed_ms),
        hms(total_ms)
    );
    if player.loops() > 0 {
        progress.push_str(&format!("  \u{b7}  pass {}", player.loops() + 1));
    }
    lines.push(Line::from(Span::styled(progress, theme::bold())));
    if let Some(next) = player.next_message() {
        lines.push(Line::from(vec![
            Span::styled("next: ", theme::muted()),
            Span::styled(
                truncate(&next.topic, width.saturating_sub(8)),
                Style::default().fg(theme::ACCENT),
            ),
        ]));
    }
    lines.push(Line::from(""));

    if replay.confirming {
        let name = state.replay_target_name().unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!(
                "Send {} message{} to {name}?  [y/Enter] start   [n/Esc] cancel",
                thousands(total),
                if total == 1 { "" } else { "s" }
            ),
            theme::bold().fg(theme::WARN),
        )));
    } else if let Some(error) = &replay.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::WARN),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Space starts and pauses. Messages are sent without the retain flag.",
            theme::muted(),
        )));
    }
    let remaining = (inner.height as usize).saturating_sub(lines.len());
    if remaining > 1 && !replay.recent.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("sent: {} so far", thousands(player.sent() as usize)),
            theme::muted().add_modifier(Modifier::BOLD),
        )));
        for topic in replay
            .recent
            .iter()
            .rev()
            .take(remaining.saturating_sub(1))
            .rev()
        {
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(topic, width.saturating_sub(4))),
                theme::muted(),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
