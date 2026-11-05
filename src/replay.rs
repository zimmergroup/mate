//! Replay of recorded `.mqttlog` files: loading, pacing and the play/pause state machine.
//! Nothing here talks to a broker; the event loop publishes what `Player::due` hands out.

use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    time::Instant,
};

use anyhow::{Context, Result, bail};

use crate::{
    mqtt::MAX_PUBLISH_PAYLOAD,
    mqttlog::{LogFormat, LogRecord},
    topics::topic_matches_filter,
};

/// More messages than this are refused; they are held in memory.
pub const MAX_MESSAGES: usize = 2_000_000;

pub const SPEEDS: [f64; 9] = [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 50.0, 100.0];
pub const RATES: [f64; 8] = [1.0, 2.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0];
/// Longest pause kept between two messages when gaps are capped, in milliseconds.
pub const GAP_CAPS: [Option<u64>; 4] = [None, Some(1_000), Some(5_000), Some(30_000)];

#[derive(Debug, Default)]
pub struct Recording {
    pub files: usize,
    pub messages: Vec<LogRecord>,
    /// Lines that are not messages (blank lines, foreign formats).
    pub skipped_lines: usize,
}

impl Recording {
    /// Reads the files in order; messages are sorted by time (stable, so equal times keep file order).
    pub fn load(paths: &[PathBuf], format: &LogFormat) -> Result<Self> {
        let mut recording = Self::default();
        for path in paths {
            let file = fs::File::open(path)
                .with_context(|| format!("could not open {}", path.display()))?;
            let mut reader = BufReader::new(file);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                let read = reader
                    .read_until(b'\n', &mut buffer)
                    .with_context(|| format!("could not read {}", path.display()))?;
                if read == 0 {
                    break;
                }
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    continue;
                }
                match format.parse_line(line) {
                    Some(record) => {
                        if recording.messages.len() >= MAX_MESSAGES {
                            bail!("more than {MAX_MESSAGES} messages; pick fewer or smaller files");
                        }
                        recording.messages.push(record);
                    }
                    None => recording.skipped_lines += 1,
                }
            }
            recording.files += 1;
        }
        recording.messages.sort_by_key(|record| record.millis);
        Ok(recording)
    }

    /// First and last timestamp, as milliseconds on the log's own clock.
    pub fn span(&self) -> Option<(i64, i64)> {
        Some((self.messages.first()?.millis, self.messages.last()?.millis))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pacing {
    /// The recorded time between messages, scaled by `speed`.
    Timestamps,
    /// A steady number of messages per second, whatever the recording says.
    FixedRate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplaySettings {
    pub pacing: Pacing,
    /// Index into `SPEEDS`.
    pub speed: usize,
    /// Index into `RATES`.
    pub rate: usize,
    /// Index into `GAP_CAPS`.
    pub gap_cap: usize,
    pub looping: bool,
    pub qos: u8,
    /// MQTT topic filter; empty replays everything.
    pub filter: String,
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self {
            pacing: Pacing::Timestamps,
            speed: 3,
            rate: 3,
            gap_cap: 2,
            looping: false,
            qos: 0,
            filter: String::new(),
        }
    }
}

impl ReplaySettings {
    pub fn speed_factor(&self) -> f64 {
        SPEEDS[self.speed.min(SPEEDS.len() - 1)]
    }

    pub fn rate_per_second(&self) -> f64 {
        RATES[self.rate.min(RATES.len() - 1)]
    }

    pub fn gap_cap_ms(&self) -> Option<u64> {
        GAP_CAPS[self.gap_cap.min(GAP_CAPS.len() - 1)]
    }
}

/// Messages a broker would refuse anyway (`$` topics, wildcards, oversized payloads).
fn sendable(record: &LogRecord) -> bool {
    !record.topic.is_empty()
        && record.topic.len() <= 65_535
        && !record.topic.contains(['+', '#', '\0'])
        && !record.topic.starts_with('$')
        && record.payload.len() <= MAX_PUBLISH_PAYLOAD
}

pub struct Player {
    recording: Recording,
    settings: ReplaySettings,
    /// Indexes into the recording of the messages to send, in order.
    selected: Vec<usize>,
    /// When each selected message is due, in milliseconds at speed 1.
    schedule: Vec<f64>,
    unsendable: usize,
    filtered_out: usize,
    cursor: usize,
    clock_ms: f64,
    last_tick: Option<Instant>,
    playing: bool,
    finished: bool,
    sent: u64,
    loops: u64,
}

impl Player {
    pub fn new(recording: Recording, settings: ReplaySettings) -> Self {
        let mut player = Self {
            recording,
            settings,
            selected: Vec::new(),
            schedule: Vec::new(),
            unsendable: 0,
            filtered_out: 0,
            cursor: 0,
            clock_ms: 0.0,
            last_tick: None,
            playing: false,
            finished: false,
            sent: 0,
            loops: 0,
        };
        player.rebuild();
        player
    }

    pub fn settings(&self) -> &ReplaySettings {
        &self.settings
    }

    pub fn recording(&self) -> &Recording {
        &self.recording
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn loops(&self) -> u64 {
        self.loops
    }

    /// Messages of the recording that a broker would refuse; they are never sent.
    pub fn unsendable(&self) -> usize {
        self.unsendable
    }

    /// Messages left out by the topic filter.
    pub fn filtered_out(&self) -> usize {
        self.filtered_out
    }

    /// Messages in one pass and the number already handed out in this pass.
    pub fn progress(&self) -> (usize, usize) {
        (self.cursor, self.selected.len())
    }

    /// The message that goes next.
    pub fn next_message(&self) -> Option<&LogRecord> {
        self.selected
            .get(self.cursor)
            .map(|index| &self.recording.messages[*index])
    }

    /// Time of one pass in milliseconds at the current pacing and speed, and how much of it is done.
    pub fn timeline_ms(&self) -> (f64, f64) {
        let speed = self.speed();
        let total = self.schedule.last().copied().unwrap_or(0.0) / speed;
        (self.clock_ms.min(total * speed) / speed, total)
    }

    fn speed(&self) -> f64 {
        match self.settings.pacing {
            Pacing::Timestamps => self.settings.speed_factor(),
            Pacing::FixedRate => 1.0,
        }
    }

    fn rebuild(&mut self) {
        let anchor = self.selected.get(self.cursor).copied();
        self.selected.clear();
        self.schedule.clear();
        self.unsendable = 0;
        self.filtered_out = 0;
        let filter = self.settings.filter.trim();
        let mut time = 0.0;
        let mut previous: Option<i64> = None;
        let cap = self.settings.gap_cap_ms().map(|cap| cap as f64);
        for (index, record) in self.recording.messages.iter().enumerate() {
            if !sendable(record) {
                self.unsendable += 1;
                continue;
            }
            if !filter.is_empty() && !topic_matches_filter(filter, &record.topic) {
                self.filtered_out += 1;
                continue;
            }
            match self.settings.pacing {
                Pacing::Timestamps => {
                    if let Some(previous) = previous {
                        let gap = (record.millis - previous).max(0) as f64;
                        time += cap.map_or(gap, |cap| gap.min(cap));
                    }
                    previous = Some(record.millis);
                }
                Pacing::FixedRate => {
                    time = self.selected.len() as f64 * 1000.0 / self.settings.rate_per_second();
                }
            }
            self.selected.push(index);
            self.schedule.push(time);
        }
        self.cursor = anchor.map_or(0, |anchor| self.selected.partition_point(|i| *i < anchor));
        self.clock_ms = if self.cursor == 0 {
            0.0
        } else {
            self.schedule.get(self.cursor).copied().unwrap_or(time)
        };
        if self.cursor < self.selected.len() {
            self.finished = false;
        }
    }

    /// Applies new settings; the position stays at the same message where it still exists.
    pub fn configure(&mut self, settings: ReplaySettings) {
        let rebuild = settings.pacing != self.settings.pacing
            || settings.gap_cap != self.settings.gap_cap
            || settings.filter != self.settings.filter
            || (settings.pacing == Pacing::FixedRate && settings.rate != self.settings.rate);
        self.settings = settings;
        if rebuild {
            self.rebuild();
        }
    }

    pub fn play(&mut self, now: Instant) {
        if self.finished {
            self.restart();
        }
        if !self.selected.is_empty() {
            self.playing = true;
            self.last_tick = Some(now);
        }
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.last_tick = None;
    }

    pub fn restart(&mut self) {
        self.cursor = 0;
        self.clock_ms = 0.0;
        self.finished = false;
        self.sent = 0;
        self.loops = 0;
        if self.playing {
            self.last_tick = None;
        }
    }

    /// The messages that are due by `now`, at most `max`. Counted as sent; `unsend` takes some back.
    pub fn due(&mut self, now: Instant, max: usize) -> Vec<&LogRecord> {
        if !self.playing {
            return Vec::new();
        }
        let elapsed = self
            .last_tick
            .map_or(0.0, |last| now.duration_since(last).as_secs_f64() * 1000.0);
        self.last_tick = Some(now);
        self.clock_ms += elapsed * self.speed();
        let mut indexes = Vec::new();
        while indexes.len() < max {
            if self.cursor >= self.selected.len() {
                if self.settings.looping && !self.selected.is_empty() {
                    self.cursor = 0;
                    self.clock_ms = 0.0;
                    self.loops += 1;
                    // The next pass starts with its first message on the following call.
                    break;
                }
                self.playing = false;
                self.finished = true;
                self.last_tick = None;
                break;
            }
            if self.schedule[self.cursor] > self.clock_ms {
                break;
            }
            indexes.push(self.selected[self.cursor]);
            self.cursor += 1;
        }
        self.sent += indexes.len() as u64;
        indexes
            .into_iter()
            .map(|index| &self.recording.messages[index])
            .collect()
    }

    /// Puts the last `count` handed-out messages back, for example when the connection was too busy.
    pub fn unsend(&mut self, count: usize) {
        let count = count.min(self.cursor);
        self.cursor -= count;
        self.sent = self.sent.saturating_sub(count as u64);
        self.finished = false;
        if let Some(time) = self.schedule.get(self.cursor) {
            self.clock_ms = self.clock_ms.min(*time);
        }
        self.playing = true;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn record(millis: i64, topic: &str) -> LogRecord {
        LogRecord {
            millis,
            topic: topic.to_owned(),
            payload: b"x".to_vec(),
            user_properties: Vec::new(),
        }
    }

    fn recording(times: &[(i64, &str)]) -> Recording {
        Recording {
            files: 1,
            messages: times.iter().map(|(t, topic)| record(*t, topic)).collect(),
            skipped_lines: 0,
        }
    }

    fn topics(batch: Vec<&LogRecord>) -> Vec<String> {
        batch
            .into_iter()
            .map(|record| record.topic.clone())
            .collect()
    }

    fn settings(speed: f64) -> ReplaySettings {
        ReplaySettings {
            speed: SPEEDS.iter().position(|s| *s == speed).unwrap(),
            gap_cap: 0,
            ..ReplaySettings::default()
        }
    }

    #[test]
    fn messages_come_out_at_their_recorded_times() {
        let mut player = Player::new(
            recording(&[(1000, "a"), (1500, "b"), (4000, "c")]),
            settings(1.0),
        );
        let start = Instant::now();
        assert!(player.due(start, 10).is_empty(), "paused until play");
        player.play(start);
        assert_eq!(topics(player.due(start, 10)), ["a"]);
        assert!(
            player
                .due(start + Duration::from_millis(400), 10)
                .is_empty()
        );
        assert_eq!(
            topics(player.due(start + Duration::from_millis(500), 10)),
            ["b"]
        );
        assert!(
            player
                .due(start + Duration::from_millis(2900), 10)
                .is_empty()
        );
        assert_eq!(
            topics(player.due(start + Duration::from_millis(3000), 10)),
            ["c"]
        );
        player.due(start + Duration::from_secs(9), 10);
        assert!(player.is_finished() && !player.is_playing());
        assert_eq!(player.sent(), 3);
    }

    #[test]
    fn speed_scales_the_time_and_pause_stops_the_clock() {
        let mut player = Player::new(recording(&[(0, "a"), (10_000, "b")]), settings(10.0));
        let start = Instant::now();
        player.play(start);
        assert_eq!(topics(player.due(start, 10)), ["a"]);
        assert!(
            player
                .due(start + Duration::from_millis(900), 10)
                .is_empty()
        );

        player.pause();
        assert!(player.due(start + Duration::from_secs(60), 10).is_empty());
        let resumed = start + Duration::from_secs(120);
        player.play(resumed);
        assert!(
            player
                .due(resumed + Duration::from_millis(50), 10)
                .is_empty()
        );
        assert_eq!(
            topics(player.due(resumed + Duration::from_millis(100), 10)),
            ["b"]
        );

        // A slower speed stretches the gap.
        let mut slow = Player::new(recording(&[(0, "a"), (1_000, "b")]), settings(0.5));
        slow.play(start);
        slow.due(start, 10);
        assert!(slow.due(start + Duration::from_millis(1900), 10).is_empty());
        assert_eq!(
            topics(slow.due(start + Duration::from_millis(2000), 10)),
            ["b"]
        );
    }

    #[test]
    fn long_gaps_can_be_capped() {
        let mut capped = settings(1.0);
        capped.gap_cap = 1;
        let mut player = Player::new(recording(&[(0, "a"), (3_600_000, "b")]), capped);
        let start = Instant::now();
        player.play(start);
        player.due(start, 10);
        assert_eq!(
            topics(player.due(start + Duration::from_millis(1000), 10)),
            ["b"]
        );
        assert_eq!(player.timeline_ms().1, 1000.0);
    }

    #[test]
    fn a_fixed_rate_ignores_the_timestamps() {
        let mut fixed = ReplaySettings {
            pacing: Pacing::FixedRate,
            rate: RATES.iter().position(|r| *r == 10.0).unwrap(),
            ..ReplaySettings::default()
        };
        let mut player = Player::new(
            recording(&[(0, "a"), (60_000, "b"), (120_000, "c")]),
            fixed.clone(),
        );
        let start = Instant::now();
        player.play(start);
        assert_eq!(topics(player.due(start, 10)), ["a"]);
        assert_eq!(
            topics(player.due(start + Duration::from_millis(100), 10)),
            ["b"]
        );
        assert_eq!(
            topics(player.due(start + Duration::from_millis(200), 10)),
            ["c"]
        );

        fixed.rate = RATES.iter().position(|r| *r == 1000.0).unwrap();
        player.restart();
        player.configure(fixed);
        player.play(start);
        let all = player.due(start + Duration::from_millis(10), 10);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn the_filter_and_unsendable_topics_are_left_out_and_counted() {
        let mut filtered = settings(1.0);
        filtered.filter = "plant/#".to_owned();
        let player = Player::new(
            recording(&[
                (0, "plant/a"),
                (1, "other/b"),
                (2, "$SYS/broker/uptime"),
                (3, "plant/c"),
            ]),
            filtered,
        );
        assert_eq!(player.progress(), (0, 2));
        assert_eq!((player.filtered_out(), player.unsendable()), (1, 1));
        assert_eq!(player.next_message().unwrap().topic, "plant/a");
    }

    #[test]
    fn looping_restarts_and_unsend_retries() {
        let mut looping = settings(100.0);
        looping.looping = true;
        let mut player = Player::new(recording(&[(0, "a"), (100, "b")]), looping);
        let start = Instant::now();
        player.play(start);
        let mut seen = Vec::new();
        for step in 0..20 {
            seen.extend(topics(
                player.due(start + Duration::from_millis(step * 10), 10),
            ));
        }
        assert!(seen.len() >= 6 && player.loops() >= 2, "{seen:?}");
        assert_eq!(&seen[..4], ["a", "b", "a", "b"]);

        let mut single = Player::new(recording(&[(0, "a"), (0, "b"), (0, "c")]), settings(1.0));
        single.play(start);
        let batch = single.due(start, 10);
        assert_eq!(batch.len(), 3);
        single.unsend(2);
        assert_eq!(single.sent(), 1);
        assert_eq!(topics(single.due(start, 10)), ["b", "c"]);
    }

    #[test]
    fn changing_settings_keeps_the_position() {
        let mut player = Player::new(
            recording(&[(0, "a/1"), (10, "b/2"), (20, "a/3"), (30, "b/4")]),
            settings(100.0),
        );
        let start = Instant::now();
        player.play(start);
        assert_eq!(
            topics(player.due(start + Duration::from_millis(50), 2)),
            ["a/1", "b/2"]
        );
        let mut next = player.settings().clone();
        next.filter = "b/#".to_owned();
        player.configure(next);
        assert_eq!(player.next_message().unwrap().topic, "b/4");
        assert_eq!(player.progress(), (1, 2));
    }

    #[test]
    fn recordings_load_sorted_and_count_foreign_lines() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("a.mqttlog");
        let second = directory.path().join("b.mqttlog");
        std::fs::write(
            &first,
            "2026-10-23 10:00:02,000 | T0  I: late [] 2\nnot a log line\n\n",
        )
        .unwrap();
        std::fs::write(
            &second,
            "2026-10-23 10:00:01,000 | T0  I: early [{\"k\":\"v\"}] 1\r\n",
        )
        .unwrap();
        let recording = Recording::load(&[first, second], &LogFormat::default()).unwrap();
        assert_eq!(recording.files, 2);
        assert_eq!(recording.skipped_lines, 1);
        let order: Vec<&str> = recording
            .messages
            .iter()
            .map(|m| m.topic.as_str())
            .collect();
        assert_eq!(order, ["early", "late"]);
        assert_eq!(
            recording.messages[0].user_properties,
            [("k".into(), "v".into())]
        );
        let (from, to) = recording.span().unwrap();
        assert_eq!(to - from, 1000);
        assert!(
            Recording::load(
                &[directory.path().join("missing.mqttlog")],
                &LogFormat::default()
            )
            .is_err()
        );
    }
}
