//! Case-insensitive substring/subsequence matching used by the topic search.

/// Returns the character indices of `text` matched by `query`, or `None` if it does not match.
/// A contiguous case-insensitive substring wins over a scattered subsequence.
pub fn match_positions(query: &str, text: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let query: Vec<char> = query.chars().map(fold).collect();
    let text: Vec<char> = text.chars().map(fold).collect();

    if let Some(start) = text
        .windows(query.len())
        .position(|window| window == query.as_slice())
    {
        return Some((start..start + query.len()).collect());
    }

    let mut positions = Vec::with_capacity(query.len());
    let mut wanted = query.iter().peekable();
    for (index, character) in text.iter().enumerate() {
        if wanted.peek() == Some(&character) {
            positions.push(index);
            wanted.next();
        }
    }
    wanted.peek().is_none().then_some(positions)
}

fn fold(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_matches_are_case_insensitive_and_contiguous() {
        assert_eq!(
            match_positions("trig", "PRESS1_TriggerService"),
            Some(vec![7, 8, 9, 10])
        );
    }

    #[test]
    fn subsequence_matches_scattered_characters() {
        assert_eq!(
            match_positions("psv", "press1/service"),
            Some(vec![0, 3, 10])
        );
        assert_eq!(match_positions("xyz", "press1"), None);
    }

    #[test]
    fn empty_query_matches_everything_without_highlights() {
        assert_eq!(match_positions("", "anything"), Some(Vec::new()));
    }
}
