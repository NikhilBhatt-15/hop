//! Ranks app names against what was typed.
//!
//! Flow Launcher's `StringMatcher` scores a subsequence match and rewards
//! acronym hits and matches that start a word. hop keeps that idea but uses a
//! fixed ladder of tiers, so the ranking is predictable: an exact name beats a
//! prefix, which beats a word start ("code" in "Visual Studio Code"), then an
//! acronym ("vsc"), a plain substring, and last scattered letters ("vscode").

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Char indices into the name that matched, for highlighting.
    pub positions: Vec<usize>,
}

const EXACT: i32 = 1000;
const PREFIX: i32 = 900;
const WORD_START: i32 = 800;
const ALL_WORDS: i32 = 750;
const ACRONYM: i32 = 700;
const SUBSTRING: i32 = 600;
const SCATTERED: i32 = 300;

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Indices where a word starts: the first char, a letter or digit after a
/// non-alphanumeric one, or an uppercase letter after a lowercase one
/// ("OneDrive" has starts at O and D).
fn word_starts(orig: &[char]) -> Vec<bool> {
    (0..orig.len())
        .map(|i| {
            if i == 0 {
                return true;
            }
            let (p, c) = (orig[i - 1], orig[i]);
            (!p.is_alphanumeric() && c.is_alphanumeric()) || (p.is_lowercase() && c.is_uppercase())
        })
        .collect()
}

