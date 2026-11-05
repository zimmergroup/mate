//! The replay menu (`R`): pick recorded `.mqttlog` files, then play them back to a connected broker.

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use crossterm::event::KeyCode;
use mate::{
    mqtt::{PublishRequest, QueueFull},
    mqttlog::FILE_EXTENSION,
    replay::{GAP_CAPS, Pacing, Player, RATES, Recording, ReplaySettings, SPEEDS},
    topics::valid_topic_filter,
};

use super::{UiAction, UiState, View};

/// Most messages handed to the connection in one pass of the event loop.
const MAX_PER_TICK: usize = 500;
/// A connection that stays too busy for this many passes in a row pauses the replay.
const MAX_STALLS: u32 = 400;
const RECENT_LINES: usize = 12;
/// Larger selections are refused; the recording is held in memory.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Files,
    Player,
}

#[derive(Debug, Clone)]
pub struct ReplayFile {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
}

#[derive(Default)]
pub struct ReplayView {
    pub stage: Stage,
    pub directory: PathBuf,
    pub files: Vec<ReplayFile>,
    pub selection: usize,
    pub marked: BTreeSet<PathBuf>,
    /// Folder or file path being typed after `p`.
    pub path_prompt: Option<String>,
    pub player: Option<Player>,
    /// What is being replayed, for the title line.
    pub source: String,
    /// ID of the broker the messages go to.
    pub target: Option<String>,
    /// Asking whether to start; nothing is sent before the answer.
    pub confirming: bool,
    /// The user has confirmed sending to `target`.
    pub armed: bool,
    /// Topic filter being typed.
    pub filter_input: Option<String>,
    pub settings: ReplaySettings,
    pub recent: VecDeque<String>,
    pub error: Option<String>,
    stalls: u32,
}

impl ReplayView {
    pub fn is_playing(&self) -> bool {
        self.player.as_ref().is_some_and(Player::is_playing)
    }

    pub fn typing(&self) -> bool {
        self.path_prompt.is_some() || self.filter_input.is_some()
    }
}

fn list_files(directory: &Path) -> std::io::Result<Vec<ReplayFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(FILE_EXTENSION) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        files.push(ReplayFile {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            bytes: metadata.len(),
        });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

fn expand_home(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(dirs) = directories::BaseDirs::new()
    {
        return dirs.home_dir().join(rest);
    }
    PathBuf::from(text)
}

impl UiState {
    pub(super) fn open_replay(&mut self) {
        if self.replay.directory.as_os_str().is_empty() {
            self.replay.directory = self.log.resolved_directory(self.broker_store.root());
        }
        if self.replay.stage == Stage::Files {
            self.refresh_replay_files();
        }
        self.view = View::Replay;
    }

    fn refresh_replay_files(&mut self) {
        match list_files(&self.replay.directory) {
            Ok(files) => {
                self.replay.error = files.is_empty().then(|| {
                    format!(
                        "No .mqttlog files in {}. Press p to type another folder or file.",
                        self.replay.directory.display()
                    )
                });
                self.replay.files = files;
            }
            Err(error) => {
                self.replay.files.clear();
                self.replay.error = Some(format!(
                    "Cannot read {}: {error}. Press p to type another folder or file.",
                    self.replay.directory.display()
                ));
            }
        }
        self.replay
            .marked
            .retain(|path| self.replay.files.iter().any(|file| &file.path == path));
        self.replay.selection = self
            .replay
            .selection
            .min(self.replay.files.len().saturating_sub(1));
    }

    /// The text for the header while a replay runs.
    pub fn replay_status(&self) -> Option<String> {
        let player = self
            .replay
            .player
            .as_ref()
            .filter(|player| player.is_playing())?;
        let (done, total) = player.progress();
        Some(format!("REPLAY \u{25b6} {done}/{total}"))
    }

    /// Whether the event loop should poll quickly so replay timing stays accurate.
    pub fn replay_active(&self) -> bool {
        self.replay.is_playing()
    }

