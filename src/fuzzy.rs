//! A small fzf-style fuzzy matcher.
//!
//! Matches a needle as a subsequence of a haystack and scores the result, so
//! that tight, word-boundary-aligned matches beat scattered ones. Case folding
//! is ASCII-only and smart: an all-lowercase needle matches case-insensitively,
//! a needle with any uppercase character matches exactly.

const SCORE_MATCH: i32 = 16;
// Consecutive outweighs boundary: worktree paths are full of `/`, `-` and `+`,
// so nearly every character is at a boundary and an unbroken run is the real
// signal that a row is what the user typed.
const BONUS_CONSECUTIVE: i32 = 10;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_FIRST: i32 = 8;
const PENALTY_GAP_START: i32 = -5;
const PENALTY_GAP_EXTEND: i32 = -1;
const MAX_GAP_PENALTY: usize = 10;

#[derive(Debug, Clone)]
pub struct Match {
    pub score: i32,
    /// Character (not byte) offsets of the matched characters.
    pub positions: Vec<usize>,
}

/// Where a query matched one worktree, and how well.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    pub score: i32,
    pub branch: Vec<usize>,
    pub path: Vec<usize>,
}

pub fn is_boundary(hay: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = hay[i - 1];
    matches!(prev, '/' | '-' | '_' | '+' | '.' | ' ' | ':' | '#' | '@')
        || (prev.is_lowercase() && hay[i].is_uppercase())
}

/// Score `needle` against `haystack`, or `None` if it is not a subsequence.
pub fn best_match(haystack: &str, needle: &str) -> Option<Match> {
    if needle.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let hay: Vec<char> = haystack.chars().collect();
    let ned: Vec<char> = needle.chars().collect();
    if ned.len() > hay.len() {
        return None;
    }

    let exact_case = ned.iter().any(|c| c.is_uppercase());
    let fold = |c: char| {
        if exact_case {
            c
        } else {
            c.to_ascii_lowercase()
        }
    };
    let hay_cmp: Vec<char> = hay.iter().copied().map(fold).collect();
    let ned_cmp: Vec<char> = ned.iter().copied().map(fold).collect();

    // Leftmost match: establishes that the needle is a subsequence at all.
    let mut forward = Vec::with_capacity(ned.len());
    let mut i = 0;
    for &n in &ned_cmp {
        loop {
            if i >= hay_cmp.len() {
                return None;
            }
            let hit = hay_cmp[i] == n;
            i += 1;
            if hit {
                forward.push(i - 1);
                break;
            }
        }
    }

    // Then pull every character as far right as it can go without passing the
    // end of the forward match. This turns scattered matches into consecutive
    // runs where one exists (`lps` over `.../brollb+lps-993`).
    let mut backward = vec![0usize; ned.len()];
    let mut j = *forward.last().expect("needle is non-empty") as isize;
    for k in (0..ned_cmp.len()).rev() {
        while j >= 0 && hay_cmp[j as usize] != ned_cmp[k] {
            j -= 1;
        }
        debug_assert!(j >= 0, "forward pass proved a match exists");
        backward[k] = j as usize;
        j -= 1;
    }

    let f = score(&hay, &forward);
    let b = score(&hay, &backward);
    Some(if b >= f {
        Match {
            score: b,
            positions: backward,
        }
    } else {
        Match {
            score: f,
            positions: forward,
        }
    })
}

fn score(hay: &[char], positions: &[usize]) -> i32 {
    let mut total = 0;
    let mut prev: Option<usize> = None;
    for (k, &p) in positions.iter().enumerate() {
        total += SCORE_MATCH;
        if k == 0 && p == 0 {
            total += BONUS_FIRST;
        }
        if is_boundary(hay, p) {
            total += BONUS_BOUNDARY;
        }
        if let Some(prev) = prev {
            let gap = p - prev - 1;
            if gap == 0 {
                total += BONUS_CONSECUTIVE;
            } else {
                total +=
                    PENALTY_GAP_START + PENALTY_GAP_EXTEND * (gap.min(MAX_GAP_PENALTY) as i32 - 1);
            }
        }
        prev = Some(p);
    }
    // All else equal, prefer the shorter haystack.
    total - (hay.len() / 8) as i32
}

/// The fields of a worktree a query is matched against.
pub struct Fields<'a> {
    pub branch: &'a str,
    pub path: &'a str,
    pub repo: &'a str,
    pub pr: Option<String>,
}

// Which field a match came from is worth something: a hit on the branch name is
// almost always what the user meant.
const WEIGHT_BRANCH: i32 = 20;
const WEIGHT_PATH: i32 = 0;
const WEIGHT_REPO: i32 = 10;
const WEIGHT_PR: i32 = 15;

