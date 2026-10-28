//! Client for the Voidtools Everything 1.4 "SDK2" IPC protocol.
//!
//! Everything owns a hidden top-level window of class
//! `EVERYTHING_TASKBAR_NOTIFICATION` (or `EVERYTHING_TASKBAR_NOTIFICATION_(name)`
//! for a named instance such as Everything 1.5's `1.5a`). A client:
//!
//! 1. finds that window with `FindWindowW`,
//! 2. sends it `WM_COPYDATA` with `dwData = COPYDATA_QUERY2W` and an
//!    [`EVERYTHING_IPC_QUERY2`](encode_query2) payload naming a reply window
//!    and a reply id,
//! 3. receives, on the reply window, a `WM_COPYDATA` whose `dwData` equals the
//!    reply id and whose payload is an [`EVERYTHING_IPC_LIST2`](decode_list2).
//!
//! Everything answers from its in-memory index, so a search never touches the
//! disk on either side. Version queries are plain `SendMessage(WM_USER, n)`.
//!
//! All integers on the wire are little-endian `DWORD`s. Strings are UTF-16
//! (for the `W` request types) and NUL-terminated.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_NORMAL,
};

use crate::wide;

pub const WINDOW_CLASS: &str = "EVERYTHING_TASKBAR_NOTIFICATION";
/// Instance name used by Everything 1.5 alpha builds.
pub const INSTANCE_1_5A: &str = "1.5a";

pub const WM_COPYDATA: u32 = 0x004A;
/// `WM_USER`: Everything's command message; `wParam` selects the command.
pub const EVERYTHING_WM_IPC: u32 = 0x0400;
pub const IPC_GET_MAJOR_VERSION: usize = 0;
pub const IPC_GET_MINOR_VERSION: usize = 1;
pub const IPC_GET_REVISION: usize = 2;

/// `COPYDATASTRUCT.dwData` values for queries.
pub const COPYDATA_QUERYW: usize = 2;
pub const COPYDATA_QUERY2W: usize = 18;

/// `EVERYTHING_IPC_QUERY2.search_flags`.
pub mod search {
    pub const MATCH_CASE: u32 = 0x1;
    pub const MATCH_WHOLE_WORD: u32 = 0x2;
    pub const MATCH_PATH: u32 = 0x4;
    pub const REGEX: u32 = 0x8;
}

/// `EVERYTHING_IPC_QUERY2.request_flags`. Reply item data is laid out in
/// ascending bit order of the flags present in the reply's `request_flags`.
pub mod request {
    pub const FILE_NAME: u32 = 0x0001;
    pub const PATH: u32 = 0x0002;
    pub const FULL_PATH_AND_FILE_NAME: u32 = 0x0004;
    pub const EXTENSION: u32 = 0x0008;
    pub const SIZE: u32 = 0x0010;
    pub const DATE_CREATED: u32 = 0x0020;
    pub const DATE_MODIFIED: u32 = 0x0040;
    pub const DATE_ACCESSED: u32 = 0x0080;
    pub const ATTRIBUTES: u32 = 0x0100;
    pub const FILE_LIST_FILE_NAME: u32 = 0x0200;
    pub const RUN_COUNT: u32 = 0x0400;
    pub const DATE_RUN: u32 = 0x0800;
    pub const DATE_RECENTLY_CHANGED: u32 = 0x1000;
    pub const HIGHLIGHTED_FILE_NAME: u32 = 0x2000;
    pub const HIGHLIGHTED_PATH: u32 = 0x4000;
    pub const HIGHLIGHTED_FULL_PATH_AND_FILE_NAME: u32 = 0x8000;
}

/// A subset of `EVERYTHING_SORT_*`.
pub mod sort {
    pub const NAME_ASCENDING: u32 = 1;
    pub const PATH_ASCENDING: u32 = 3;
    pub const SIZE_DESCENDING: u32 = 6;
    pub const DATE_MODIFIED_DESCENDING: u32 = 14;
    pub const RUN_COUNT_DESCENDING: u32 = 20;
}

/// `EVERYTHING_IPC_ITEM2.flags`.
pub const ITEM_FOLDER: u32 = 0x1;
pub const ITEM_DRIVE: u32 = 0x2;

