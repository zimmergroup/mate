//! Line-based diff of two payload renderings.

/// Larger inputs are refused to keep the quadratic table small.
pub const MAX_DIFF_LINES: usize = 3000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    Removed(String),
    Added(String),
}

pub fn line_diff(old: &str, new: &str) -> Option<Vec<DiffLine>> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();

    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    if old_mid.len() > MAX_DIFF_LINES || new_mid.len() > MAX_DIFF_LINES {
        return None;
    }

    // lcs[i][j] is the longest common subsequence of old_mid[i..] and new_mid[j..].
    let width = new_mid.len() + 1;
    let mut lcs = vec![0u16; (old_mid.len() + 1) * width];
    for i in (0..old_mid.len()).rev() {
        for j in (0..new_mid.len()).rev() {
            lcs[i * width + j] = if old_mid[i] == new_mid[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }

    let mut lines: Vec<DiffLine> = old[..prefix]
        .iter()
        .map(|line| DiffLine::Same((*line).to_owned()))
        .collect();
    let (mut i, mut j) = (0, 0);
    while i < old_mid.len() || j < new_mid.len() {
        if i < old_mid.len() && j < new_mid.len() && old_mid[i] == new_mid[j] {
            lines.push(DiffLine::Same(old_mid[i].to_owned()));
            i += 1;
            j += 1;
        } else if i < old_mid.len()
            && (j == new_mid.len() || lcs[(i + 1) * width + j] >= lcs[i * width + j + 1])
        {
            lines.push(DiffLine::Removed(old_mid[i].to_owned()));
            i += 1;
        } else {
            lines.push(DiffLine::Added(new_mid[j].to_owned()));
            j += 1;
        }
    }
    lines.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|line| DiffLine::Same((*line).to_owned())),
    );
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_marks_changed_lines_and_keeps_context() {
        let diff = line_diff(
            "{\n  \"a\": 1,\n  \"b\": 2\n}",
            "{\n  \"a\": 1,\n  \"b\": 3\n}",
        )
        .unwrap();
        assert_eq!(
            diff,
            vec![
                DiffLine::Same("{".into()),
                DiffLine::Same("  \"a\": 1,".into()),
                DiffLine::Removed("  \"b\": 2".into()),
                DiffLine::Added("  \"b\": 3".into()),
                DiffLine::Same("}".into()),
            ]
        );
    }

    #[test]
    fn diff_handles_insertions_and_identical_input() {
        let diff = line_diff("a\nc", "a\nb\nc").unwrap();
        assert_eq!(diff[1], DiffLine::Added("b".into()));
        assert!(
            line_diff("x", "x")
                .unwrap()
                .iter()
                .all(|line| matches!(line, DiffLine::Same(_)))
        );
    }

    #[test]
    fn oversized_diffs_are_refused() {
        let big_old = "a\n".repeat(MAX_DIFF_LINES + 1);
        let big_new = "b\n".repeat(MAX_DIFF_LINES + 1);
        assert!(line_diff(&big_old, &big_new).is_none());
    }
}
