//! The logging dialog (`L`): switch the `.mqttlog` files on or off and choose their directory.

use std::path::PathBuf;

use crossterm::event::KeyCode;
use mate::{mqttlog, ui_state::LogSettings};

use super::{UiState, publish};

pub const FIELD_ENABLED: usize = 0;
pub const FIELD_DIRECTORY: usize = 1;
pub const FIELD_SIZE: usize = 2;
const FIELD_COUNT: usize = 3;

pub struct LogDialog {
    pub enabled: bool,
    pub directory: String,
    /// The size limit in megabytes as typed.
    pub size: String,
    /// Cursor in the directory as a character index.
    pub cursor: usize,
    pub field: usize,
    pub error: Option<String>,
}

impl UiState {
    /// The size at which a log file is closed and the next block begins.
    pub fn log_max_bytes(&self) -> u64 {
        self.log.max_bytes()
    }

    /// The directory messages are written to, or `None` while logging is off.
    pub fn log_directory(&self) -> Option<PathBuf> {
        self.log
            .enabled
            .then(|| self.log.resolved_directory(self.broker_store.root()))
    }

    /// Switches logging off after a write error and says why.
    pub fn logging_failed(&mut self, error: &str) {
        self.log.enabled = false;
        self.notify(format!("Logging stopped: {error}"), true);
        self.dirty = true;
        self.persist_ui_settings();
    }

    pub(super) fn open_log_dialog(&mut self) {
        let directory = self.log.directory.clone();
        self.log_dialog = Some(LogDialog {
            enabled: self.log.enabled,
            cursor: directory.chars().count(),
            directory,
            size: self.log.max_megabytes.to_string(),
            field: FIELD_ENABLED,
            error: None,
        });
    }

    /// The files the current connections write to, for the dialog.
    pub(super) fn log_files_preview(&self, dialog: &LogDialog) -> Vec<(String, PathBuf)> {
        let settings = LogSettings {
            enabled: true,
            directory: dialog.directory.clone(),
            ..LogSettings::default()
        };
        let directory = settings.resolved_directory(self.broker_store.root());
        let brokers: Vec<(String, String)> = if self.connections.is_empty() {
            self.resolve_profile()
                .into_iter()
                .map(|profile| (profile.name, profile.id))
                .collect()
        } else {
            self.connections
                .iter()
                .map(|connection| (connection.broker_name.clone(), connection.broker_id.clone()))
                .collect()
        };
        brokers
            .into_iter()
            .map(|(name, id)| {
                let day = self.log_format.day(std::time::SystemTime::now());
                (name, directory.join(mqttlog::file_name(&id, &day, 0)))
            })
            .collect()
    }

    pub(super) fn handle_log_dialog_key(&mut self, code: KeyCode) {
        let Some(mut dialog) = self.log_dialog.take() else {
            return;
        };
        dialog.error = None;
        match code {
            KeyCode::Esc => return,
            KeyCode::Tab | KeyCode::Down => dialog.field = (dialog.field + 1) % FIELD_COUNT,
            KeyCode::BackTab | KeyCode::Up => {
                dialog.field = (dialog.field + FIELD_COUNT - 1) % FIELD_COUNT;
            }
            KeyCode::Enter => match self.apply_log_dialog(&dialog) {
                Ok(()) => return,
                Err(error) => dialog.error = Some(error),
            },
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                if dialog.field == FIELD_ENABLED =>
            {
                dialog.enabled = !dialog.enabled;
            }
            KeyCode::Backspace if dialog.field == FIELD_SIZE => {
                dialog.size.pop();
            }
            KeyCode::Char(digit) if dialog.field == FIELD_SIZE && digit.is_ascii_digit() => {
                if dialog.size.len() < 5 {
                    dialog.size.push(digit);
                }
            }
            _ if dialog.field == FIELD_DIRECTORY => match code {
                KeyCode::Left => dialog.cursor = dialog.cursor.saturating_sub(1),
                KeyCode::Right => {
                    dialog.cursor = (dialog.cursor + 1).min(dialog.directory.chars().count());
                }
                KeyCode::Home => dialog.cursor = 0,
                KeyCode::End => dialog.cursor = dialog.directory.chars().count(),
                KeyCode::Backspace => {
                    publish::backspace_at(&mut dialog.directory, &mut dialog.cursor);
                }
                KeyCode::Delete => publish::delete_at(&mut dialog.directory, dialog.cursor),
                KeyCode::Char(character) => publish::insert_at(
                    &mut dialog.directory,
                    &mut dialog.cursor,
                    character.encode_utf8(&mut [0; 4]),
                ),
                _ => {}
            },
            _ => {}
        }
        self.log_dialog = Some(dialog);
    }

    fn apply_log_dialog(&mut self, dialog: &LogDialog) -> Result<(), String> {
        let max_megabytes = dialog
            .size
            .parse::<u32>()
            .ok()
            .filter(|megabytes| (1..=mqttlog::MAX_MEGABYTES).contains(megabytes))
            .ok_or_else(|| {
                format!(
                    "the size limit must be between 1 and {} MB",
                    mqttlog::MAX_MEGABYTES
                )
            })?;
        let wanted = LogSettings {
            enabled: dialog.enabled,
            directory: dialog.directory.trim().to_owned(),
            max_megabytes,
        };
        if wanted.enabled {
            let directory = wanted.resolved_directory(self.broker_store.root());
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("cannot use {}: {error}", directory.display()))?;
        }
        self.notify(
            if wanted.enabled {
                format!(
                    "Logging to {}",
                    wanted
                        .resolved_directory(self.broker_store.root())
                        .display()
                )
            } else {
                "Logging is off".to_owned()
            },
            false,
        );
        self.log = wanted;
        self.persist_ui_settings();
        Ok(())
    }
}