/// `sizeof(EVERYTHING_IPC_QUERY2)` without the trailing search string.
pub const QUERY2_HEADER_LEN: usize = 28;
/// `sizeof(EVERYTHING_IPC_LIST2)` without the item array.
pub const LIST2_HEADER_LEN: usize = 20;
/// `sizeof(EVERYTHING_IPC_ITEM2)`.
pub const ITEM2_LEN: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub search_flags: u32,
    pub offset: u32,
    pub max_results: u32,
    pub request_flags: u32,
    pub sort: u32,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Query {
            text: text.into(),
            search_flags: 0,
            offset: 0,
            max_results: 100,
            request_flags: request::FILE_NAME | request::PATH,
            sort: sort::NAME_ASCENDING,
        }
    }
}

/// Encodes an `EVERYTHING_IPC_QUERY2`:
///
/// ```text
/// +0x00 DWORD reply_hwnd             window that receives the WM_COPYDATA reply
/// +0x04 DWORD reply_copydata_message dwData of the reply (the caller's reply id)
/// +0x08 DWORD search_flags           search::*
/// +0x0C DWORD offset                 first result index
/// +0x10 DWORD max_results
/// +0x14 DWORD request_flags          request::*
/// +0x18 DWORD sort_type              sort::*
/// +0x1C WCHAR search_string[]        NUL-terminated
/// ```
pub fn encode_query2(reply_hwnd: u32, reply_id: u32, q: &Query) -> Vec<u8> {
    let text: Vec<u16> = q.text.encode_utf16().collect();
    let mut v = Vec::with_capacity(QUERY2_HEADER_LEN + (text.len() + 1) * 2);
    for d in [
        reply_hwnd,
        reply_id,
        q.search_flags,
        q.offset,
        q.max_results,
        q.request_flags,
        q.sort,
    ] {
        v.extend_from_slice(&d.to_le_bytes());
    }
    for c in text.iter().chain(std::iter::once(&0)) {
        v.extend_from_slice(&c.to_le_bytes());
    }
    v
}

