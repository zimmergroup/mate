//! Pure layout computation for the main view; independent of application state.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use mate::ui_state::LayoutSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Topics,
    Inspector,
    History,
    /// Docked below the other panes.
    Publish,
}

/// Terminals at least this wide can show three panes side by side.
pub const WIDE_WIDTH: u16 = 100;
/// Terminals narrower than this show a single pane at a time.
pub const NARROW_WIDTH: u16 = 60;
pub const MIN_SIDE_PERCENT: u16 = 15;
pub const MIN_INSPECTOR_PERCENT: u16 = 30;
const STEP_PERCENT: u16 = 5;
/// The publish panel needs this many rows in total to leave room for the panes above it.
const MIN_HEIGHT_WITH_PUBLISH: u16 = 22;

#[derive(Debug, Clone)]
pub struct LayoutPrefs {
    pub show_history: bool,
    /// Not saved: the panel starts hidden.
    pub show_publish: bool,
    pub maximized: Option<Panel>,
    pub topics_two_pane: u16,
    pub topics_three_pane: u16,
    pub history_three_pane: u16,
}

impl Default for LayoutPrefs {
    fn default() -> Self {
        Self {
            show_history: false,
            show_publish: false,
            maximized: None,
            topics_two_pane: 35,
            topics_three_pane: 25,
            history_three_pane: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneRects {
    pub topics: Option<Rect>,
    pub inspector: Option<Rect>,
    pub history: Option<Rect>,
    pub publish: Option<Rect>,
}

impl LayoutPrefs {
    pub fn settings(&self) -> LayoutSettings {
        LayoutSettings {
            show_history: self.show_history,
            topics_two_pane: self.topics_two_pane,
            topics_three_pane: self.topics_three_pane,
            history_three_pane: self.history_three_pane,
        }
    }

    /// Applies saved settings; values outside the allowed ranges keep their defaults.
    pub fn apply(&mut self, saved: &LayoutSettings) {
        self.show_history = saved.show_history;
        let two = saved.topics_two_pane;
        if (MIN_SIDE_PERCENT..=100 - MIN_INSPECTOR_PERCENT).contains(&two) {
            self.topics_two_pane = two;
        }
        let (topics, history) = (saved.topics_three_pane, saved.history_three_pane);
        if topics >= MIN_SIDE_PERCENT
            && history >= MIN_SIDE_PERCENT
            && topics + history + MIN_INSPECTOR_PERCENT <= 100
        {
            self.topics_three_pane = topics;
            self.history_three_pane = history;
        }
    }

    pub fn visible_panels(&self) -> Vec<Panel> {
        let mut panels = vec![Panel::Topics, Panel::Inspector];
        if self.show_history {
            panels.push(Panel::History);
        }
        if self.show_publish {
            panels.push(Panel::Publish);
        }
        panels
    }

    fn is_visible(&self, panel: Panel) -> bool {
        match panel {
            Panel::Topics | Panel::Inspector => true,
            Panel::History => self.show_history,
            Panel::Publish => self.show_publish,
        }
    }

    pub fn next_panel(&self, focus: Panel, backwards: bool) -> Panel {
        let panels = self.visible_panels();
        let position = panels.iter().position(|panel| *panel == focus).unwrap_or(0);
        let next = if backwards {
            (position + panels.len() - 1) % panels.len()
        } else {
            (position + 1) % panels.len()
        };
        panels[next]
    }

    fn three_pane(&self, width: u16) -> bool {
        self.show_history && width >= WIDE_WIDTH
    }

    /// Widens (positive) or narrows (negative) a panel; returns whether anything changed.
    pub fn resize(&mut self, panel: Panel, delta: i16, width: u16) -> bool {
        if panel == Panel::Publish {
            return false;
        }
        let step = delta.signum() * STEP_PERCENT as i16;
        let before = (
            self.topics_two_pane,
            self.topics_three_pane,
            self.history_three_pane,
        );
        if self.three_pane(width) {
            let (mut topics, mut history) = (
                i32::from(self.topics_three_pane),
                i32::from(self.history_three_pane),
            );
            match panel {
                Panel::Topics => topics += i32::from(step),
                Panel::History => history += i32::from(step),
                // The inspector takes its space from the history pane.
                Panel::Inspector | Panel::Publish => history -= i32::from(step),
            }
            let inspector = 100 - topics - history;
            if topics >= i32::from(MIN_SIDE_PERCENT)
                && history >= i32::from(MIN_SIDE_PERCENT)
                && inspector >= i32::from(MIN_INSPECTOR_PERCENT)
            {
                self.topics_three_pane = topics as u16;
                self.history_three_pane = history as u16;
            }
        } else if panel != Panel::History {
            let topics = i32::from(self.topics_two_pane)
                + if panel == Panel::Topics {
                    i32::from(step)
                } else {
                    -i32::from(step)
                };
            if topics >= i32::from(MIN_SIDE_PERCENT)
                && 100 - topics >= i32::from(MIN_INSPECTOR_PERCENT)
            {
                self.topics_two_pane = topics as u16;
            }
        }
        before
            != (
                self.topics_two_pane,
                self.topics_three_pane,
                self.history_three_pane,
            )
    }

    pub fn compute(&self, area: Rect, focus: Panel) -> PaneRects {
        let maximized = self.maximized.filter(|panel| self.is_visible(*panel));
        if let Some(panel) = maximized {
            return single(panel, area);
        }
        let cramped = area.height < MIN_HEIGHT_WITH_PUBLISH;
        if area.width < NARROW_WIDTH || (self.show_publish && cramped && focus == Panel::Publish) {
            let panel = if self.is_visible(focus) {
                focus
            } else {
                Panel::Inspector
            };
            return single(panel, area);
        }
        if self.show_publish && !cramped {
            let height = (area.height * 45 / 100).clamp(10, 16);
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(height)])
                .split(area);
            let mut rects = self.compute_panes(parts[0]);
            rects.publish = Some(parts[1]);
            return rects;
        }
        self.compute_panes(area)
    }

    fn compute_panes(&self, area: Rect) -> PaneRects {
        if !self.show_history {
            let [topics, inspector] = split_horizontal(area, [self.topics_two_pane]);
            return PaneRects {
                topics: Some(topics),
                inspector: Some(inspector),
                ..PaneRects::default()
            };
        }
        if area.width >= WIDE_WIDTH {
            let [topics, inspector, history] =
                split_horizontal_three(area, self.topics_three_pane, self.history_three_pane);
            return PaneRects {
                topics: Some(topics),
                inspector: Some(inspector),
                history: Some(history),
                ..PaneRects::default()
            };
        }
        // Medium width: history is stacked below the inspector.
        let [topics, right] = split_horizontal(area, [self.topics_two_pane]);
        let stacked = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(right);
        PaneRects {
            topics: Some(topics),
            inspector: Some(stacked[0]),
            history: Some(stacked[1]),
            ..PaneRects::default()
        }
    }
}

fn single(panel: Panel, area: Rect) -> PaneRects {
    let mut rects = PaneRects::default();
    match panel {
        Panel::Topics => rects.topics = Some(area),
        Panel::Inspector => rects.inspector = Some(area),
        Panel::History => rects.history = Some(area),
        Panel::Publish => rects.publish = Some(area),
    }
    rects
}

fn split_horizontal(area: Rect, [first]: [u16; 1]) -> [Rect; 2] {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(first),
            Constraint::Percentage(100 - first),
        ])
        .split(area);
    [parts[0], parts[1]]
}

