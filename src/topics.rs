use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::{Instant, SystemTime},
};

pub const MAX_HISTORY_PER_TOPIC: usize = 100;
pub const MAX_HISTORY_BYTES_PER_TOPIC: usize = 512 * 1024;
pub const MAX_TOTAL_HISTORY_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_TOTAL_MESSAGES: usize = 200_000;
/// Bounds the work a single insert spends on global eviction.
const EVICTION_ATTEMPTS: usize = 64;

/// MQTT 5 properties of one PUBLISH; absent for MQTT 3.1.1.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageProperties {
    pub payload_is_utf8: Option<bool>,
    pub message_expiry_secs: Option<u32>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Vec<u8>>,
    pub user_properties: Vec<(String, String)>,
    pub subscription_ids: Vec<usize>,
    pub content_type: Option<String>,
}

impl MessageProperties {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub seq: u64,
    pub payload: Arc<[u8]>,
    pub qos: u8,
    pub retain: bool,
    pub dup: bool,
    pub received: Instant,
    pub wall: SystemTime,
    /// The payload differs from the previous message on the same topic (always true for the first).
    pub changed: bool,
    pub properties: Option<Arc<MessageProperties>>,
    /// ID of the broker that delivered it; `None` when only one broker is involved.
    pub broker: Option<Arc<str>>,
}

#[derive(Debug, Clone)]
pub struct MessageMeta {
    pub qos: u8,
    pub retain: bool,
    pub dup: bool,
    pub received: Instant,
    pub wall: SystemTime,
    pub properties: Option<Arc<MessageProperties>>,
    pub broker: Option<Arc<str>>,
}

impl MessageMeta {
    pub fn now() -> Self {
        Self {
            qos: 0,
            retain: false,
            dup: false,
            received: Instant::now(),
            wall: SystemTime::now(),
            properties: None,
            broker: None,
        }
    }
}

/// What the analytics views need to know about one topic.
#[derive(Debug, Clone, Copy)]
pub struct TopicSnapshot<'a> {
    pub path: &'a str,
    /// Messages since the last connection attempt.
    pub session_count: u64,
    pub total_count: u64,
    pub latest: &'a Message,
    /// Messages still in the history and the time they span.
    pub window_len: usize,
    pub window_span: std::time::Duration,
    /// Oldest first.
    pub history: &'a VecDeque<Message>,
}

/// Totals over a topic and everything below it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubtreeStats {
    pub topics: usize,
    pub messages: u64,
    pub latest: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct TopicEntry {
    key: Arc<str>,
    /// Oldest first; the last element is the latest message.
    history: VecDeque<Message>,
    bytes: usize,
    received_total: u64,
    /// Messages received since `session` began; stale when the entry's session is older.
    session: u64,
    session_count: u64,
    /// Brokers that delivered this topic, in order of first appearance.
    brokers: Vec<Arc<str>>,
}