/// Decodes an `EVERYTHING_IPC_QUERY2` into `(reply_hwnd, reply_id, query)`.
pub fn decode_query2(buf: &[u8]) -> Option<(u32, u32, Query)> {
    let r = Reader(buf);
    let text = r.wstr_nul(QUERY2_HEADER_LEN)?;
    Some((
        r.u32(0)?,
        r.u32(4)?,
        Query {
            text,
            search_flags: r.u32(8)?,
            offset: r.u32(12)?,
            max_results: r.u32(16)?,
            request_flags: r.u32(20)?,
            sort: r.u32(24)?,
        },
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub flags: u32,
    pub name: String,
    pub path: String,
    pub full_path: Option<String>,
    pub highlighted_name: Option<String>,
    pub highlighted_path: Option<String>,
    pub size: Option<u64>,
    pub date_modified: Option<u64>,
}

impl Item {
    pub fn is_folder(&self) -> bool {
        self.flags & (ITEM_FOLDER | ITEM_DRIVE) != 0
    }

    /// Joins path and name the way the SDK does: one separator, unless the
    /// path already ends with one. Drives (`C:` with no path) get a trailing `\`.
    pub fn full_path(&self) -> String {
        if let Some(p) = &self.full_path {
            return p.clone();
        }
        if self.path.is_empty() {
            return if self.name.ends_with(':') {
                format!("{}\\", self.name)
            } else {
                self.name.clone()
            };
        }
        if self.path.ends_with('\\') {
            format!("{}{}", self.path, self.name)
        } else {
            format!("{}\\{}", self.path, self.name)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultList {
    pub total: u32,
    pub offset: u32,
    pub request_flags: u32,
    pub sort: u32,
    pub items: Vec<Item>,
}

fn is_string_field(flag: u32) -> bool {
    use request::*;
    matches!(
        flag,
        FILE_NAME
            | PATH
            | FULL_PATH_AND_FILE_NAME
            | EXTENSION
            | FILE_LIST_FILE_NAME
            | HIGHLIGHTED_FILE_NAME
            | HIGHLIGHTED_PATH
            | HIGHLIGHTED_FULL_PATH_AND_FILE_NAME
    )
}

fn fixed_field_len(flag: u32) -> usize {
    use request::*;
    match flag {
        ATTRIBUTES | RUN_COUNT => 4,
        _ => 8, // SIZE (LARGE_INTEGER) and every DATE_* (FILETIME)
    }
}

/// Decodes an `EVERYTHING_IPC_LIST2` reply:
///
/// ```text
/// +0x00 DWORD totitems       matches in the whole index
/// +0x04 DWORD numitems       items in this reply
/// +0x08 DWORD offset
/// +0x0C DWORD request_flags  fields actually present per item
/// +0x10 DWORD sort_type
/// +0x14 EVERYTHING_IPC_ITEM2 items[numitems] { DWORD flags; DWORD data_offset; }
///       ...item data, addressed by data_offset from the start of the list:
///       for each request flag in ascending bit order,
///         strings:      DWORD len; WCHAR text[len]; WCHAR nul;
///         size/dates:   8 bytes
///         attrs/count:  4 bytes
/// ```
///
/// Returns `None` instead of panicking on truncated or inconsistent data.
pub fn decode_list2(buf: &[u8]) -> Option<ResultList> {
    let r = Reader(buf);
    let total = r.u32(0)?;
    let num = r.u32(4)? as usize;
    let offset = r.u32(8)?;
    let request_flags = r.u32(12)?;
    let sort = r.u32(16)?;
    if LIST2_HEADER_LEN.checked_add(num.checked_mul(ITEM2_LEN)?)? > buf.len() {
        return None;
    }
    let mut items = Vec::with_capacity(num);
    for i in 0..num {
        let base = LIST2_HEADER_LEN + i * ITEM2_LEN;
        let mut item = Item {
            flags: r.u32(base)?,
            ..Default::default()
        };
        let mut p = r.u32(base + 4)? as usize;
        for bit in 0..32 {
            let flag = 1u32 << bit;
            if request_flags & flag == 0 {
                continue;
            }
            if is_string_field(flag) {
                let len = r.u32(p)? as usize;
                let s = r.wstr(p + 4, len)?;
                p = p.checked_add(4 + len.checked_add(1)?.checked_mul(2)?)?;
                if p > buf.len() {
                    return None; // terminator missing
                }
                match flag {
                    request::FILE_NAME => item.name = s,
                    request::PATH => item.path = s,
                    request::FULL_PATH_AND_FILE_NAME => item.full_path = Some(s),
                    request::HIGHLIGHTED_FILE_NAME => item.highlighted_name = Some(s),
                    request::HIGHLIGHTED_PATH => item.highlighted_path = Some(s),
                    _ => {}
                }
            } else {
                let n = fixed_field_len(flag);
                match flag {
                    request::SIZE => item.size = Some(r.u64(p)?),
                    request::DATE_MODIFIED => item.date_modified = Some(r.u64(p)?),
                    _ => {
                        r.0.get(p..p + n)?;
                    }
                }
                p += n;
            }
        }
        items.push(item);
    }
    Some(ResultList {
        total,
        offset,
        request_flags,
        sort,
        items,
    })
}

/// Encodes an `EVERYTHING_IPC_LIST2`, the inverse of [`decode_list2`]. Only
/// the fields [`Item`] carries are written; other requested flags get zeroed
/// or empty values so the layout stays valid.
pub fn encode_list2(total: u32, offset: u32, request_flags: u32, sort: u32, items: &[Item]) -> Vec<u8> {
    let mut head = Vec::new();
    for d in [total, items.len() as u32, offset, request_flags, sort] {
        head.extend_from_slice(&d.to_le_bytes());
    }
    let data_start = LIST2_HEADER_LEN + items.len() * ITEM2_LEN;
    let mut data = Vec::new();
    for item in items {
        head.extend_from_slice(&item.flags.to_le_bytes());
        head.extend_from_slice(&((data_start + data.len()) as u32).to_le_bytes());
        for bit in 0..32 {
            let flag = 1u32 << bit;
            if request_flags & flag == 0 {
                continue;
            }
            if is_string_field(flag) {
                let full = item.full_path();
                let s: &str = match flag {
                    request::FILE_NAME => &item.name,
                    request::PATH => &item.path,
                    request::FULL_PATH_AND_FILE_NAME => &full,
                    request::HIGHLIGHTED_FILE_NAME => item.highlighted_name.as_deref().unwrap_or(&item.name),
                    request::HIGHLIGHTED_PATH => item.highlighted_path.as_deref().unwrap_or(&item.path),
                    _ => "",
                };
                let w: Vec<u16> = s.encode_utf16().collect();
                data.extend_from_slice(&(w.len() as u32).to_le_bytes());
                for c in w.iter().chain(std::iter::once(&0)) {
                    data.extend_from_slice(&c.to_le_bytes());
                }
            } else {
                match flag {
                    request::SIZE => data.extend_from_slice(&item.size.unwrap_or(0).to_le_bytes()),
                    request::DATE_MODIFIED => {
                        data.extend_from_slice(&item.date_modified.unwrap_or(0).to_le_bytes())
                    }
                    _ => data.extend(std::iter::repeat(0).take(fixed_field_len(flag))),
                }
            }
        }
    }
    head.extend_from_slice(&data);
    head
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn u32(&self, at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.0.get(at..at.checked_add(4)?)?.try_into().ok()?))
    }

    fn u64(&self, at: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.0.get(at..at.checked_add(8)?)?.try_into().ok()?))
    }

    fn wstr(&self, at: usize, len: usize) -> Option<String> {
        let bytes = self.0.get(at..at.checked_add(len.checked_mul(2)?)?)?;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }

    fn wstr_nul(&self, at: usize) -> Option<String> {
        let tail = self.0.get(at..)?;
        let units: Vec<u16> = tail
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// `EVERYTHING_TASKBAR_NOTIFICATION` or `EVERYTHING_TASKBAR_NOTIFICATION_(name)`.
pub fn window_class(instance: Option<&str>) -> String {
    match instance {
        Some(name) if !name.is_empty() => format!("{WINDOW_CLASS}_({name})"),
        _ => WINDOW_CLASS.to_string(),
    }
}

pub fn find_window(instance: Option<&str>) -> HWND {
    let class = wide(&window_class(instance));
    unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }
}

/// Finds the IPC window of an explicitly named instance, or else the default
/// Everything 1.4 instance, or else the Everything 1.5 alpha instance.
pub fn find_any(instance: Option<&str>) -> HWND {
    if instance.is_some() {
        return find_window(instance);
    }
    let hwnd = find_window(None);
    if !hwnd.is_null() {
        return hwnd;
    }
    find_window(Some(INSTANCE_1_5A))
}

/// Sends `WM_COPYDATA` and reports whether the receiver accepted it. Uses a
/// timeout so a hung Everything cannot hang the caller.
///
/// # Safety
/// `target` and `sender` must be window handles (or null for `sender`).
pub unsafe fn send_copydata(target: HWND, sender: HWND, dw_data: usize, payload: &[u8], timeout_ms: u32) -> bool {
    let cds = COPYDATASTRUCT {
        dwData: dw_data,
        cbData: payload.len() as u32,
        lpData: payload.as_ptr() as *mut c_void,
    };
    let mut result: usize = 0;
    let ok = SendMessageTimeoutW(
        target,
        WM_COPYDATA,
        sender as usize,
        &cds as *const COPYDATASTRUCT as isize,
        SMTO_ABORTIFHUNG | SMTO_NORMAL,
        timeout_ms,
        &mut result,
    );
    ok != 0 && result != 0
}

/// Sends the query; the reply arrives later on `reply_hwnd` as `WM_COPYDATA`
/// with `dwData == reply_id`.
///
/// # Safety
/// `target` and `reply_hwnd` must be window handles.
pub unsafe fn send_query(target: HWND, reply_hwnd: HWND, reply_id: u32, q: &Query) -> bool {
    let payload = encode_query2(reply_hwnd as usize as u32, reply_id, q);
    send_copydata(target, reply_hwnd, COPYDATA_QUERY2W, &payload, 3000)
}

/// Reads `(major, minor, revision)` with `SendMessage(WM_USER, n, 0)`.
///
/// # Safety
/// `target` must be a window handle.
pub unsafe fn version(target: HWND) -> Option<(u32, u32, u32)> {
    let ask = |cmd: usize| -> Option<u32> {
        let mut result: usize = 0;
        let ok = SendMessageTimeoutW(
            target,
            EVERYTHING_WM_IPC,
            cmd,
            0,
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            1000,
            &mut result,
        );
        (ok != 0).then_some(result as u32)
    };
    Some((
        ask(IPC_GET_MAJOR_VERSION)?,
        ask(IPC_GET_MINOR_VERSION)?,
        ask(IPC_GET_REVISION)?,
    ))
}

/// If a `WM_COPYDATA` `lParam` is a reply with id `reply_id`, returns its
/// payload. Mirrors the SDK's `Everything_IsQueryReply` check.
///
/// # Safety
/// `lparam` must be the `lParam` of a `WM_COPYDATA` currently being handled.
pub unsafe fn reply_payload<'a>(msg: u32, lparam: isize, reply_id: u32) -> Option<&'a [u8]> {
    if msg != WM_COPYDATA || lparam == 0 {
        return None;
    }
    let cds = &*(lparam as *const COPYDATASTRUCT);
    if cds.dwData != reply_id as usize || cds.lpData.is_null() {
        return None;
    }
    Some(std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query2_layout() {
        let mut q = Query::new("ab");
        q.search_flags = search::MATCH_PATH;
        q.max_results = 50;
        q.request_flags = request::FILE_NAME | request::PATH;
        let b = encode_query2(0x1234, 7, &q);
        assert_eq!(b.len(), QUERY2_HEADER_LEN + 6);
        assert_eq!(&b[0..4], &0x1234u32.to_le_bytes());
        assert_eq!(&b[4..8], &7u32.to_le_bytes());
        assert_eq!(&b[8..12], &4u32.to_le_bytes());
        assert_eq!(&b[16..20], &50u32.to_le_bytes());
        assert_eq!(&b[20..24], &3u32.to_le_bytes());
        assert_eq!(&b[24..28], &sort::NAME_ASCENDING.to_le_bytes());
        assert_eq!(&b[28..], &[b'a', 0, b'b', 0, 0, 0]);
        assert_eq!(decode_query2(&b), Some((0x1234, 7, q)));
    }

    #[test]
    fn list2_round_trip() {
        let items = vec![
            Item {
                flags: 0,
                name: "notepad.exe".into(),
                path: "C:\\Windows".into(),
                highlighted_name: Some("*note*pad.exe".into()),
                size: Some(201_216),
                date_modified: Some(133_000_000_000_000_000),
                ..Default::default()
            },
            Item {
                flags: ITEM_FOLDER,
                name: "Notes".into(),
                path: "D:\\".into(),
                highlighted_name: Some("*Note*s".into()),
                size: Some(0),
                date_modified: Some(0),
                ..Default::default()
            },
        ];
        // ATTRIBUTES and DATE_CREATED are requested but not modelled: they
        // must still be skipped correctly.
        let flags = request::FILE_NAME
            | request::PATH
            | request::SIZE
            | request::DATE_CREATED
            | request::DATE_MODIFIED
            | request::ATTRIBUTES
            | request::HIGHLIGHTED_FILE_NAME;
        let buf = encode_list2(1234, 0, flags, sort::NAME_ASCENDING, &items);
        let list = decode_list2(&buf).unwrap();
        assert_eq!(list.total, 1234);
        assert_eq!(list.request_flags, flags);
        assert_eq!(list.items, items);
        assert!(!list.items[0].is_folder());
        assert!(list.items[1].is_folder());
        assert_eq!(list.items[0].full_path(), "C:\\Windows\\notepad.exe");
        assert_eq!(list.items[1].full_path(), "D:\\Notes");
    }

    #[test]
    fn list2_rejects_truncated_input() {
        let items = vec![Item {
            name: "a".into(),
            path: "b".into(),
            ..Default::default()
        }];
        let buf = encode_list2(1, 0, request::FILE_NAME | request::PATH, 1, &items);
        for cut in 0..buf.len() {
            assert!(decode_list2(&buf[..cut]).is_none(), "cut at {cut}");
        }
        // numitems far larger than the buffer must not allocate or panic.
        let mut huge = buf.clone();
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_list2(&huge).is_none());
    }

    #[test]
    fn instance_class_names() {
        assert_eq!(window_class(None), "EVERYTHING_TASKBAR_NOTIFICATION");
        assert_eq!(window_class(Some("1.5a")), "EVERYTHING_TASKBAR_NOTIFICATION_(1.5a)");
    }
}