fn split_horizontal_three(area: Rect, first: u16, last: u16) -> [Rect; 3] {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(first),
            Constraint::Percentage(100 - first - last),
            Constraint::Percentage(last),
        ])
        .split(area);
    [parts[0], parts[1], parts[2]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16) -> Rect {
        Rect::new(0, 0, width, 30)
    }

    #[test]
    fn default_layout_is_two_panes_without_history() {
        let rects = LayoutPrefs::default().compute(area(120), Panel::Topics);
        let (topics, inspector) = (rects.topics.unwrap(), rects.inspector.unwrap());
        assert!(rects.history.is_none());
        assert_eq!(topics.width + inspector.width, 120);
        assert!(
            (41..=43).contains(&topics.width),
            "topics was {}",
            topics.width
        );
    }

    #[test]
    fn history_adds_a_third_pane_on_wide_terminals_and_stacks_on_medium_ones() {
        let prefs = LayoutPrefs {
            show_history: true,
            ..LayoutPrefs::default()
        };
        let wide = prefs.compute(area(140), Panel::Topics);
        assert_eq!(
            wide.topics.unwrap().width
                + wide.inspector.unwrap().width
                + wide.history.unwrap().width,
            140
        );
        assert_eq!(wide.history.unwrap().y, wide.inspector.unwrap().y);

        let medium = prefs.compute(area(80), Panel::Topics);
        assert_eq!(medium.history.unwrap().x, medium.inspector.unwrap().x);
        assert!(medium.history.unwrap().y > medium.inspector.unwrap().y);
    }

    #[test]
    fn maximized_panel_fills_the_area_and_narrow_terminals_show_one_pane() {
        let mut prefs = LayoutPrefs {
            maximized: Some(Panel::Inspector),
            ..LayoutPrefs::default()
        };
        let rects = prefs.compute(area(120), Panel::Topics);
        assert_eq!(rects.inspector, Some(area(120)));
        assert!(rects.topics.is_none() && rects.history.is_none());

        prefs.maximized = None;
        let narrow = prefs.compute(area(40), Panel::Inspector);
        assert_eq!(narrow.inspector, Some(area(40)));
        assert!(narrow.topics.is_none());
        let narrow_topics = prefs.compute(area(40), Panel::Topics);
        assert_eq!(narrow_topics.topics, Some(area(40)));
    }

    #[test]
    fn maximizing_a_hidden_history_panel_is_ignored() {
        let prefs = LayoutPrefs {
            maximized: Some(Panel::History),
            ..LayoutPrefs::default()
        };
        let rects = prefs.compute(area(120), Panel::Topics);
        assert!(rects.topics.is_some() && rects.inspector.is_some());
    }

    #[test]
    fn resizing_respects_minimum_widths() {
        let mut prefs = LayoutPrefs::default();
        assert!(prefs.resize(Panel::Topics, 1, 120));
        assert_eq!(prefs.topics_two_pane, 40);
        assert!(prefs.resize(Panel::Inspector, 1, 120));
        assert_eq!(prefs.topics_two_pane, 35);
        for _ in 0..20 {
            prefs.resize(Panel::Topics, -1, 120);
        }
        assert_eq!(prefs.topics_two_pane, MIN_SIDE_PERCENT);
        assert!(!prefs.resize(Panel::Topics, -1, 120));
        for _ in 0..20 {
            prefs.resize(Panel::Topics, 1, 120);
        }
        assert_eq!(prefs.topics_two_pane, 70);
    }

    #[test]
    fn three_pane_resize_adjusts_history_and_keeps_inspector_large_enough() {
        let mut prefs = LayoutPrefs {
            show_history: true,
            ..LayoutPrefs::default()
        };
        assert!(prefs.resize(Panel::History, 1, 140));
        assert_eq!(prefs.history_three_pane, 35);
        for _ in 0..20 {
            prefs.resize(Panel::History, 1, 140);
        }
        let inspector = 100 - prefs.topics_three_pane - prefs.history_three_pane;
        assert!(inspector >= MIN_INSPECTOR_PERCENT);
    }

    #[test]
    fn saved_settings_round_trip_and_invalid_values_are_ignored() {
        let mut prefs = LayoutPrefs {
            show_history: true,
            topics_two_pane: 45,
            topics_three_pane: 20,
            history_three_pane: 35,
            ..LayoutPrefs::default()
        };
        let saved = prefs.settings();
        let mut restored = LayoutPrefs::default();
        restored.apply(&saved);
        assert_eq!(restored.settings(), saved);

        prefs.apply(&LayoutSettings {
            show_history: false,
            topics_two_pane: 3,
            topics_three_pane: 60,
            history_three_pane: 60,
        });
        assert!(!prefs.show_history);
        assert_eq!(prefs.topics_two_pane, 45, "out-of-range width is ignored");
        assert_eq!(
            (prefs.topics_three_pane, prefs.history_three_pane),
            (20, 35)
        );
    }

    #[test]
    fn focus_cycles_only_through_visible_panels() {
        let mut prefs = LayoutPrefs::default();
        assert_eq!(prefs.next_panel(Panel::Topics, false), Panel::Inspector);
        assert_eq!(prefs.next_panel(Panel::Inspector, false), Panel::Topics);
        prefs.show_history = true;
        assert_eq!(prefs.next_panel(Panel::Inspector, false), Panel::History);
        assert_eq!(prefs.next_panel(Panel::Topics, true), Panel::History);
    }

    #[test]
    fn the_publish_panel_is_docked_below_the_panes_and_needs_enough_height() {
        let prefs = LayoutPrefs {
            show_publish: true,
            show_history: true,
            ..LayoutPrefs::default()
        };
        let tall = Rect::new(0, 0, 140, 40);
        let rects = prefs.compute(tall, Panel::Topics);
        let publish = rects.publish.unwrap();
        assert_eq!((publish.x, publish.width), (0, 140));
        assert_eq!(publish.y + publish.height, 40);
        assert_eq!(rects.topics.unwrap().y, 0);
        assert!(rects.history.is_some() && rects.topics.unwrap().height + publish.height == 40);

        let short = Rect::new(0, 0, 140, 18);
        assert!(prefs.compute(short, Panel::Topics).publish.is_none());
        assert_eq!(prefs.compute(short, Panel::Publish).publish, Some(short));

        let narrow = Rect::new(0, 0, 50, 40);
        assert_eq!(prefs.compute(narrow, Panel::Publish).publish, Some(narrow));
        assert!(prefs.compute(narrow, Panel::Topics).publish.is_none());
    }

    #[test]
    fn focus_cycles_through_the_publish_panel_and_it_cannot_be_resized() {
        let mut prefs = LayoutPrefs {
            show_publish: true,
            ..LayoutPrefs::default()
        };
        assert_eq!(prefs.next_panel(Panel::Inspector, false), Panel::Publish);
        assert_eq!(prefs.next_panel(Panel::Publish, false), Panel::Topics);
        assert!(!prefs.resize(Panel::Publish, 1, 120));
        prefs.show_publish = false;
        assert_eq!(prefs.next_panel(Panel::Inspector, false), Panel::Topics);
        assert!(prefs.settings() == LayoutPrefs::default().settings());
    }
}