fn find(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

pub fn score(name: &str, query: &str) -> Option<Match> {
    let orig: Vec<char> = name.chars().collect();
    let n: Vec<char> = orig.iter().map(|&c| lower(c)).collect();
    let q: Vec<char> = query.split_whitespace().collect::<Vec<_>>().join(" ").chars().map(lower).collect();
    if q.is_empty() || n.is_empty() {
        return None;
    }
    let starts = word_starts(&orig);
    let range = |at: usize, len: usize| (at..at + len).collect::<Vec<_>>();

    if n == q {
        return Some(Match { score: EXACT, positions: range(0, q.len()) });
    }
    if n.starts_with(&q) {
        let extra = (n.len() - q.len()) as i32;
        return Some(Match { score: PREFIX - extra.min(90), positions: range(0, q.len()) });
    }
    if let Some(i) = (1..n.len()).find(|&i| starts[i] && n[i..].starts_with(&q)) {
        return Some(Match { score: WORD_START - (i as i32).min(90), positions: range(i, q.len()) });
    }

    // Several words typed: each must start a word of the name, in any order.
    if q.contains(&' ') {
        let mut positions = Vec::new();
        let mut used = vec![false; n.len()];
        let all = q.split(|&c| c == ' ').all(|tok| {
            let hit = (0..n.len()).find(|&i| starts[i] && !used[i] && n[i..].starts_with(tok));
            if let Some(i) = hit {
                used[i] = true;
                positions.extend(range(i, tok.len()));
            }
            hit.is_some()
        });
        if all {
            positions.sort_unstable();
            return Some(Match { score: ALL_WORDS, positions });
        }
        return None;
    }

    // Acronym: every typed letter is the start of a word, in order.
    {
        let mut positions = Vec::new();
        let mut skipped = 0;
        let mut i = 0;
        for &c in &q {
            while i < n.len() && !(starts[i] && n[i] == c) {
                if starts[i] {
                    skipped += 1;
                }
                i += 1;
            }
            if i == n.len() {
                break;
            }
            positions.push(i);
            i += 1;
        }
        if positions.len() == q.len() && q.len() >= 2 {
            return Some(Match { score: ACRONYM - skipped * 10, positions });
        }
    }

    if let Some(i) = find(&n, &q, 0) {
        return Some(Match { score: SUBSTRING - (i as i32).min(90), positions: range(i, q.len()) });
    }

    // Scattered letters in order. Continue a run when the next char matches,
    // otherwise prefer the next word start, so "vscode" picks V, S(tudio),
    // C(ode)... rather than letters buried mid-word.
    let mut positions: Vec<usize> = Vec::with_capacity(q.len());
    let mut from = 0;
    for &c in &q {
        let next = if positions.last().is_some_and(|&p| p + 1 < n.len() && n[p + 1] == c) {
            Some(positions.last().unwrap() + 1)
        } else {
            (from..n.len())
                .find(|&i| starts[i] && n[i] == c)
                .or_else(|| (from..n.len()).find(|&i| n[i] == c))
        };
        let i = next?;
        positions.push(i);
        from = i + 1;
    }
    // Letters strewn from mid-word ("resume" in "SolitaiRE & CaSUal GaMEs")
    // are noise; a scattered match has to begin a word.
    if !starts[positions[0]] {
        return None;
    }
    let at_starts = positions.iter().filter(|&&p| starts[p]).count() as i32;
    let gaps = positions.windows(2).filter(|w| w[1] != w[0] + 1).count() as i32;
    let score = SCATTERED + at_starts * 15 - gaps * 10 - (positions[0] as i32).min(50);
    Some(Match { score: score.clamp(1, SUBSTRING - 100), positions })
}

/// Splits `name` into `(text, matched)` runs from sorted match positions, the
/// same shape `highlight::segments` gives for Everything's results.
pub fn runs(name: &str, positions: &[usize]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for (i, c) in name.chars().enumerate() {
        let hit = positions.binary_search(&i).is_ok();
        match out.last_mut() {
            Some((s, h)) if *h == hit => s.push(c),
            _ => out.push((c.to_string(), hit)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_group_matched_chars() {
        let r = runs("Visual Studio Code", &[0, 7, 14, 15]);
        let r: Vec<(&str, bool)> = r.iter().map(|(s, h)| (s.as_str(), *h)).collect();
        assert_eq!(r, vec![("V", true), ("isual ", false), ("S", true), ("tudio ", false), ("Co", true), ("de", false)]);
        assert!(runs("", &[]).is_empty());
    }

    fn s(name: &str, q: &str) -> i32 {
        score(name, q).map_or(0, |m| m.score)
    }

    #[test]
    fn tiers_rank_in_order() {
        let exact = s("Code", "code");
        let prefix = s("Code Insiders", "code");
        let word = s("Visual Studio Code", "code");
        let acronym = s("Visual Studio Code", "vsc");
        let sub = s("Barcode Reader", "code");
        let scattered = s("Visual Studio Code", "vscode");
        assert!(exact > prefix && prefix > word && word > acronym && acronym > sub && sub > scattered && scattered > 0);
    }

    #[test]
    fn positions_cover_the_match() {
        assert_eq!(score("Visual Studio Code", "code").unwrap().positions, vec![14, 15, 16, 17]);
        assert_eq!(score("Visual Studio Code", "vsc").unwrap().positions, vec![0, 7, 14]);
        assert_eq!(score("Visual Studio Code", "vscode").unwrap().positions, vec![0, 7, 14, 15, 16, 17]);
    }

    #[test]
    fn camel_case_and_multiword() {
        assert!(score("OneDrive", "drive").unwrap().score >= WORD_START - 90);
        assert_eq!(s("Microsoft Edge", "edge micro"), ALL_WORDS);
        assert_eq!(s("Microsoft Edge", "edge zzz"), 0);
    }

    #[test]
    fn scattered_matches_must_start_a_word() {
        assert_eq!(s("Solitaire & Casual Games", "resume"), 0);
        assert!(s("Visual Studio Code", "vscode") > 0);
    }

    #[test]
    fn case_and_spacing_are_ignored() {
        assert_eq!(s("Notepad", "  NOTEPAD "), EXACT);
        assert_eq!(s("Notepad", ""), 0);
        assert_eq!(s("Notepad", "xyz"), 0);
    }

    #[test]
    fn shorter_prefix_matches_rank_higher() {
        assert!(s("Paint", "pa") > s("Paint 3D Preview Edition", "pa"));
    }
}
