//! Library half of hop: app enumeration, ranking, the calculator, launch
//! history, and the Everything IPC client shared with evbar.

pub mod apps;
pub mod calc;
pub mod files;
pub mod fuzzy;
pub mod highlight;
pub mod history;
pub mod hotkey;
pub mod ipc;

/// Encodes a Rust string as a NUL-terminated UTF-16 buffer for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