#[derive(Debug, Clone, Default)]
pub struct TopicStore {
    entries: BTreeMap<String, TopicEntry>,
    segments: BTreeSet<String>,
    roots: BTreeSet<String>,
    children: BTreeMap<String, BTreeSet<String>>,
    structure_revision: u64,
    session: u64,
    next_seq: u64,
    total_bytes: usize,
    order: VecDeque<(Arc<str>, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicRow {
    pub path: String,
    pub segment: String,
    pub depth: usize,
    pub has_children: bool,
    pub has_value: bool,
}

impl TopicStore {
    pub fn insert(&mut self, topic: impl AsRef<str>, payload: Vec<u8>) {
        self.insert_message(topic.as_ref(), payload, MessageMeta::now());
    }

    pub fn insert_message(&mut self, topic: &str, payload: Vec<u8>, meta: MessageMeta) {
        if !self.entries.contains_key(topic) {
            self.index_segments(topic);
            self.entries.insert(
                topic.to_owned(),
                TopicEntry {
                    key: Arc::from(topic),
                    history: VecDeque::new(),
                    bytes: 0,
                    received_total: 0,
                    session: self.session,
                    session_count: 0,
                    brokers: Vec::new(),
                },
            );
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        let payload: Arc<[u8]> = Arc::from(payload);
        let Some(entry) = self.entries.get_mut(topic) else {
            return;
        };
        let changed = entry
            .history
            .back()
            .is_none_or(|previous| *previous.payload != *payload);
        let size = payload.len();
        if let Some(broker) = &meta.broker
            && !entry.brokers.iter().any(|known| known == broker)
        {
            entry.brokers.push(Arc::clone(broker));
        }
        entry.history.push_back(Message {
            seq,
            payload,
            qos: meta.qos,
            retain: meta.retain,
            dup: meta.dup,
            received: meta.received,
            wall: meta.wall,
            changed,
            properties: meta.properties,
            broker: meta.broker,
        });
        entry.bytes += size;
        entry.received_total += 1;
        if entry.session != self.session {
            entry.session = self.session;
            entry.session_count = 0;
        }
        entry.session_count += 1;
        self.total_bytes += size;
        self.order.push_back((Arc::clone(&entry.key), seq));

        while entry.history.len() > 1
            && (entry.history.len() > MAX_HISTORY_PER_TOPIC
                || entry.bytes > MAX_HISTORY_BYTES_PER_TOPIC)
        {
            if let Some(evicted) = entry.history.pop_front() {
                entry.bytes -= evicted.payload.len();
                self.total_bytes -= evicted.payload.len();
            }
        }
        self.enforce_global_budget();
    }

    fn enforce_global_budget(&mut self) {
        for _ in 0..EVICTION_ATTEMPTS {
            if self.total_bytes <= MAX_TOTAL_HISTORY_BYTES && self.order.len() <= MAX_TOTAL_MESSAGES
            {
                return;
            }
            let Some((key, seq)) = self.order.pop_front() else {
                return;
            };
            let Some(entry) = self.entries.get_mut(&*key) else {
                continue;
            };
            if entry.history.front().is_some_and(|front| front.seq == seq) {
                if entry.history.len() > 1 {
                    if let Some(evicted) = entry.history.pop_front() {
                        entry.bytes -= evicted.payload.len();
                        self.total_bytes -= evicted.payload.len();
                    }
                } else {
                    // The latest message is never evicted; keep tracking it.
                    self.order.push_back((key, seq));
                }
            }
        }
    }

    fn index_segments(&mut self, topic: &str) {
        let mut path = String::new();
        let mut parent = String::new();
        for (index, segment) in topic.split('/').enumerate() {
            if index > 0 {
                path.push('/');
            }
            path.push_str(segment);
            if self.segments.insert(path.clone()) {
                if index == 0 {
                    self.roots.insert(path.clone());
                } else {
                    self.children
                        .entry(parent.clone())
                        .or_default()
                        .insert(path.clone());
                }
                self.children.entry(path.clone()).or_default();
                self.structure_revision = self.structure_revision.wrapping_add(1);
            }
            parent.clone_from(&path);
        }
    }

    pub fn topics(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.entries.iter().filter_map(|(topic, entry)| {
            entry
                .history
                .back()
                .map(|message| (topic.as_str(), &*message.payload))
        })
    }

    pub fn topic_count(&self) -> usize {
        self.entries.len()
    }

    /// One snapshot per topic that has at least one message.
    pub fn snapshots(&self) -> impl Iterator<Item = TopicSnapshot<'_>> {
        self.entries.iter().filter_map(move |(path, entry)| {
            let latest = entry.history.back()?;
            let first = entry.history.front()?;
            Some(TopicSnapshot {
                path,
                session_count: if entry.session == self.session {
                    entry.session_count
                } else {
                    0
                },
                total_count: entry.received_total,
                latest,
                window_len: entry.history.len(),
                window_span: latest.received.saturating_duration_since(first.received),
                history: &entry.history,
            })
        })
    }

    /// Every path that has children, for "expand all".
    pub fn branch_paths(&self) -> BTreeSet<String> {
        self.children
            .iter()
            .filter(|(_, children)| !children.is_empty())
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// `path` (if it has children) and every path with children below it.
    pub fn branch_paths_below(&self, path: &str) -> Vec<String> {
        let prefix = format!("{path}/");
        let below = self
            .children
            .range(prefix.clone()..)
            .take_while(|(candidate, _)| candidate.starts_with(&prefix));
        self.children
            .get_key_value(path)
            .into_iter()
            .chain(below)
            .filter(|(_, children)| !children.is_empty())
            .map(|(candidate, _)| candidate.clone())
            .collect()
    }

    pub fn contains_path(&self, path: &str) -> bool {
        self.segments.contains(path)
    }

    pub fn latest(&self, topic: &str) -> Option<&[u8]> {
        self.latest_message(topic).map(|message| &*message.payload)
    }

    /// Brokers that delivered `topic`, in order of first appearance; empty for a single broker.
    pub fn topic_brokers(&self, topic: &str) -> &[Arc<str>] {
        self.entries
            .get(topic)
            .map_or(&[], |entry| entry.brokers.as_slice())
    }

    pub fn latest_message(&self, topic: &str) -> Option<&Message> {
        self.entries.get(topic)?.history.back()
    }

    /// Oldest first.
    pub fn history(&self, topic: &str) -> Option<&VecDeque<Message>> {
        self.entries.get(topic).map(|entry| &entry.history)
    }

    pub fn find_message(&self, topic: &str, seq: u64) -> Option<&Message> {
        self.history(topic)?
            .iter()
            .find(|message| message.seq == seq)
    }

    /// The message received right before `seq` on the same topic, if still retained.
    pub fn previous_message(&self, topic: &str, seq: u64) -> Option<&Message> {
        self.history(topic)?
            .iter()
            .rev()
            .find(|message| message.seq < seq)
    }

    /// Sequence number of the most recently stored message; later messages have larger numbers.
    pub fn last_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn received_total(&self, topic: &str) -> u64 {
        self.entries
            .get(topic)
            .map_or(0, |entry| entry.received_total)
    }

    /// Starts a new connection attempt: `received_in_session` counts from zero again.
    pub fn begin_session(&mut self) {
        self.session += 1;
    }

    pub fn received_in_session(&self, topic: &str) -> u64 {
        self.entries
            .get(topic)
            .filter(|entry| entry.session == self.session)
            .map_or(0, |entry| entry.session_count)
    }

    pub fn structure_revision(&self) -> u64 {
        self.structure_revision
    }

    /// Counts the topic itself (if it has messages) and every topic below it.
    pub fn subtree_stats(&self, path: &str) -> SubtreeStats {
        let prefix = format!("{path}/");
        let below = self
            .entries
            .range(prefix.clone()..)
            .take_while(|(topic, _)| topic.starts_with(&prefix));
        let mut stats = SubtreeStats::default();
        for (_, entry) in self.entries.get_key_value(path).into_iter().chain(below) {
            stats.topics += 1;
            stats.messages += entry.received_total;
            let wall = entry.history.back().map(|message| message.wall);
            stats.latest = stats.latest.max(wall);
        }
        stats
    }

    pub fn clear(&mut self) {
        let revision = self.structure_revision.wrapping_add(1);
        *self = Self::default();
        self.structure_revision = revision;
    }

    /// Topics (and their ancestors) for which `matches` returns true.
    pub fn matching_paths(&self, mut matches: impl FnMut(&str) -> bool) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        for topic in self.entries.keys() {
            if matches(topic) {
                let mut path = String::new();
                for (index, segment) in topic.split('/').enumerate() {
                    if index > 0 {
                        path.push('/');
                    }
                    path.push_str(segment);
                    paths.insert(path.clone());
                }
            }
        }
        paths
    }

    pub fn visible_rows(&self, expanded: &BTreeSet<String>) -> Vec<TopicRow> {
        self.visible_rows_filtered(expanded, None)
    }

    /// With a filter, only listed paths are shown and all of them are treated as expanded.
    pub fn visible_rows_filtered(
        &self,
        expanded: &BTreeSet<String>,
        filter: Option<&BTreeSet<String>>,
    ) -> Vec<TopicRow> {
        let allowed = |path: &String| filter.is_none_or(|set| set.contains(path));
        let mut rows = Vec::with_capacity(self.roots.len());
        let mut stack: Vec<_> = self
            .roots
            .iter()
            .rev()
            .filter(|path| allowed(path))
            .map(|path| (path.clone(), 0usize))
            .collect();

        while let Some((path, depth)) = stack.pop() {
            let children = self.children.get(&path);
            let visible_children = children
                .map(|items| items.iter().filter(|child| allowed(child)).count())
                .unwrap_or(0);
            rows.push(TopicRow {
                segment: path.rsplit('/').next().unwrap_or(&path).to_owned(),
                has_children: visible_children > 0,
                has_value: self.entries.contains_key(&path),
                path: path.clone(),
                depth,
            });
            if (filter.is_some() || expanded.contains(&path))
                && let Some(children) = children
            {
                stack.extend(
                    children
                        .iter()
                        .rev()
                        .filter(|child| allowed(child))
                        .map(|child| (child.clone(), depth + 1)),
                );
            }
        }
        rows
    }
}

/// MQTT filter matching: `+` matches one level, a trailing `#` the rest (and the level above it).
/// Like brokers do, wildcards in the first level do not match topics that start with `$`.
pub fn topic_matches_filter(filter: &str, topic: &str) -> bool {
    let reserved = topic.starts_with('$');
    let mut filter_levels = filter.split('/');
    let mut topic_levels = topic.split('/');
    let mut first = true;
    loop {
        match (filter_levels.next(), topic_levels.next()) {
            (Some("#"), _) => return !(reserved && first),
            (Some("+"), Some(_)) => {
                if reserved && first {
                    return false;
                }
            }
            (Some(wanted), Some(level)) if wanted == level => {}
            (None, None) => return true,
            _ => return false,
        }
        first = false;
    }
}

pub fn valid_topic_filter(filter: &str) -> bool {
    if filter.is_empty() || filter.contains('\0') {
        return false;
    }
    let segments: Vec<_> = filter.split('/').collect();
    segments
        .iter()
        .enumerate()
        .all(|(index, segment)| match *segment {
            "+" => true,
            "#" => index == segments.len() - 1,
            _ => !segment.contains('+') && !segment.contains('#'),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_topics_like_a_broker_does() {
        let matches = topic_matches_filter;
        assert!(matches("#", "a/b/c") && matches("a/#", "a/b/c") && matches("a/#", "a"));
        assert!(matches("+/hmi/#", "site1/hmi/message/x") && matches("+/hmi/#", "x/hmi"));
        assert!(!matches("+/hmi/#", "site1/conveyor/hmi") && !matches("+/hmi/#", "hmi/x"));
        assert!(matches("a/+/c", "a/b/c") && !matches("a/+/c", "a/b/d") && !matches("a/+", "a"));
        assert!(matches("a/+", "a/") && !matches("a", "a/b") && !matches("a/b", "a"));
        assert!(!matches("#", "$SYS/broker") && !matches("+/x", "$SYS/x"));
        assert!(matches("$SYS/#", "$SYS/broker/load"));
    }

    #[test]
    fn insertion_keeps_latest_value_and_indexes_path_segments() {
        let mut topics = TopicStore::default();
        topics.insert("plant/machine/status", b"RUNNING".to_vec());
        topics.insert("plant/machine/status", b"STOPPED".to_vec());
        assert_eq!(topics.topic_count(), 1);
        assert!(topics.contains_path("plant/machine"));
        assert_eq!(topics.topics().next().unwrap().1, b"STOPPED");
    }

    #[test]
    fn visible_topic_rows_follow_hierarchy_and_collapse_state() {
        let mut topics = TopicStore::default();
        topics.insert("plant/line-1/status", b"RUNNING".to_vec());
        topics.insert("plant/line-2/status", b"STOPPED".to_vec());

        let collapsed = topics.visible_rows(&BTreeSet::new());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].path, "plant");
        assert!(collapsed[0].has_children);

        let expanded = BTreeSet::from(["plant".to_owned(), "plant/line-1".to_owned()]);
        let visible = topics.visible_rows(&expanded);
        let paths: Vec<_> = visible.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "plant",
                "plant/line-1",
                "plant/line-1/status",
                "plant/line-2"
            ]
        );
        assert!(visible[2].has_value);
        assert_eq!(visible[2].depth, 2);
    }

