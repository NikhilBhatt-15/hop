//! File results: the Everything query hop sends, and how it re-ranks the
//! answer.
//!
//! A launcher wants documents you might open, not the millions of files
//! tools keep for themselves. Sorted by name, the first 20 matches for
//! "code" on a developer machine are `.vscode` folders and gradle docs, and
//! Everything 1.4 has no relevance sort. So hop:
//!
//! 1. leaves out dot-folders, AppData, package caches, Windows and the
//!    Recycle Bin in the query itself,
//! 2. asks for the 100 newest matches,
//! 3. re-ranks them: a good name match (same tiers as apps), files in your
//!    own folders (Desktop, Documents, Downloads...) up, build output and
//!    Program Files down. Ties keep Everything's newest-first order.

use crate::fuzzy;
use crate::ipc::{sort, Item};

/// Everything path terms (they contain `\`, so they match the full path).
const EXCLUDE: &[&str] = &[
    "\\.", // dot-files and dot-folders: .git, .vscode, .cache, .gradle...
    "\\appdata\\",
    "\\node_modules\\",
    "\\site-packages\\",
    "\\$recycle.bin\\",
    ":\\windows\\",
    "\\programdata\\",
];

/// Folders under the user profile where people keep their own files.
const USER_FOLDERS: &[&str] = &["desktop", "documents", "downloads", "pictures", "videos", "music", "onedrive"];

/// Path segments that mark generated files.
const BUILD_DIRS: &[&str] = &["target", "build", "dist", "obj", "bin", "deps", "out", "vendor", "packages"];

pub const SORT: u32 = sort::DATE_MODIFIED_DESCENDING;
/// How many matches to fetch for re-ranking.
pub const FETCH: u32 = 100;
/// How many to show.
pub const SHOW: usize = 20;

pub fn query(typed: &str) -> String {
    let mut q = typed.trim().to_string();
    for term in EXCLUDE {
        q.push_str(" !");
        q.push_str(term);
    }
    q
}

/// Score for one result; higher is better.
pub fn score(name: &str, path: &str, typed: &str, profile: &str) -> i32 {
    // Everything matched every word somewhere in the name, so this is about
    // how well: whole name, prefix, word start... Path-only matches get 300.
    let mut s = fuzzy::score(name, typed).map_or(300, |m| m.score);
    let p = path.to_lowercase();
    let profile = profile.trim_end_matches('\\').to_lowercase();
    let segments: Vec<&str> = p.split('\\').collect();
    if !profile.is_empty() && (p == profile || p.starts_with(&format!("{profile}\\"))) {
        s += 50;
        let first = p[profile.len()..].trim_start_matches('\\').split('\\').next().unwrap_or("");
        if USER_FOLDERS.iter().any(|f| first == *f || first.starts_with("onedrive")) {
            s += 150;
        }
    }
    if segments.iter().any(|seg| seg.starts_with("program files")) {
        s -= 150;
    }
    if segments.iter().any(|seg| BUILD_DIRS.contains(seg)) {
        s -= 150;
    }
    s
}

/// Re-ranks Everything's newest-first results and keeps the best `SHOW`.
pub fn rank(mut items: Vec<Item>, typed: &str, profile: &str) -> Vec<Item> {
    // sort_by_key is stable: equal scores stay newest first.
    items.sort_by_key(|it| -score(&it.name, &it.path, typed, profile));
    items.truncate(SHOW);
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "C:\\Users\\me";

    #[test]
    fn appends_exclusions() {
        let q = query("  budget 2026 ");
        assert!(q.starts_with("budget 2026 !\\. !\\appdata\\"));
        assert!(q.ends_with("!\\programdata\\"));
    }

    #[test]
    fn own_documents_beat_build_output_and_app_assets() {
        let doc = score("resume.pdf", "C:\\Users\\me\\Downloads", "resume", HOME);
        let project = score("resume.pdf", "C:\\Users\\me\\site\\public", "resume", HOME);
        let build = score("resume.pdf", "C:\\Users\\me\\site\\dist", "resume", HOME);
        let asset = score("resumeIcon.svg", "C:\\Program Files\\OneDrive\\images", "resume", HOME);
        assert!(doc > project && project > build && build > asset, "{doc} {project} {build} {asset}");
        assert!(score("x.docx", "C:\\Users\\me\\OneDrive - Work\\Docs", "x", HOME) > project);
    }

    #[test]
    fn rank_is_stable_and_truncates() {
        let item = |name: &str, path: &str| Item { name: name.into(), path: path.into(), ..Default::default() };
        let mut items: Vec<Item> = (0..30).map(|i| item(&format!("report {i}.txt"), "D:\\work")).collect();
        items.insert(5, item("report.txt", "C:\\Users\\me\\Documents"));
        let ranked = rank(items, "report", HOME);
        assert_eq!(ranked.len(), SHOW);
        assert_eq!(ranked[0].name, "report.txt");
        assert_eq!(ranked[1].name, "report 0.txt"); // ties keep the incoming order
        assert_eq!(ranked[2].name, "report 1.txt");
    }
}
