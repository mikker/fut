//! Small, deterministic fuzzy ranking shared by client search surfaces.

use std::collections::BTreeSet;

/// Return matching item indices, best match first. Every whitespace-separated
/// query term must match as an ordered, case-insensitive subsequence.
pub(super) fn ranked(query: &str, haystacks: impl IntoIterator<Item = String>) -> Vec<usize> {
    let terms = query.split_whitespace().map(fold).collect::<Vec<_>>();
    let mut matches = haystacks
        .into_iter()
        .enumerate()
        .filter_map(|(index, haystack)| {
            let haystack = fold(&haystack);
            let score = terms.iter().try_fold(0_i64, |total, term| {
                let matched = match_indices(term, &haystack)?;
                Some(total + score(term, &haystack, &matched))
            })?;
            Some((index, score))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(index, score)| (std::cmp::Reverse(*score), *index));
    matches.into_iter().map(|(index, _)| index).collect()
}

/// Original-character indices consumed by the same fuzzy matching rules used
/// for ranking. Each whitespace-separated term starts at the beginning of the
/// haystack, allowing one visual path to show matches across its ancestry.
pub(super) fn matched_char_indices(query: &str, haystack: &str) -> Option<Vec<usize>> {
    let folded_haystack = haystack
        .chars()
        .enumerate()
        .flat_map(|(index, character)| character.to_lowercase().map(move |folded| (folded, index)))
        .collect::<Vec<_>>();
    let folded_characters = folded_haystack
        .iter()
        .map(|(character, _)| *character)
        .collect::<Vec<_>>();
    let mut matched = BTreeSet::new();
    for term in query.split_whitespace().map(fold) {
        for index in match_indices(&term, &folded_characters)? {
            matched.insert(folded_haystack[index].1);
        }
    }
    Some(matched.into_iter().collect())
}

fn fold(value: &str) -> Vec<char> {
    value.chars().flat_map(char::to_lowercase).collect()
}

fn match_indices(needle: &[char], haystack: &[char]) -> Option<Vec<usize>> {
    let mut cursor = 0;
    needle
        .iter()
        .map(|wanted| {
            let offset = haystack[cursor..]
                .iter()
                .position(|candidate| candidate == wanted)?;
            let index = cursor + offset;
            cursor = index + 1;
            Some(index)
        })
        .collect()
}

fn score(needle: &[char], haystack: &[char], matched: &[usize]) -> i64 {
    if needle.is_empty() {
        return 0;
    }
    let mut cursor = 0;
    let mut previous = None;
    let mut total = 0_i64;
    for &index in matched {
        let offset = index - cursor;
        total += 10;
        if previous == Some(index.saturating_sub(1)) {
            total += 12;
        }
        if index == 0 || haystack[index - 1].is_whitespace() || "-_/›".contains(haystack[index - 1])
        {
            total += 8;
        }
        total -= i64::try_from(offset).unwrap_or(i64::MAX);
        previous = Some(index);
        cursor = index + 1;
    }
    total -= i64::try_from(haystack.len().saturating_sub(needle.len())).unwrap_or(i64::MAX) / 4;
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_fuzzy_unicode_multi_token_matches_stably() {
        let values = ["Résumé preview", "resource pane", "Résumé primary", "other"];
        assert_eq!(ranked("rs pri", values.map(str::to_owned)), [2, 0]);
        assert_eq!(ranked("rés", values.map(str::to_owned)), [0, 2]);
        assert!(ranked("rs missing", values.map(str::to_owned)).is_empty());
    }

    #[test]
    fn exposes_matching_source_characters_across_path_segments() {
        assert_eq!(
            matched_char_indices("fuvim", "fut › work › nvim"),
            Some(vec![0, 1, 14, 15, 16])
        );
        assert_eq!(matched_char_indices("nope", "fut › nvim"), None);
    }
}
