use strsim::jaro_winkler;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchCandidate {
    pub key: String,
    pub slug: String,
    pub title: String,
    pub downloads: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CandidateChoice {
    Selected(SearchCandidate),
    Ambiguous(Vec<SearchCandidate>),
    NoMatches,
}

pub fn choose_best_match(query: &str, candidates: &[SearchCandidate]) -> CandidateChoice {
    if candidates.is_empty() {
        return CandidateChoice::NoMatches;
    }

    let normalized_query = normalize(query);
    let compact_query = compact(&normalized_query);
    if normalized_query.is_empty() {
        return CandidateChoice::NoMatches;
    }

    let mut scored: Vec<(SearchCandidate, f64)> = candidates
        .iter()
        .cloned()
        .map(|candidate| {
            let score = score_candidate(&normalized_query, &compact_query, &candidate);
            (candidate, score)
        })
        .collect();

    scored.sort_by(|left, right| right.1.total_cmp(&left.1));

    let Some((top_candidate, top_score)) = scored.first().cloned() else {
        return CandidateChoice::NoMatches;
    };

    if top_score < 85.0 {
        return CandidateChoice::Ambiguous(scored.into_iter().take(5).map(|entry| entry.0).collect());
    }

    let second_score = scored.get(1).map(|entry| entry.1).unwrap_or(f64::NEG_INFINITY);
    if second_score > 95.0 && (top_score - second_score) < 12.0 {
        return CandidateChoice::Ambiguous(scored.into_iter().take(5).map(|entry| entry.0).collect());
    }

    CandidateChoice::Selected(top_candidate)
}

fn score_candidate(query: &str, compact_query: &str, candidate: &SearchCandidate) -> f64 {
    let title = normalize(&candidate.title);
    let slug = normalize(&candidate.slug);
    let compact_title = compact(&title);
    let compact_slug = compact(&slug);
    let query_tokens: Vec<&str> = query.split_whitespace().collect();

    let mut score = 0.0;

    if slug == query {
        score += 220.0;
    }
    if title == query {
        score += 210.0;
    }
    if compact_slug == compact_query {
        score += 190.0;
    }
    if compact_title == compact_query {
        score += 185.0;
    }
    if slug.starts_with(query) {
        score += 70.0;
    }
    if title.starts_with(query) {
        score += 65.0;
    }
    if slug.contains(query) {
        score += 55.0;
    }
    if title.contains(query) {
        score += 50.0;
    }

    let mut missing_tokens = 0usize;
    for token in &query_tokens {
        if compact_title.contains(token) || title.contains(token) {
            score += 18.0;
        } else if compact_slug.contains(token) || slug.contains(token) {
            score += 16.0;
        } else {
            missing_tokens += 1;
        }
    }
    score -= missing_tokens as f64 * 22.0;

    let title_similarity = jaro_winkler(query, &title);
    let slug_similarity = jaro_winkler(query, &slug);
    let compact_title_similarity = jaro_winkler(compact_query, &compact_title);
    let compact_slug_similarity = jaro_winkler(compact_query, &compact_slug);
    score += title_similarity.max(slug_similarity) * 90.0;
    score += compact_title_similarity.max(compact_slug_similarity) * 70.0;

    let popularity = (candidate.downloads as f64 + 1.0).log10() * 2.0;
    score += popularity.min(12.0);

    score
}

fn normalize(input: &str) -> String {
    let mut normalized = String::new();
    let mut last_space = false;

    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_space = false;
        } else if !last_space {
            normalized.push(' ');
            last_space = true;
        }
    }

    normalized.trim().to_string()
}

fn compact(input: &str) -> String {
    input.chars().filter(|ch| !ch.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(key: &str, slug: &str, title: &str, downloads: u64) -> SearchCandidate {
        SearchCandidate {
            key: key.to_string(),
            slug: slug.to_string(),
            title: title.to_string(),
            downloads,
        }
    }

    #[test]
    fn exact_slug_wins() {
        let choice = choose_best_match(
            "sodium",
            &[
                candidate("1", "sodium", "Sodium", 10_000),
                candidate("2", "sodium-extra", "Sodium Extra", 100_000),
            ],
        );

        assert_eq!(
            choice,
            CandidateChoice::Selected(candidate("1", "sodium", "Sodium", 10_000))
        );
    }

    #[test]
    fn fuzzy_query_finds_shulkerboxtooltips() {
        let choice = choose_best_match(
            "shulker tooltips",
            &[
                candidate("1", "shulkerboxtooltips", "ShulkerBox Tooltips", 1000),
                candidate("2", "tooltipfix", "Tooltip Fix", 3000),
            ],
        );

        assert_eq!(
            choice,
            CandidateChoice::Selected(candidate(
                "1",
                "shulkerboxtooltips",
                "ShulkerBox Tooltips",
                1000
            ))
        );
    }
}