    fn load_replay(&mut self, paths: Vec<PathBuf>) {
        let total: u64 = paths
            .iter()
            .filter_map(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum();
        if total > MAX_TOTAL_BYTES {
            self.replay.error =
                Some("The selected files are larger than 512 MB; pick fewer.".to_owned());
            return;
        }
        let recording = match Recording::load(&paths, &self.log_format) {
            Ok(recording) => recording,
            Err(error) => {
                self.replay.error = Some(format!("{error:#}"));
                return;
            }
        };
        if recording.messages.is_empty() {
            self.replay.error = Some(format!(
                "No messages found ({} lines are not in the .mqttlog format).",
                recording.skipped_lines
            ));
            return;
        }
        self.replay.source = match paths.as_slice() {
            [single] => single
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            many => format!("{} files", many.len()),
        };
        self.replay.player = Some(Player::new(recording, self.replay.settings.clone()));
        self.replay.target = self.default_publish_broker().filter(|id| {
            self.publishable_brokers()
                .any(|connection| connection.broker_id == *id)
        });
        self.replay.armed = false;
        self.replay.confirming = false;
        self.replay.recent.clear();
        self.replay.error = None;
        self.replay.stalls = 0;
        self.replay.stage = Stage::Player;
    }

    fn stop_replay(&mut self) {
        if let Some(player) = self.replay.player.as_mut() {
            player.pause();
        }
        self.replay.confirming = false;
    }

    fn apply_replay_settings(&mut self, change: impl FnOnce(&mut ReplaySettings)) {
        change(&mut self.replay.settings);
        if let Some(player) = self.replay.player.as_mut() {
            player.configure(self.replay.settings.clone());
        }
    }

    fn cycle_replay_target(&mut self) {
        let ids: Vec<String> = self
            .publishable_brokers()
            .map(|connection| connection.broker_id.clone())
            .collect();
        if ids.is_empty() {
            self.replay.error = Some(
                "No connected broker allows publishing: connect one (b) and allow publishing (w)."
                    .to_owned(),
            );
            return;
        }
        let next = self
            .replay
            .target
            .as_ref()
            .and_then(|current| ids.iter().position(|id| id == current))
            .map_or(0, |index| (index + 1) % ids.len());
        self.replay.target = Some(ids[next].clone());
        self.replay.armed = false;
        self.replay.error = None;
        self.stop_replay();
    }

    /// Space: asks once per target, then plays and pauses.
    fn toggle_replay(&mut self) {
        let Some(player) = self.replay.player.as_mut() else {
            return;
        };
        if player.is_playing() {
            player.pause();
            return;
        }
        let target_ok = self.replay.target.as_ref().is_some_and(|id| {
            self.publishable_brokers()
                .any(|connection| connection.broker_id == *id)
        });
        if !target_ok {
            self.replay.target = None;
            self.replay.error =
                Some("Choose a connected broker that allows publishing with b first.".to_owned());
            return;
        }
        self.replay.error = None;
        if self.replay.armed {
            self.start_replay();
        } else {
            self.replay.confirming = true;
        }
    }

    fn start_replay(&mut self) {
        let now = Instant::now();
        if let Some(player) = self.replay.player.as_mut() {
            player.play(now);
        }
        self.replay.confirming = false;
        self.replay.armed = true;
        self.replay.stalls = 0;
    }

    pub(super) fn handle_replay_key(&mut self, code: KeyCode) -> UiAction {
        match self.replay.stage {
            Stage::Files => self.handle_replay_files_key(code),
            Stage::Player => self.handle_replay_player_key(code),
        }
    }

    fn handle_replay_files_key(&mut self, code: KeyCode) -> UiAction {
        if let Some(mut prompt) = self.replay.path_prompt.take() {
            match code {
                KeyCode::Esc => return UiAction::None,
                KeyCode::Enter => {
                    let path = expand_home(prompt.trim());
                    if path.is_dir() {
                        self.replay.directory = path;
                        self.replay.selection = 0;
                        self.replay.marked.clear();
                        self.refresh_replay_files();
                    } else if path.is_file() {
                        self.load_replay(vec![path]);
                    } else {
                        self.replay.error = Some(format!("{} does not exist", path.display()));
                    }
                    return UiAction::None;
                }
                KeyCode::Backspace => {
                    prompt.pop();
                }
                KeyCode::Char(character) => prompt.push(character),
                _ => {}
            }
            self.replay.path_prompt = Some(prompt);
            return UiAction::None;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('R') => self.view = View::Main,
            KeyCode::Char('q') => return UiAction::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.replay.selection =
                    (self.replay.selection + 1).min(self.replay.files.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.replay.selection = self.replay.selection.saturating_sub(1);
            }
            KeyCode::Char(' ') => {
                if let Some(file) = self.replay.files.get(self.replay.selection) {
                    let path = file.path.clone();
                    if !self.replay.marked.remove(&path) {
                        self.replay.marked.insert(path);
                    }
                }
            }
            KeyCode::Char('p') => {
                self.replay.path_prompt = Some(self.replay.directory.display().to_string());
            }
            KeyCode::Char('r') => self.refresh_replay_files(),
            KeyCode::Char('a') => {
                self.replay.marked = self
                    .replay
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect();
            }
            KeyCode::Enter => {
                let paths: Vec<PathBuf> = if self.replay.marked.is_empty() {
                    self.replay
                        .files
                        .get(self.replay.selection)
                        .map(|file| file.path.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.replay.marked.iter().cloned().collect()
                };
                if paths.is_empty() {
                    self.replay.error = Some("No file selected".to_owned());
                } else {
                    self.load_replay(paths);
                }
            }
            _ => {}
        }
        UiAction::None
    }

    fn handle_replay_player_key(&mut self, code: KeyCode) -> UiAction {
        if let Some(mut input) = self.replay.filter_input.take() {
            match code {
                KeyCode::Esc => return UiAction::None,
                KeyCode::Enter => {
                    let filter = input.trim().to_owned();
                    if filter.is_empty() || valid_topic_filter(&filter) {
                        self.apply_replay_settings(|settings| settings.filter = filter);
                        self.replay.error = None;
                    } else {
                        self.replay.error = Some(format!("'{filter}' is not a valid topic filter"));
                    }
                    return UiAction::None;
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(character) => input.push(character),
                _ => {}
            }
            self.replay.filter_input = Some(input);
            return UiAction::None;
        }
        if self.replay.confirming {
            match code {
                KeyCode::Char('y') | KeyCode::Enter => self.start_replay(),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char(' ') => {
                    self.replay.confirming = false;
                }
                _ => {}
            }
            return UiAction::None;
        }
        let pacing = self.replay.settings.pacing;
        match code {
            KeyCode::Esc | KeyCode::Char('f') => {
                self.stop_replay();
                self.replay.stage = Stage::Files;
                self.refresh_replay_files();
            }
            KeyCode::Char('q') => return UiAction::Quit,
            KeyCode::Char(' ') => self.toggle_replay(),
            KeyCode::Char('b') => self.cycle_replay_target(),
            KeyCode::Char('m') => self.apply_replay_settings(|settings| {
                settings.pacing = match settings.pacing {
                    Pacing::Timestamps => Pacing::FixedRate,
                    Pacing::FixedRate => Pacing::Timestamps,
                };
            }),
            KeyCode::Char('+' | '=') => self.apply_replay_settings(|settings| match pacing {
                Pacing::Timestamps => settings.speed = (settings.speed + 1).min(SPEEDS.len() - 1),
                Pacing::FixedRate => settings.rate = (settings.rate + 1).min(RATES.len() - 1),
            }),
            KeyCode::Char('-') => self.apply_replay_settings(|settings| match pacing {
                Pacing::Timestamps => settings.speed = settings.speed.saturating_sub(1),
                Pacing::FixedRate => settings.rate = settings.rate.saturating_sub(1),
            }),
            KeyCode::Char('g') => self.apply_replay_settings(|settings| {
                settings.gap_cap = (settings.gap_cap + 1) % GAP_CAPS.len();
            }),
            KeyCode::Char('l') => {
                self.apply_replay_settings(|settings| settings.looping = !settings.looping);
            }
            KeyCode::Char(digit @ '0'..='2') => {
                let qos = digit as u8 - b'0';
                self.apply_replay_settings(|settings| settings.qos = qos);
            }
            KeyCode::Char('/') => {
                self.replay.filter_input = Some(self.replay.settings.filter.clone());
            }
            KeyCode::Char('r') => {
                if let Some(player) = self.replay.player.as_mut() {
                    player.restart();
                }
                self.replay.recent.clear();
            }
            _ => {}
        }
        UiAction::None
    }

    /// The messages due now and the broker to publish them to; counted as sent until reported otherwise.
    pub fn replay_batch(&mut self, now: Instant) -> Option<(String, Vec<PublishRequest>)> {
        let target = self.replay.target.clone()?;
        let qos = self.replay.settings.qos;
        let player = self
            .replay
            .player
            .as_mut()
            .filter(|player| player.is_playing())?;
        let requests: Vec<PublishRequest> = player
            .due(now, MAX_PER_TICK)
            .into_iter()
            .map(|record| PublishRequest {
                topic: record.topic.clone(),
                payload: record.payload.clone(),
                qos,
                retain: false,
                user_properties: record.user_properties.clone(),
            })
            .collect();
        self.dirty = true;
        if player.is_finished() {
            self.notify("Replay finished", false);
        }
        if requests.is_empty() {
            return None;
        }
        for request in &requests {
            self.replay.recent.push_back(request.topic.clone());
        }
        while self.replay.recent.len() > RECENT_LINES {
            self.replay.recent.pop_front();
        }
        self.replay.stalls = 0;
        Some((target, requests))
    }

    /// `unsent` messages of the last batch were not published because of `error`.
    pub fn replay_failed(&mut self, unsent: usize, error: &anyhow::Error) {
        self.dirty = true;
        let busy = error.downcast_ref::<QueueFull>().is_some();
        for _ in 0..unsent.min(self.replay.recent.len()) {
            self.replay.recent.pop_back();
        }
        let Some(player) = self.replay.player.as_mut() else {
            return;
        };
        player.unsend(unsent);
        self.replay.stalls += 1;
        if busy && self.replay.stalls < MAX_STALLS {
            return;
        }
        player.pause();
        self.notify(format!("Replay paused: {error:#}"), true);
    }

    /// The broker the replay goes to, by profile name.
    pub(super) fn replay_target_name(&self) -> Option<String> {
        self.replay.target.as_deref().map(|id| self.broker_name(id))
    }
}

pub(super) fn describe_pacing(settings: &ReplaySettings) -> String {
    match settings.pacing {
        Pacing::Timestamps => format!(
            "recorded times \u{d7}{}",
            speed_text(settings.speed_factor())
        ),
        Pacing::FixedRate => format!("{} messages per second", settings.rate_per_second()),
    }
}

fn speed_text(speed: f64) -> String {
    if speed.fract() == 0.0 {
        format!("{speed:.0}")
    } else {
        format!("{speed}")
    }
}
