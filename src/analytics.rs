//! Pure helpers for the analytics views: payload classification and rate arithmetic.

use std::time::{Duration, Instant};

use serde::de::IgnoredAny;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadKind {
    /// Valid JSON of any kind, including bare numbers and strings.
    Json,
    Empty,
    /// Starts like JSON (`{`, `[` or `"`) but does not parse.
    BrokenJson,
    Text,
    Binary,
}

impl PayloadKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Empty => "empty",
            Self::BrokenJson => "broken",
            Self::Text => "text",
            Self::Binary => "binary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadClass {
    pub kind: PayloadKind,
    /// The parser's complaint for broken JSON.
    pub error: Option<String>,
}

/// Validates without building a document, so classifying many topics stays cheap.
pub fn classify_payload(payload: &[u8]) -> PayloadClass {
    let plain = |kind| PayloadClass { kind, error: None };
    if payload.is_empty() {
        return plain(PayloadKind::Empty);
    }
    let error = match serde_json::from_slice::<IgnoredAny>(payload) {
        Ok(_) => return plain(PayloadKind::Json),
        Err(error) => error.to_string(),
    };
    if std::str::from_utf8(payload).is_err() {
        return plain(PayloadKind::Binary);
    }
    let first = payload.iter().find(|byte| !byte.is_ascii_whitespace());
    if matches!(first, Some(b'{' | b'[' | b'"')) {
        PayloadClass {
            kind: PayloadKind::BrokenJson,
            error: Some(error),
        }
    } else {
        plain(PayloadKind::Text)
    }
}

/// Messages per second over a window of `messages` messages that spans `span`.
pub fn window_rate(messages: usize, span: Duration) -> Option<f64> {
    (messages >= 2 && !span.is_zero()).then(|| (messages - 1) as f64 / span.as_secs_f64())
}

/// Fewer intervals than this say nothing about regularity.
pub const MIN_JITTER_INTERVALS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntervalStats {
    pub mean: Duration,
    pub min: Duration,
    pub max: Duration,
    /// Standard deviation divided by the mean: 0 for a perfectly regular topic.
    pub jitter: f64,
}

/// Statistics over the gaps between consecutive receive times (oldest first).
pub fn interval_stats(times: impl IntoIterator<Item = Instant>) -> Option<IntervalStats> {
    let times: Vec<Instant> = times.into_iter().collect();
    let gaps: Vec<Duration> = times
        .windows(2)
        .map(|pair| pair[1].saturating_duration_since(pair[0]))
        .collect();
    if gaps.len() < MIN_JITTER_INTERVALS {
        return None;
    }
    let seconds: Vec<f64> = gaps.iter().map(Duration::as_secs_f64).collect();
    let mean = seconds.iter().sum::<f64>() / seconds.len() as f64;
    if mean <= 0.0 {
        return None;
    }
    let variance =
        seconds.iter().map(|gap| (gap - mean).powi(2)).sum::<f64>() / seconds.len() as f64;
    Some(IntervalStats {
        mean: Duration::from_secs_f64(mean),
        min: *gaps.iter().min()?,
        max: *gaps.iter().max()?,
        jitter: variance.sqrt() / mean,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(offsets: &[u64]) -> Vec<Instant> {
        let start = Instant::now();
        offsets
            .iter()
            .map(|seconds| start + Duration::from_secs(*seconds))
            .collect()
    }

    #[test]
    fn a_regular_topic_has_no_jitter_and_an_irregular_one_has_some() {
        let regular = interval_stats(times(&[0, 1, 2, 3, 4])).unwrap();
        assert_eq!(regular.mean, Duration::from_secs(1));
        assert_eq!(
            (regular.min, regular.max),
            (Duration::from_secs(1), Duration::from_secs(1))
        );
        assert!(regular.jitter < 1e-9);

        let irregular = interval_stats(times(&[0, 1, 3, 6, 10])).unwrap();
        assert_eq!(irregular.mean, Duration::from_millis(2500));
        assert_eq!(
            (irregular.min, irregular.max),
            (Duration::from_secs(1), Duration::from_secs(4))
        );
        assert!((irregular.jitter - 0.447).abs() < 0.001);
    }

    #[test]
    fn jitter_needs_enough_intervals_and_a_nonzero_mean() {
        assert!(interval_stats(times(&[0, 1, 2, 3])).is_none());
        assert!(interval_stats(times(&[5, 5, 5, 5, 5])).is_none());
    }

    fn kind(payload: &[u8]) -> PayloadKind {
        classify_payload(payload).kind
    }

    #[test]
    fn payloads_are_sorted_into_json_text_broken_binary_and_empty() {
        assert_eq!(kind(br#"{"a":1}"#), PayloadKind::Json);
        assert_eq!(kind(b"[1, 2]"), PayloadKind::Json);
        assert_eq!(
            kind(b"42"),
            PayloadKind::Json,
            "a bare number is valid JSON"
        );
        assert_eq!(kind(b" true\n"), PayloadKind::Json);
        assert_eq!(kind(b""), PayloadKind::Empty);
        assert_eq!(kind(b"RUNNING"), PayloadKind::Text);
        assert_eq!(kind(b"12:30 start"), PayloadKind::Text);
        assert_eq!(kind(br#"{"a":"#), PayloadKind::BrokenJson);
        assert_eq!(kind(br#"{"a":1} trailing"#), PayloadKind::BrokenJson);
        assert_eq!(kind(b"[1,2"), PayloadKind::BrokenJson);
        assert_eq!(kind(&[0x00, 0xff, 0xfe]), PayloadKind::Binary);
        assert_eq!(kind(&[b'{', 0xff]), PayloadKind::Binary);
    }

    #[test]
    fn broken_json_carries_the_parser_message() {
        let class = classify_payload(br#"{"a":"#);
        assert!(class.error.unwrap().contains("EOF"));
        assert!(classify_payload(b"RUNNING").error.is_none());
    }

    #[test]
    fn window_rates_need_two_messages_and_a_span() {
        assert_eq!(window_rate(0, Duration::from_secs(1)), None);
        assert_eq!(window_rate(1, Duration::from_secs(1)), None);
        assert_eq!(window_rate(5, Duration::ZERO), None);
        assert_eq!(window_rate(11, Duration::from_secs(5)), Some(2.0));
    }
}