    #[test]
    fn leading_slash_topics_keep_their_empty_root_level() {
        let mut topics = TopicStore::default();
        topics.insert("/a", b"1".to_vec());
        topics.insert("a", b"2".to_vec());

        let expanded = BTreeSet::from([String::new()]);
        let rows = topics.visible_rows(&expanded);
        let paths: Vec<_> = rows.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(paths, ["", "/a", "a"]);
        assert_eq!(topics.latest("/a"), Some(&b"1"[..]));
        assert_eq!(topics.latest("a"), Some(&b"2"[..]));
    }

    #[test]
    fn payload_updates_do_not_change_topic_structure_revision() {
        let mut topics = TopicStore::default();
        topics.insert("plant/status", b"old".to_vec());
        let revision = topics.structure_revision();
        topics.insert("plant/status", b"new".to_vec());
        assert_eq!(topics.structure_revision(), revision);
        assert_eq!(topics.latest("plant/status"), Some(&b"new"[..]));
    }

    #[test]
    fn topic_filter_wildcards_follow_mqtt_segment_rules() {
        assert!(valid_topic_filter("plant/+/status"));
        assert!(valid_topic_filter("plant/#"));
        assert!(!valid_topic_filter("plant/#/status"));
        assert!(!valid_topic_filter("plant/ma+chine"));
        assert!(!valid_topic_filter(""));
    }

