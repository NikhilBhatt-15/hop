//! Everything's "highlighted" result fields mark matched text by wrapping it in
//! `*`, with `**` standing for a literal asterisk. For example `*note*pad.exe`
//! means "note" matched.

/// Splits highlighted text into `(text, is_match)` runs.
pub fn segments(s: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut on = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            if chars.peek() == Some(&'*') {
                chars.next();
                cur.push('*');
                continue;
            }
            if !cur.is_empty() {
                out.push((std::mem::take(&mut cur), on));
            }
            on = !on;
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push((cur, on));
    }
    out
}

/// Produces Everything-style highlighted text: every case-insensitive
/// occurrence of any term is wrapped in `*`, and literal `*` is doubled.
pub fn mark(text: &str, terms: &[String]) -> String {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let chars: Vec<char> = text.chars().collect();
    // to_lowercase can change length for some scripts; fall back to no marks.
    let mut hit = vec![false; chars.len()];
    if lower.len() == chars.len() {
        for term in terms.iter().filter(|t| !t.is_empty()) {
            let t: Vec<char> = term.to_lowercase().chars().collect();
            let mut i = 0;
            while i + t.len() <= lower.len() {
                if lower[i..i + t.len()] == t[..] {
                    hit[i..i + t.len()].iter_mut().for_each(|h| *h = true);
                    i += t.len();
                } else {
                    i += 1;
                }
            }
        }
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut on = false;
    for (c, h) in chars.iter().zip(hit) {
        if h != on {
            out.push('*');
            on = h;
        }
        if *c == '*' {
            out.push_str("**");
        } else {
            out.push(*c);
        }
    }
    if on {
        out.push('*');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_runs() {
        assert_eq!(
            segments("*note*pad.exe"),
            vec![("note".into(), true), ("pad.exe".into(), false)]
        );
        assert_eq!(segments("a**b"), vec![("a*b".into(), false)]);
        assert_eq!(segments(""), vec![]);
    }

    #[test]
    fn mark_round_trips() {
        let m = mark("a*b Notepad.exe", &["pad".into()]);
        assert_eq!(m, "a**b Note*pad*.exe");
        assert_eq!(
            segments(&m),
            vec![("a*b Note".into(), false), ("pad".into(), true), (".exe".into(), false)]
        );
    }

    #[test]
    fn literal_star_after_match_reads_greedily() {
        // "*pad***" is ambiguous in Everything's format; like the SDK, "**"
        // always wins, so the star lands inside the match.
        assert_eq!(
            segments("Note*pad***.exe"),
            vec![("Note".into(), false), ("pad*".into(), true), (".exe".into(), false)]
        );
    }
}
