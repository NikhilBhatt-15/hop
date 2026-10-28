//! Remembers what was launched, like Flow Launcher's `UserSelectedRecord`:
//! apps you open often rank higher, and the empty box lists recent ones.
//!
//! Stored as plain text, one launch target per line: `count<TAB>unix
//! seconds<TAB>id`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct History {
    entries: HashMap<String, (u32, u64)>,
    path: Option<PathBuf>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl History {
    /// Loads `%APPDATA%\hop\history.txt`; a missing or unreadable file is
    /// an empty history.
    pub fn load_default() -> History {
        let path = std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("hop").join("history.txt"));
        let mut h = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| History::parse(&s)).unwrap_or_default();
        h.path = path;
        h
    }

    pub fn parse(text: &str) -> History {
        let mut entries = HashMap::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(count), Some(last), Some(id)) = (parts.next(), parts.next(), parts.next()) else { continue };
            if let (Ok(count), Ok(last)) = (count.parse(), last.parse()) {
                if !id.is_empty() {
                    entries.insert(id.to_string(), (count, last));
                }
            }
        }
        History { entries, path: None }
    }

    pub fn serialize(&self) -> String {
        let mut rows: Vec<_> = self.entries.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1 .1));
        rows.iter().map(|(id, (count, last))| format!("{count}\t{last}\t{id}\n")).collect()
    }

    pub fn record(&mut self, id: &str) {
        self.record_at(id, now());
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, self.serialize());
        }
    }

    pub fn record_at(&mut self, id: &str, at: u64) {
        let e = self.entries.entry(id.to_string()).or_insert((0, 0));
        e.0 = e.0.saturating_add(1);
        e.1 = at;
    }

    /// Ranking bonus: grows with launches (capped), plus a little for
    /// anything used in the last three days.
    pub fn boost(&self, id: &str) -> i32 {
        self.boost_at(id, now())
    }

    pub fn boost_at(&self, id: &str, at: u64) -> i32 {
        let Some(&(count, last)) = self.entries.get(id) else { return 0 };
        let recent = if at.saturating_sub(last) < 3 * 86_400 { 20 } else { 0 };
        count.min(10) as i32 * 12 + recent
    }

    /// Launch targets, most recently used first.
    pub fn recent(&self) -> Vec<&str> {
        let mut rows: Vec<_> = self.entries.iter().collect();
        rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then(b.1 .0.cmp(&a.1 .0)));
        rows.into_iter().map(|(id, _)| id.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_ordering() {
        let mut h = History::default();
        h.record_at("chrome", 100);
        h.record_at("code", 300);
        h.record_at("chrome", 200);
        let back = History::parse(&h.serialize());
        assert_eq!(back.recent(), vec!["code", "chrome"]);
        assert_eq!(back.entries["chrome"], (2, 200));
    }

    #[test]
    fn boost_grows_with_use_and_recency() {
        let mut h = History::default();
        let day = 86_400;
        for _ in 0..3 {
            h.record_at("a", 0);
        }
        h.record_at("b", 0);
        assert!(h.boost_at("a", 10 * day) > h.boost_at("b", 10 * day));
        assert!(h.boost_at("b", day) > h.boost_at("b", 10 * day));
        assert_eq!(h.boost_at("zzz", 0), 0);
    }

    #[test]
    fn ignores_malformed_lines() {
        let h = History::parse("x\ty\tz\n3\t5\n4\t9\tok\n");
        assert_eq!(h.recent(), vec!["ok"]);
    }
}