    #[test]
    fn history_tracks_changes_metadata_and_previous_message() {
        let mut topics = TopicStore::default();
        topics.insert("a/b", b"1".to_vec());
        topics.insert("a/b", b"1".to_vec());
        topics.insert_message(
            "a/b",
            b"2".to_vec(),
            MessageMeta {
                qos: 1,
                retain: true,
                ..MessageMeta::now()
            },
        );

        let history = topics.history("a/b").unwrap();
        let flags: Vec<_> = history.iter().map(|message| message.changed).collect();
        assert_eq!(flags, [true, false, true]);
        let latest = topics.latest_message("a/b").unwrap();
        assert_eq!((latest.qos, latest.retain), (1, true));
        assert_eq!(topics.received_total("a/b"), 3);
        let previous = topics.previous_message("a/b", latest.seq).unwrap();
        assert_eq!(&*previous.payload, b"1");
        assert!(topics.find_message("a/b", latest.seq).is_some());
        assert!(topics.find_message("a/b", 9_999).is_none());
    }

    #[test]
    fn history_is_bounded_per_topic_and_keeps_the_latest() {
        let mut topics = TopicStore::default();
        for index in 0..MAX_HISTORY_PER_TOPIC + 25 {
            topics.insert("busy", index.to_string().into_bytes());
        }
        let history = topics.history("busy").unwrap();
        assert_eq!(history.len(), MAX_HISTORY_PER_TOPIC);
        assert_eq!(
            topics.received_total("busy"),
            (MAX_HISTORY_PER_TOPIC + 25) as u64
        );
        assert_eq!(
            topics.latest("busy"),
            Some((MAX_HISTORY_PER_TOPIC + 24).to_string().as_bytes())
        );

        let big = vec![b'x'; MAX_HISTORY_BYTES_PER_TOPIC];
        topics.insert("large", big.clone());
        topics.insert("large", big);
        assert_eq!(topics.history("large").unwrap().len(), 1);
    }

