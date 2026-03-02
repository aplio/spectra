pub fn fzf_style_match(haystack: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }

    let haystack_chars: Vec<char> = haystack.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();

    let mut positions = Vec::with_capacity(needle_chars.len());
    let mut hay_idx = 0usize;
    for &needle_ch in &needle_chars {
        let needle_lower = needle_ch.to_lowercase().next().unwrap_or(needle_ch);
        let mut found = false;
        while hay_idx < haystack_chars.len() {
            let hay = haystack_chars[hay_idx];
            let hay_lower = hay.to_lowercase().next().unwrap_or(hay);
            if hay_lower == needle_lower {
                positions.push(hay_idx);
                hay_idx += 1;
                found = true;
                break;
            }
            hay_idx += 1;
        }
        if !found {
            return None;
        }
    }

    let mut score = 0i32;
    for (i, &position) in positions.iter().enumerate() {
        if position == 0 {
            score += 12;
        }
        if position > 0 {
            let prev = haystack_chars[position - 1];
            if prev == ' ' || prev == '_' || prev == '-' || prev == '/' || prev == '.' {
                score += 10;
            }
        }
        if i > 0 {
            let prev = positions[i - 1];
            if position == prev + 1 {
                score += 18;
            } else {
                score -= (position - prev - 1) as i32;
            }
        }
        if haystack_chars[position] == needle_chars[i] {
            score += 4;
        }
    }
    score -= (haystack_chars.len() as i32) / 5;

    Some((score, positions))
}

/// Fuzzy match `haystack` against `needle` (case-insensitive sequential matching).
/// Returns `Some((score, match_positions))` if all needle chars are found in order,
/// or `None` if no match.
pub fn fuzzy_match(haystack: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }

    let haystack_chars: Vec<char> = haystack.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();

    let mut positions = Vec::with_capacity(needle_chars.len());
    let mut hay_idx = 0;

    for &needle_ch in &needle_chars {
        let needle_lower = needle_ch.to_lowercase().next().unwrap_or(needle_ch);
        let mut found = false;
        while hay_idx < haystack_chars.len() {
            let hay_lower = haystack_chars[hay_idx]
                .to_lowercase()
                .next()
                .unwrap_or(haystack_chars[hay_idx]);
            if hay_lower == needle_lower {
                positions.push(hay_idx);
                hay_idx += 1;
                found = true;
                break;
            }
            hay_idx += 1;
        }
        if !found {
            return None;
        }
    }

    let score = compute_score(&haystack_chars, &needle_chars, &positions);
    Some((score, positions))
}

fn compute_score(haystack: &[char], needle: &[char], positions: &[usize]) -> i32 {
    let mut score: i32 = 0;

    for (i, &position) in positions.iter().enumerate() {
        if position == 0 {
            score += 8;
        }

        if position > 0 {
            let prev = haystack[position - 1];
            if prev == ' ' || prev == '_' || prev == '-' || prev == '.' || prev == '/' {
                score += 8;
            }
        }

        if i > 0 && position == positions[i - 1] + 1 {
            score += 12;
        }

        if haystack[position] == needle[i] {
            score += 4;
        }

        if i > 0 {
            let gap = position as i32 - positions[i - 1] as i32 - 1;
            score -= gap;
        }
    }

    score -= (haystack.len() as i32) / 4;

    score
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, fzf_style_match};

    #[test]
    fn fuzzy_match_is_case_insensitive() {
        let result = fuzzy_match("Save File", "sf");
        assert!(result.is_some());
    }

    #[test]
    fn fuzzy_match_returns_none_for_missing_sequence() {
        let result = fuzzy_match("Save File", "xyz");
        assert!(result.is_none());
    }

    #[test]
    fn fzf_style_match_prefers_consecutive_matches() {
        let (consecutive_score, _) = fzf_style_match("abcdef", "abc").expect("consecutive");
        let (sparse_score, _) = fzf_style_match("axbxcxdef", "abc").expect("sparse");
        assert!(consecutive_score > sparse_score);
    }
}
