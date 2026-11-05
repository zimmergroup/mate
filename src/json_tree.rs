//! Parsed JSON documents flattened into collapsible rows. Independent of any UI toolkit.

use std::collections::BTreeSet;

use serde_json::Value;

/// Containers with more children show a trailing "more" row instead of all of them.
pub const MAX_CHILDREN_SHOWN: usize = 500;
const MAX_SEARCH_NODES: usize = 200_000;
const MAX_SCALAR_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
    More,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRow {
    /// RFC 6901 pointer; the root is the empty string.
    pub pointer: String,
    /// Human-readable path such as `$.data.items[2]`.
    pub path: String,
    pub depth: usize,
    pub label: String,
    pub kind: NodeKind,
    /// Scalar text (strings quoted) or a container summary such as `{3 keys}`.
    pub value: String,
    pub expandable: bool,
    pub expanded: bool,
}

#[derive(Debug, Clone)]
pub struct JsonDoc {
    root: Value,
}

impl JsonDoc {
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(payload)
            .map(|root| Self { root })
            .map_err(|error| error.to_string())
    }

    pub fn pretty(&self) -> String {
        serde_json::to_string_pretty(&self.root).unwrap_or_default()
    }

    pub fn value_at(&self, pointer: &str) -> Option<&Value> {
        self.root.pointer(pointer)
    }

    /// Pointers of all containers up to `max_depth` (the root has depth 0).
    pub fn default_expanded(&self, max_depth: usize) -> BTreeSet<String> {
        let mut expanded = BTreeSet::new();
        collect_containers(&self.root, String::new(), 0, max_depth, &mut expanded);
        expanded
    }

    pub fn rows(&self, expanded: &BTreeSet<String>) -> Vec<JsonRow> {
        let mut rows = Vec::new();
        push_rows(
            &self.root,
            String::new(),
            "$".to_owned(),
            "root".to_owned(),
            0,
            expanded,
            &mut rows,
        );
        rows
    }

    /// Pointers of nodes whose key or scalar value contains `query` (case-insensitive), in document order.
    pub fn search(&self, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        let mut matches = Vec::new();
        if query.is_empty() {
            return matches;
        }
        let mut budget = MAX_SEARCH_NODES;
        search_value(&self.root, "", None, &query, &mut matches, &mut budget);
        matches
    }
}

/// All proper ancestors of a pointer, root first.
pub fn ancestors(pointer: &str) -> Vec<String> {
    let mut result = vec![String::new()];
    for (index, _) in pointer.match_indices('/').skip(1) {
        result.push(pointer[..index].to_owned());
    }
    if pointer.is_empty() {
        result.clear();
    }
    result
}

/// Text copied to the clipboard for a value: strings without quotes, everything else as JSON.
pub fn copy_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) | Value::Array(_) => {
            serde_json::to_string_pretty(value).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn is_plain_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_' || character == '-')
}

fn child_path(path: &str, key: &str) -> String {
    if is_plain_key(key) {
        format!("{path}.{key}")
    } else {
        format!("{path}[{}]", serde_json::to_string(key).unwrap_or_default())
    }
}

fn scalar_text(value: &Value) -> String {
    let text = match value {
        Value::String(text) => serde_json::to_string(text).unwrap_or_default(),
        other => other.to_string(),
    };
    if text.chars().count() > MAX_SCALAR_CHARS {
        let mut shortened: String = text.chars().take(MAX_SCALAR_CHARS).collect();
        shortened.push('…');
        shortened
    } else {
        text
    }
}

fn summary(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => Some(match map.len() {
            0 => "{}".to_owned(),
            1 => "{1 key}".to_owned(),
            count => format!("{{{count} keys}}"),
        }),
        Value::Array(items) => Some(match items.len() {
            0 => "[]".to_owned(),
            1 => "[1 item]".to_owned(),
            count => format!("[{count} items]"),
        }),
        _ => None,
    }
}

fn kind_of(value: &Value) -> NodeKind {
    match value {
        Value::Object(_) => NodeKind::Object,
        Value::Array(_) => NodeKind::Array,
        Value::String(_) => NodeKind::String,
        Value::Number(_) => NodeKind::Number,
        Value::Bool(_) => NodeKind::Bool,
        Value::Null => NodeKind::Null,
    }
}

fn child_count(value: &Value) -> usize {
    match value {
        Value::Object(map) => map.len(),
        Value::Array(items) => items.len(),
        _ => 0,
    }
}

fn collect_containers(
    value: &Value,
    pointer: String,
    depth: usize,
    max_depth: usize,
    expanded: &mut BTreeSet<String>,
) {
    if depth > max_depth || child_count(value) == 0 {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter().take(MAX_CHILDREN_SHOWN) {
                let child_pointer = format!("{pointer}/{}", escape_token(key));
                collect_containers(child, child_pointer, depth + 1, max_depth, expanded);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate().take(MAX_CHILDREN_SHOWN) {
                collect_containers(
                    child,
                    format!("{pointer}/{index}"),
                    depth + 1,
                    max_depth,
                    expanded,
                );
            }
        }
        _ => {}
    }
    expanded.insert(pointer);
}

