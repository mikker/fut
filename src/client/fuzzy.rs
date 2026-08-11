//! Small, deterministic fuzzy ranking shared by client search surfaces.

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
                score(term, &haystack).map(|score| total + score)
            })?;
            Some((index, score))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(index, score)| (std::cmp::Reverse(*score), *index));
    matches.into_iter().map(|(index, _)| index).collect()
}

fn fold(value: &str) -> Vec<char> {
    value.chars().flat_map(char::to_lowercase).collect()
}

fn score(needle: &[char], haystack: &[char]) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut cursor = 0;
    let mut previous = None;
    let mut total = 0_i64;
    for wanted in needle {
        let offset = haystack[cursor..]
            .iter()
            .position(|candidate| candidate == wanted)?;
        let index = cursor + offset;
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
    Some(total)
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
}