    #[test]
    fn filtered_rows_show_matches_with_their_ancestors_expanded() {
        let mut topics = TopicStore::default();
        topics.insert("plant/line-1/status", b"RUNNING".to_vec());
        topics.insert("plant/line-2/status", b"STOPPED".to_vec());
        topics.insert("other/value", b"1".to_vec());

        let paths = topics.matching_paths(|topic| topic.contains("line-2"));
        let rows = topics.visible_rows_filtered(&BTreeSet::new(), Some(&paths));
        let shown: Vec<_> = rows.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(shown, ["plant", "plant/line-2", "plant/line-2/status"]);
        assert!(rows[0].has_children);
    }

    #[test]
    fn clear_resets_data_and_invalidates_row_caches() {
        let mut topics = TopicStore::default();
        topics.insert("a/b", b"1".to_vec());
        let revision = topics.structure_revision();
        topics.clear();
        assert_eq!(topics.topic_count(), 0);
        assert_ne!(topics.structure_revision(), revision);
        assert!(topics.visible_rows(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn subtree_stats_count_a_topic_and_everything_below_it() {
        let mut topics = TopicStore::default();
        topics.insert("a/b", b"1".to_vec());
        topics.insert("a/b", b"2".to_vec());
        topics.insert("a/b/c", b"3".to_vec());
        topics.insert("a/bc", b"4".to_vec());
        topics.insert("z", b"5".to_vec());

        let stats = topics.subtree_stats("a/b");
        assert_eq!((stats.topics, stats.messages), (2, 3));
        assert!(stats.latest.is_some());
        assert_eq!(topics.subtree_stats("a").topics, 3);
        assert_eq!(topics.subtree_stats("missing"), SubtreeStats::default());
    }

    #[test]
    fn session_counts_restart_with_each_connection_attempt() {
        let mut topics = TopicStore::default();
        topics.insert("a", b"1".to_vec());
        topics.insert("a", b"2".to_vec());
        assert_eq!(topics.received_in_session("a"), 2);

        topics.begin_session();
        assert_eq!(topics.received_in_session("a"), 0);
        assert_eq!(topics.received_total("a"), 2, "the lifetime total is kept");
        topics.insert("a", b"3".to_vec());
        topics.insert("b", b"4".to_vec());
        assert_eq!(topics.received_in_session("a"), 1);
        assert_eq!(topics.received_in_session("b"), 1);
        assert_eq!(topics.received_in_session("missing"), 0);
    }

    #[test]
    fn branch_paths_lists_every_path_with_children() {
        let mut topics = TopicStore::default();
        topics.insert("a/b/c", b"1".to_vec());
        topics.insert("a/d", b"2".to_vec());
        topics.insert("e", b"3".to_vec());
        let branches: Vec<_> = topics.branch_paths().into_iter().collect();
        assert_eq!(branches, ["a", "a/b"]);
    }

    #[test]
    fn branch_paths_below_stays_inside_the_subtree() {
        let mut topics = TopicStore::default();
        topics.insert("a/b/c", b"1".to_vec());
        topics.insert("a/b/d/e", b"2".to_vec());
        topics.insert("a-x/y", b"3".to_vec());
        topics.insert("a/z", b"4".to_vec());
        assert_eq!(topics.branch_paths_below("a/b"), ["a/b", "a/b/d"]);
        assert_eq!(topics.branch_paths_below("a"), ["a", "a/b", "a/b/d"]);
        assert!(topics.branch_paths_below("a/z").is_empty());
    }

    #[test]
    fn snapshots_report_session_counts_and_the_history_window() {
        let mut topics = TopicStore::default();
        topics.insert("a", b"1".to_vec());
        topics.begin_session();
        topics.insert("b", b"2".to_vec());
        topics.insert("b", b"3".to_vec());
        let mut snapshots: Vec<_> = topics.snapshots().collect();
        snapshots.sort_by_key(|snapshot| snapshot.path);
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            (
                snapshots[0].path,
                snapshots[0].session_count,
                snapshots[0].total_count
            ),
            ("a", 0, 1)
        );
        assert_eq!(
            (
                snapshots[1].path,
                snapshots[1].session_count,
                snapshots[1].window_len
            ),
            ("b", 2, 2)
        );
        assert_eq!(&*snapshots[1].latest.payload, b"3");
    }
}