fn push_rows(
    value: &Value,
    pointer: String,
    path: String,
    label: String,
    depth: usize,
    expanded: &BTreeSet<String>,
    rows: &mut Vec<JsonRow>,
) {
    let expandable = child_count(value) > 0;
    let is_expanded = expandable && expanded.contains(&pointer);
    rows.push(JsonRow {
        pointer: pointer.clone(),
        path: path.clone(),
        depth,
        label,
        kind: kind_of(value),
        value: summary(value).unwrap_or_else(|| scalar_text(value)),
        expandable,
        expanded: is_expanded,
    });
    if !is_expanded {
        return;
    }
    let total = child_count(value);
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter().take(MAX_CHILDREN_SHOWN) {
                push_rows(
                    child,
                    format!("{pointer}/{}", escape_token(key)),
                    child_path(&path, key),
                    key.clone(),
                    depth + 1,
                    expanded,
                    rows,
                );
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate().take(MAX_CHILDREN_SHOWN) {
                push_rows(
                    child,
                    format!("{pointer}/{index}"),
                    format!("{path}[{index}]"),
                    format!("[{index}]"),
                    depth + 1,
                    expanded,
                    rows,
                );
            }
        }
        _ => {}
    }
    if total > MAX_CHILDREN_SHOWN {
        rows.push(JsonRow {
            pointer: format!("{pointer}/…"),
            path: path.clone(),
            depth: depth + 1,
            label: "…".to_owned(),
            kind: NodeKind::More,
            value: format!("{} more not shown", total - MAX_CHILDREN_SHOWN),
            expandable: false,
            expanded: false,
        });
    }
}

fn search_value(
    value: &Value,
    pointer: &str,
    key: Option<&str>,
    query: &str,
    matches: &mut Vec<String>,
    budget: &mut usize,
) {
    if *budget == 0 {
        return;
    }
    *budget -= 1;
    let key_matches = key.is_some_and(|key| key.to_lowercase().contains(query));
    let value_matches = match value {
        Value::String(text) => text.to_lowercase().contains(query),
        Value::Number(_) | Value::Bool(_) | Value::Null => value.to_string().contains(query),
        _ => false,
    };
    if key_matches || value_matches {
        matches.push(pointer.to_owned());
    }
    match value {
        Value::Object(map) => {
            for (child_key, child) in map {
                let child_pointer = format!("{pointer}/{}", escape_token(child_key));
                search_value(
                    child,
                    &child_pointer,
                    Some(child_key),
                    query,
                    matches,
                    budget,
                );
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                search_value(
                    child,
                    &format!("{pointer}/{index}"),
                    None,
                    query,
                    matches,
                    budget,
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        r#"{"data":{"index":33,"name":"Trigger","tags":["a","b"]},"header":{"seq":6},"empty":{}}"#;

    fn doc() -> JsonDoc {
        JsonDoc::parse(SAMPLE.as_bytes()).unwrap()
    }

    #[test]
    fn rows_follow_original_key_order_and_collapse_state() {
        let doc = doc();
        let collapsed = doc.rows(&BTreeSet::new());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].value, "{3 keys}");
        assert!(collapsed[0].expandable && !collapsed[0].expanded);

        let rows = doc.rows(&BTreeSet::from([String::new()]));
        let labels: Vec<_> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["root", "data", "header", "empty"]);
        assert!(!rows[3].expandable);
        assert_eq!(rows[3].value, "{}");
    }

    #[test]
    fn default_expansion_opens_nested_containers_up_to_depth() {
        let doc = doc();
        let expanded = doc.default_expanded(1);
        assert!(
            expanded.contains("") && expanded.contains("/data") && expanded.contains("/header")
        );
        assert!(!expanded.contains("/data/tags"));
        let rows = doc.rows(&expanded);
        assert!(
            rows.iter()
                .any(|row| row.path == "$.data.index" && row.value == "33")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "tags" && row.value == "[2 items]")
        );
    }

    #[test]
    fn paths_and_pointers_resolve_to_values() {
        let doc = doc();
        let expanded = doc.default_expanded(5);
        let rows = doc.rows(&expanded);
        let tag = rows
            .iter()
            .find(|row| row.path == "$.data.tags[1]")
            .unwrap();
        assert_eq!(tag.pointer, "/data/tags/1");
        assert_eq!(copy_text(doc.value_at(&tag.pointer).unwrap()), "b");
        assert!(copy_text(doc.value_at("/data").unwrap()).contains("\"index\": 33"));
    }

    #[test]
    fn search_matches_keys_and_values_and_ancestors_are_listed() {
        let doc = doc();
        assert_eq!(doc.search("TRIGGER"), ["/data/name"]);
        assert_eq!(doc.search("seq"), ["/header/seq"]);
        assert!(doc.search("nothing").is_empty());
        assert_eq!(ancestors("/data/tags/1"), ["", "/data", "/data/tags"]);
        assert!(ancestors("").is_empty());
    }

    #[test]
    fn invalid_json_reports_an_error_and_keys_with_slashes_are_escaped() {
        assert!(JsonDoc::parse(b"{broken").is_err());
        let doc = JsonDoc::parse(br#"{"a/b":{"c d":1}}"#).unwrap();
        let rows = doc.rows(&doc.default_expanded(3));
        assert_eq!(rows[1].pointer, "/a~1b");
        assert_eq!(rows[2].path, "$[\"a/b\"][\"c d\"]");
        assert_eq!(doc.value_at(&rows[2].pointer).unwrap(), 1);
    }

    #[test]
    fn huge_containers_are_capped() {
        let items: Vec<_> = (0..MAX_CHILDREN_SHOWN + 10)
            .map(|n| n.to_string())
            .collect();
        let doc = JsonDoc::parse(format!("[{}]", items.join(",")).as_bytes()).unwrap();
        let rows = doc.rows(&BTreeSet::from([String::new()]));
        assert_eq!(rows.len(), MAX_CHILDREN_SHOWN + 2);
        assert_eq!(rows.last().unwrap().kind, NodeKind::More);
    }
}