/// Match a whitespace-separated query against a worktree. Every token must
/// match at least one field, so tokens narrow the list the way fzf's do.
pub fn match_fields(fields: &Fields, query: &str) -> Option<Hits> {
    let mut hits = Hits::default();
    let mut matched_any_token = false;

    for token in query.split_whitespace() {
        matched_any_token = true;
        let branch = best_match(fields.branch, token);
        let path = best_match(fields.path, token);
        let repo = best_match(fields.repo, token);
        let pr = fields.pr.as_deref().and_then(|pr| best_match(pr, token));

        let best = [
            branch.as_ref().map(|m| m.score + WEIGHT_BRANCH),
            path.as_ref().map(|m| m.score + WEIGHT_PATH),
            repo.as_ref().map(|m| m.score + WEIGHT_REPO),
            pr.as_ref().map(|m| m.score + WEIGHT_PR),
        ]
        .into_iter()
        .flatten()
        .max()?;

        hits.score += best;
        // Highlight the token wherever it appears, not only in the winning
        // field, so the eye can see why a row is in the list.
        if let Some(m) = branch {
            hits.branch.extend(m.positions);
        }
        if let Some(m) = path {
            hits.path.extend(m.positions);
        }
    }

    if !matched_any_token {
        return Some(Hits::default()); // whitespace-only query matches everything
    }
    hits.branch.sort_unstable();
    hits.branch.dedup();
    hits.path.sort_unstable();
    hits.path.dedup();
    Some(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score_of(hay: &str, needle: &str) -> i32 {
        best_match(hay, needle).expect("expected a match").score
    }

    #[test]
    fn requires_a_subsequence() {
        assert!(best_match("brollb/loops-worker", "lw").is_some());
        assert!(best_match("brollb/loops-worker", "wl").is_none());
        assert!(best_match("short", "muchlonger").is_none());
        assert!(best_match("anything", "").unwrap().positions.is_empty());
    }

    #[test]
    fn smart_case() {
        assert!(best_match("brollb/Loops", "loops").is_some());
        assert!(best_match("brollb/loops", "Loops").is_none());
        assert!(best_match("brollb/Loops", "Loops").is_some());
    }

    #[test]
    fn consecutive_beats_scattered() {
        assert!(score_of("loops-broker", "loops") > score_of("l-o-o-p-s-x", "loops"));
    }

    #[test]
    fn word_boundaries_beat_mid_word() {
        // `lb` as the start of two segments beats `lb` inside one word.
        assert!(score_of("loops/broker", "lb") > score_of("albatross", "lb"));
    }

    #[test]
    fn prefers_the_tighter_run_further_right() {
        // The greedy leftmost match would take `l` from `brollb`; the backward
        // pass should find the consecutive `lps` instead.
        let m = best_match("brollb+lps-993-turn-gate", "lps").unwrap();
        assert_eq!(m.positions, vec![7, 8, 9]);
    }

    #[test]
    fn tightens_onto_a_real_branch_name() {
        // `brok` should land on `broker`, not on b-r-o from `brollb` plus a
        // stray `k`.
        let m = best_match("brollb/loops-broker-direct", "brok").unwrap();
        assert_eq!(m.positions, vec![13, 14, 15, 16]);
    }

    #[test]
    fn positions_are_char_offsets_of_the_match() {
        let m = best_match("abcdef", "ace").unwrap();
        assert_eq!(m.positions, vec![0, 2, 4]);
    }

    fn fields<'a>(branch: &'a str, path: &'a str, repo: &'a str, pr: Option<&str>) -> Fields<'a> {
        Fields {
            branch,
            path,
            repo,
            pr: pr.map(str::to_string),
        }
    }

    #[test]
    fn every_token_must_match_some_field() {
        let f = fields(
            "brollb/loops-worker",
            "…/brollb+loops-worker",
            "trainers",
            Some("#837"),
        );
        assert!(match_fields(&f, "loops").is_some());
        assert!(
            match_fields(&f, "trainers loops").is_some(),
            "repo + branch"
        );
        assert!(match_fields(&f, "837").is_some(), "PR number");
        assert!(match_fields(&f, "loops nonexistent").is_none());
    }

    #[test]
    fn branch_matches_outrank_path_matches() {
        let on_branch = fields("brollb/metrics", "…/aaa", "repo", None);
        let on_path = fields("brollb/aaa", "…/metrics", "repo", None);
        assert!(
            match_fields(&on_branch, "metrics").unwrap().score
                > match_fields(&on_path, "metrics").unwrap().score
        );
    }

    #[test]
    fn hits_are_collected_from_every_field_that_matched() {
        let f = fields("loops", "…/loops-x", "repo", None);
        let hits = match_fields(&f, "loops").unwrap();
        assert_eq!(hits.branch, vec![0, 1, 2, 3, 4]);
        assert_eq!(hits.path, vec![2, 3, 4, 5, 6]);
    }

    #[test]
    fn blank_query_matches_everything_with_no_hits() {
        let f = fields("a", "b", "c", None);
        let hits = match_fields(&f, "   ").unwrap();
        assert_eq!(hits.score, 0);
        assert!(hits.branch.is_empty());
    }
}
