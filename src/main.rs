//! hop: a keyboard launcher for Windows. Press the hotkey, type, press Enter.
//! One box finds apps (the shell's Applications folder), files (Voidtools
//! Everything over WM_COPYDATA), does arithmetic, runs commands (`> ...`)
//! and searches the web.
//!
//! Threads:
//! - UI thread: owns the window, ranks apps on every keystroke (a few hundred
//!   names, well under a millisecond), and receives Everything's replies
//!   (WM_COPYDATA) directly.
//! - IPC worker: sends file queries to Everything. It always sends the newest
//!   pending query and drops older ones, so typing never queues up work.
//! - Indexer: lists the Applications folder at startup and again when the box
//!   opens if the list is more than two minutes old.

#![windows_subsystem = "windows"]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hop::apps::{self, App as AppEntry};
use hop::history::History;
use hop::ipc::{self, request, Item, Query};
use hop::{calc, files, fuzzy, highlight, hotkey, wide};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows_sys::Win32::UI::Controls::{ImageList_Draw, ILD_TRANSPARENT};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, SHGetFileInfoW, SetWindowSubclass, ShellExecuteW, SHFILEINFOW, SHGFI_LARGEICON, SHGFI_SYSICONINDEX,
    SHGFI_USEFILEATTRIBUTES,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "HopLauncher";
const MUTEX_NAME: &str = "Local\\HopSingleInstance";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

const WM_APP_NOT_RUNNING: u32 = WM_APP + 1;
/// lParam: `Box<Vec<AppEntry>>` from the indexer thread.
const WM_APP_APPS: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;
const ID_EDIT: usize = 100;
const ID_MENU_EXIT: usize = 1;
const ID_MENU_STARTUP: usize = 2;
const ID_MENU_REINDEX: usize = 3;
const RENDER_TIMER: usize = 1;

/// Apps shown for a query, and recent apps shown in the empty box.
const MAX_APPS: usize = 6;
const MAX_RECENT: usize = 6;
const VISIBLE_ROWS: i32 = 8;
const REINDEX_AFTER: Duration = Duration::from_secs(120);
const FILE_FLAGS: u32 = request::FILE_NAME | request::PATH | request::HIGHLIGHTED_FILE_NAME;

// Layout in 96-DPI pixels.
const WIDTH: i32 = 640;
const BOX_H: i32 = 56;
const ROW_H: i32 = 52;
const PAD: i32 = 6;
const FOOTER_H: i32 = 30;
const ICON: i32 = 32;

const EM_SETSEL: u32 = 0x00B1;
const EM_GETSEL: u32 = 0x00B0;
const EM_REPLACESEL: u32 = 0x00C2;
const EM_SETMARGINS: u32 = 0x00D3;
const EM_SETCUEBANNER: u32 = 0x1501;
const EN_CHANGE: u32 = 0x0300;
const DWMWCP_ROUND: i32 = 2;
const WM_MOUSELEAVE: u32 = 0x02A3;
const CF_UNICODETEXT: u32 = 13;

// Segoe MDL2 Assets glyphs.
const GLYPH_SEARCH: u16 = 0xE721;
const GLYPH_CALC: u16 = 0xE8EF;
const GLYPH_COMMAND: u16 = 0xE756;
const GLYPH_GLOBE: u16 = 0xE774;
const GLYPH_APP: u16 = 0xE71D;

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[derive(Clone, Copy)]
struct Theme {
    bg: COLORREF,
    border: COLORREF,
    text: COLORREF,
    dim: COLORREF,
    sel: COLORREF,
    accent: COLORREF,
}

impl Theme {
    fn current(force_light: Option<bool>) -> Theme {
        if force_light.unwrap_or_else(system_uses_light_theme) {
            Theme {
                bg: rgb(251, 251, 251),
                border: rgb(214, 214, 214),
                text: rgb(26, 26, 26),
                dim: rgb(96, 96, 96),
                sel: rgb(234, 234, 234),
                accent: rgb(0, 95, 184),
            }
        } else {
            Theme {
                bg: rgb(43, 43, 43),
                border: rgb(70, 70, 70),
                text: rgb(242, 242, 242),
                dim: rgb(166, 166, 166),
                sel: rgb(61, 61, 61),
                accent: rgb(96, 205, 255),
            }
        }
    }
}

fn system_uses_light_theme() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut len = size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut len,
        )
    };
    rc == 0 && data != 0
}

/// The single-slot mailbox between the UI thread and the IPC worker.
struct Worker {
    /// (reply id, query text)
    slot: Mutex<Option<(u32, String)>>,
    wake: Condvar,
}

#[derive(PartialEq, Clone, Copy)]
enum FileStatus {
    /// No file search for this query (too short, a calculation, a command).
    Off,
    Pending,
    Ready,
    NotRunning,
}

enum Row {
    Calc(f64),
    App { idx: usize, positions: Vec<usize> },
    File(usize),
    Command(String),
    Web(String),
}

struct Fonts {
    edit: HFONT,
    name: HFONT,
    name_bold: HFONT,
    small: HFONT,
    glyph: HFONT,
    glyph_big: HFONT,
}

struct App {
    hwnd: HWND,
    edit: HWND,
    dpi: u32,
    theme: Theme,
    bg_brush: HBRUSH,
    fonts: Option<Fonts>,
    apps: Vec<AppEntry>,
    indexed_at: Option<Instant>,
    indexing: bool,
    history: History,
    query: String,
    rows: Vec<Row>,
    files: Vec<Item>,
    file_status: FileStatus,
    sel: i32,
    scroll: i32,
    hover: i32,
    tracking_mouse: bool,
    latest_id: u32,
    worker: Arc<Worker>,
    /// App icons by id, at the current DPI.
    app_icons: HashMap<String, HBITMAP>,
    image_list: isize,
    ext_icons: HashMap<String, i32>,
    hotkey: Option<String>,
    force_light: Option<bool>,
    render: Option<RenderJob>,
}

/// `--render <query> <out.bmp>`: index apps, run one query with the window
/// hidden, paint it offscreen, write a bitmap and a text summary, exit.
struct RenderJob {
    query: String,
    out: String,
}

struct Options {
    instance: Option<String>,
    force_light: Option<bool>,
    render: Option<RenderJob>,
    hotkey: String,
    start_hidden: bool,
}

thread_local! {
    static APP: Cell<*mut App> = const { Cell::new(null_mut()) };
}

/// The UI state, owned by the UI thread for the life of the process.
fn app() -> &'static mut App {
    unsafe { &mut *APP.with(|a| a.get()) }
}

fn scale(v: i32) -> i32 {
    v * app().dpi as i32 / 96
}

fn main() {
    let opts = parse_args();
    unsafe {
        CreateMutexW(null(), 0, wide(MUTEX_NAME).as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS && opts.render.is_none() {
            // Second launch: open the running instance instead.
            let existing = FindWindowW(wide(CLASS).as_ptr(), null());
            if !existing.is_null() {
                AllowSetForegroundWindow(ASFW_ANY);
                PostMessageW(existing, WM_HOTKEY, HOTKEY_ID as usize, 0);
            }
            return;
        }
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        run(opts);
    }
}

fn parse_args() -> Options {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str, n: usize| args.iter().position(|a| a == flag).and_then(|i| args.get(i + n).cloned());
    Options {
        instance: value("--instance", 1),
        force_light: value("--theme", 1).map(|t| t == "light"),
        hotkey: value("--hotkey", 1).unwrap_or_else(|| hotkey::DEFAULT.to_string()),
        start_hidden: args.iter().any(|a| a == "--hidden"),
        render: value("--render", 1).zip(value("--render", 2)).map(|(query, out)| RenderJob { query, out }),
    }
}

unsafe fn run(opts: Options) {
    let worker = Arc::new(Worker { slot: Mutex::new(None), wake: Condvar::new() });
    let state = Box::new(App {
        hwnd: null_mut(),
        edit: null_mut(),
        dpi: 96,
        theme: Theme::current(opts.force_light),
        bg_brush: null_mut(),
        fonts: None,
        apps: Vec::new(),
        indexed_at: None,
        indexing: false,
        history: if opts.render.is_some() { History::default() } else { History::load_default() },
        query: String::new(),
        rows: Vec::new(),
        files: Vec::new(),
        file_status: FileStatus::Off,
        sel: 0,
        scroll: 0,
        hover: -1,
        tracking_mouse: false,
        latest_id: 0,
        worker: worker.clone(),
        app_icons: HashMap::new(),
        image_list: 0,
        ext_icons: HashMap::new(),
        hotkey: None,
        force_light: opts.force_light,
        render: opts.render,
    });
    APP.with(|a| a.set(Box::into_raw(state)));
    let a = app();
    a.bg_brush = CreateSolidBrush(a.theme.bg);

    let hinst = GetModuleHandleW(null());
    let class = wide(CLASS);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: class.as_ptr(),
        ..zeroed()
    };
    RegisterClassExW(&wc);
    a.hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class.as_ptr(),
        wide("hop").as_ptr(),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        scale(WIDTH),
        scale(BOX_H),
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    a.edit = CreateWindowExW(
        0,
        wide("EDIT").as_ptr(),
        null(),
        WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
        0,
        0,
        10,
        10,
        a.hwnd,
        ID_EDIT as HMENU,
        hinst,
        null(),
    );
    SetWindowSubclass(a.edit, Some(edit_proc), 1, 0);
    SendMessageW(a.edit, EM_SETCUEBANNER, 1, wide("Search apps and files, or type 2+2").as_ptr() as isize);
    // If hop runs elevated and Everything does not, let its replies in.
    ChangeWindowMessageFilterEx(a.hwnd, ipc::WM_COPYDATA, MSGFLT_ALLOW, null_mut());
    place_window();

    let reply_hwnd = a.hwnd as usize;
    let instance = opts.instance;
    std::thread::spawn(move || ipc_worker(worker, reply_hwnd, instance));

    if let Some(job) = &a.render {
        // Index synchronously so the picture shows real apps.
        a.apps = apps::enumerate();
        a.indexed_at = Some(Instant::now());
        SetTimer(a.hwnd, RENDER_TIMER, 1500, None);
        let q = job.query.clone();
        SetWindowTextW(a.edit, wide(&q).as_ptr()); // EN_CHANGE runs the query
        if a.file_status != FileStatus::Pending {
            render_and_quit();
        }
    } else {
        reindex();
        register_hotkey(&opts.hotkey);
        // Pop up once on a manual launch so it is obvious hop started;
        // --hidden (for autostart) skips that.
        if !opts.start_hidden {
            activate();
        }
    }

    let mut msg: MSG = zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Registers the global hotkey. Another app may already own the combination
/// (RegisterHotKey is first come, first served), so say so instead of
/// failing silently.
unsafe fn register_hotkey(spec: &str) {
    let a = app();
    let problem = match hotkey::parse(spec) {
        Err(e) => Some(format!("The hotkey {e}.")),
        Ok(hk) => {
            if RegisterHotKey(a.hwnd, HOTKEY_ID, hk.modifiers | MOD_NOREPEAT, hk.vk) != 0 {
                a.hotkey = Some(spec.to_string());
                None
            } else {
                Some(format!(
                    "{spec} is already used by another app (PowerToys Run and Flow Launcher use Alt+Space), so hop can't use it."
                ))
            }
        }
    };
    if let Some(problem) = problem {
        let text = format!(
            "{problem}\n\nRun hop.exe again to open it, or restart it with a different hotkey, e.g.\n\n    hop.exe --hotkey Ctrl+Space"
        );
        MessageBoxW(a.hwnd, wide(&text).as_ptr(), wide("hop").as_ptr(), MB_OK | MB_ICONWARNING);
    }
}

/// Lists the Applications folder on a background thread; the result comes
/// back as WM_APP_APPS.
unsafe fn reindex() {
    let a = app();
    if a.indexing {
        return;
    }
    a.indexing = true;
    let hwnd = a.hwnd as usize;
    std::thread::spawn(move || {
        unsafe { CoInitializeEx(null(), COINIT_MULTITHREADED as u32) };
        let list = Box::new(apps::enumerate());
        unsafe { PostMessageW(hwnd as HWND, WM_APP_APPS, 0, Box::into_raw(list) as isize) };
    });
}

/// Sends the newest pending query; older pending text is simply overwritten
/// in the slot and never sent.
fn ipc_worker(worker: Arc<Worker>, reply_hwnd: usize, instance: Option<String>) {
    loop {
        let (id, text) = {
            let mut slot = worker.slot.lock().unwrap();
            while slot.is_none() {
                slot = worker.wake.wait(slot).unwrap();
            }
            slot.take().unwrap()
        };
        let mut q = Query::new(files::query(&text));
        q.max_results = files::FETCH;
        q.request_flags = FILE_FLAGS;
        q.sort = files::SORT;
        unsafe {
            let reply = reply_hwnd as HWND;
            let target = ipc::find_any(instance.as_deref());
            if target.is_null() || !ipc::send_query(target, reply, id, &q) {
                PostMessageW(reply, WM_APP_NOT_RUNNING, id as usize, 0);
            }
        }
    }
}

fn send_file_query(text: String) {
    let a = app();
    a.latest_id = a.latest_id.wrapping_add(1).max(1);
    *a.worker.slot.lock().unwrap() = Some((a.latest_id, text));
    a.worker.wake.notify_one();
}

fn is_url(q: &str) -> bool {
    !q.contains(' ') && (q.starts_with("http://") || q.starts_with("https://") || q.starts_with("www."))
}

/// Rebuilds the result rows from the query, the app list and the file
/// results that have arrived so far. Order: a calculation, apps, files, and
/// last a web search.
fn rebuild_rows() {
    let a = app();
    let q = a.query.trim();
    let mut rows = Vec::new();
    if let Some(cmd) = q.strip_prefix('>') {
        rows.push(Row::Command(cmd.trim().to_string()));
    } else if q.is_empty() {
        for id in a.history.recent() {
            if let Some(idx) = a.apps.iter().position(|x| x.id == id) {
                rows.push(Row::App { idx, positions: Vec::new() });
                if rows.len() == MAX_RECENT {
                    break;
                }
            }
        }
    } else {
        if let Some(v) = calc::eval(q) {
            rows.push(Row::Calc(v));
        }
        let mut scored: Vec<(i32, usize, Vec<usize>)> = a
            .apps
            .iter()
            .enumerate()
            .filter_map(|(i, x)| fuzzy::score(&x.name, q).map(|m| (m.score + a.history.boost(&x.id), i, m.positions)))
            .collect();
        scored.sort_by(|x, y| y.0.cmp(&x.0).then(a.apps[x.1].name.len().cmp(&a.apps[y.1].name.len())));
        rows.extend(scored.into_iter().take(MAX_APPS).map(|(_, idx, positions)| Row::App { idx, positions }));
        rows.extend((0..a.files.len()).map(Row::File));
        rows.push(Row::Web(q.to_string()));
    }
    a.rows = rows;
}

unsafe fn on_text_changed() {
    let a = app();
    a.query = edit_text();
    let q = a.query.trim().to_string();
    let wants_files = q.chars().count() >= 2 && !q.starts_with('>') && !is_url(&q) && calc::eval(&q).is_none();
    if wants_files {
        // Keep the previous files on screen until the new reply lands: no flicker.
        if a.file_status != FileStatus::Ready {
            a.files.clear();
        }
        a.file_status = FileStatus::Pending;
        send_file_query(q);
    } else {
        a.files.clear();
        a.file_status = FileStatus::Off;
        a.latest_id = a.latest_id.wrapping_add(1).max(1); // drop in-flight replies
    }
    rebuild_rows();
    a.sel = 0;
    a.scroll = 0;
    a.hover = -1;
    relayout();
}

unsafe fn on_files(payload: &[u8]) {
    let a = app();
    let Some(list) = ipc::decode_list2(payload) else { return };
    let on_web = matches!(a.rows.get(a.sel as usize), Some(Row::Web(_)));
    let profile = std::env::var("USERPROFILE").unwrap_or_default();
    a.files = files::rank(list.items, a.query.trim(), &profile);
    a.file_status = FileStatus::Ready;
    rebuild_rows();
    if on_web {
        a.sel = a.rows.len() as i32 - 1; // the web row moved down
    }
    a.sel = a.sel.clamp(0, (a.rows.len() as i32 - 1).max(0));
    relayout();
    if a.render.is_some() {
        render_and_quit();
    }
}

unsafe fn style_frame(hwnd: HWND) {
    let t = app().theme;
    let round = DWMWCP_ROUND;
    let hr = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &round as *const i32 as *const c_void, 4);
    if hr < 0 {
        // Pre-Windows 11: no DWM rounding, so clip to a rounded region.
        let mut rc: RECT = zeroed();
        GetWindowRect(hwnd, &mut rc);
        let r = scale(16);
        SetWindowRgn(hwnd, CreateRoundRectRgn(0, 0, rc.right - rc.left + 1, rc.bottom - rc.top + 1, r, r), 1);
    }
    DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as u32, &t.border as *const u32 as *const c_void, 4);
}

unsafe fn make_font(px: i32, weight: i32, face: &str) -> HFONT {
    CreateFontW(
        -scale(px),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        CLEARTYPE_QUALITY as u32,
        0,
        wide(face).as_ptr(),
    )
}

unsafe fn rebuild_fonts() {
    let a = app();
    if let Some(f) = a.fonts.take() {
        for h in [f.edit, f.name, f.name_bold, f.small, f.glyph, f.glyph_big] {
            DeleteObject(h);
        }
    }
    a.fonts = Some(Fonts {
        edit: make_font(20, 400, "Segoe UI"),
        name: make_font(15, 400, "Segoe UI"),
        name_bold: make_font(15, 700, "Segoe UI"),
        small: make_font(12, 400, "Segoe UI"),
        glyph: make_font(18, 400, "Segoe MDL2 Assets"),
        glyph_big: make_font(22, 400, "Segoe MDL2 Assets"),
    });
    SendMessageW(a.edit, WM_SETFONT, a.fonts.as_ref().unwrap().edit as usize, 1);
    // Icons are rendered for one size; fetch them again at the new DPI.
    for (_, bmp) in a.app_icons.drain() {
        DeleteObject(bmp);
    }
}

/// Centres the box horizontally on the monitor under the mouse, a fifth of
/// the way down, and sizes the edit control inside it.
unsafe fn place_window() {
    let a = app();
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut mi);
    let work = mi.rcWork;
    let (mut dx, mut dy) = (96u32, 96u32);
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) < 0 {
        dx = 96;
    }
    if dx != a.dpi || a.fonts.is_none() {
        a.dpi = dx;
        rebuild_fonts();
    }
    let w = scale(WIDTH).min(work.right - work.left - scale(32));
    let x = work.left + (work.right - work.left - w) / 2;
    let y = work.top + (work.bottom - work.top) / 5;
    SetWindowPos(a.hwnd, HWND_TOPMOST, x, y, w, window_height(), SWP_NOACTIVATE);

    let mut tm: TEXTMETRICW = zeroed();
    let dc = GetDC(a.edit);
    let old = SelectObject(dc, a.fonts.as_ref().unwrap().edit);
    GetTextMetricsW(dc, &mut tm);
    SelectObject(dc, old);
    ReleaseDC(a.edit, dc);
    let eh = tm.tmHeight + scale(2);
    let left = scale(52);
    SetWindowPos(a.edit, null_mut(), left, (scale(BOX_H) - eh) / 2, w - left - scale(16), eh, SWP_NOZORDER | SWP_NOACTIVATE);
    SendMessageW(a.edit, EM_SETMARGINS, 3, 0);
    style_frame(a.hwnd);
}

fn visible_rows() -> i32 {
    (app().rows.len() as i32).min(VISIBLE_ROWS)
}

fn list_top() -> i32 {
    scale(BOX_H) + 1
}

fn window_height() -> i32 {
    let n = visible_rows();
    if n == 0 {
        scale(BOX_H)
    } else {
        list_top() + scale(PAD) * 2 + n * scale(ROW_H) + scale(FOOTER_H)
    }
}

/// Resizes the window for the current rows, keeping its top edge fixed.
unsafe fn relayout() {
    let a = app();
    let mut rc: RECT = zeroed();
    GetWindowRect(a.hwnd, &mut rc);
    let h = window_height();
    if rc.bottom - rc.top != h {
        SetWindowPos(a.hwnd, null_mut(), 0, 0, rc.right - rc.left, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        style_frame(a.hwnd); // the Windows 10 rounded region depends on the size
    }
    InvalidateRect(a.hwnd, null(), 0);
}

fn edit_text() -> String {
    let a = app();
    unsafe {
        let len = GetWindowTextLengthW(a.edit);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(a.edit, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

unsafe fn move_selection(delta: i32) {
    let a = app();
    let n = a.rows.len() as i32;
    if n == 0 {
        return;
    }
    a.sel = (a.sel + delta).clamp(0, n - 1);
    if a.sel < a.scroll {
        a.scroll = a.sel;
    } else if a.sel >= a.scroll + VISIBLE_ROWS {
        a.scroll = a.sel - VISIBLE_ROWS + 1;
    }
    InvalidateRect(a.hwnd, null(), 0);
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

unsafe fn shell_open(verb: Option<&str>, file: &str, args: Option<&str>, dir: Option<&str>) -> bool {
    let (verb, file, args, dir) = (verb.map(wide), wide(file), args.map(wide), dir.map(wide));
    let ptr = |v: &Option<Vec<u16>>| v.as_ref().map_or(null(), |w| w.as_ptr());
    ShellExecuteW(null_mut(), ptr(&verb), file.as_ptr(), ptr(&args), ptr(&dir), SW_SHOWNORMAL) as isize > 32
}

unsafe fn reveal(path: &str) {
    shell_open(None, "explorer.exe", Some(&format!("/select,\"{path}\"")), None);
}

unsafe fn set_clipboard(text: &str) {
    let w = wide(text);
    if OpenClipboard(app().hwnd) == 0 {
        return;
    }
    EmptyClipboard();
    let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
    if !mem.is_null() {
        let p = GlobalLock(mem) as *mut u16;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            GlobalUnlock(mem);
            SetClipboardData(CF_UNICODETEXT, mem);
        }
    }
    CloseClipboard();
}

/// Enter: open the row. Ctrl+Enter: show it in its folder. Ctrl+Shift+Enter:
/// run it as administrator.
unsafe fn open_row(index: i32, ctrl: bool, shift: bool) {
    let a = app();
    let Some(row) = a.rows.get(index as usize) else { return };
    let admin = ctrl && shift;
    let verb = admin.then_some("runas");
    match row {
        Row::Calc(v) => {
            set_clipboard(&calc::plain(*v));
            dismiss();
        }
        Row::App { idx, .. } => {
            let entry = a.apps[*idx].clone();
            dismiss();
            match (&entry.path, ctrl && !shift) {
                (Some(path), true) => reveal(path),
                _ => {
                    if shell_open(verb, &entry.launch_target(), None, None) {
                        a.history.record(&entry.id);
                    }
                }
            }
        }
        Row::File(i) => {
            let item = a.files[*i].clone();
            dismiss();
            let full = item.full_path();
            if ctrl && !shift {
                reveal(&full);
            } else {
                shell_open(verb, &full, None, Some(&item.path));
            }
        }
        Row::Command(cmd) => {
            if cmd.is_empty() {
                return;
            }
            let args = format!("/k {cmd}");
            dismiss();
            let home = std::env::var("USERPROFILE").ok();
            shell_open(verb, "cmd.exe", Some(&args), home.as_deref());
        }
        Row::Web(q) => {
            let url = if is_url(q) {
                if q.starts_with("www.") { format!("https://{q}") } else { q.clone() }
            } else {
                format!("https://www.google.com/search?q={}", url_encode(q))
            };
            dismiss();
            shell_open(None, &url, None, None);
        }
    }
}

unsafe fn dismiss() {
    ShowWindow(app().hwnd, SW_HIDE);
}

/// Like EverythingToolbar's ForciblySetForegroundWindow: try the polite way,
/// then borrow the foreground thread's input state.
unsafe fn force_foreground(hwnd: HWND) {
    if SetForegroundWindow(hwnd) != 0 {
        return;
    }
    let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
    let me = GetCurrentThreadId();
    if fg_thread != 0 && fg_thread != me {
        AttachThreadInput(me, fg_thread, 1);
        SetForegroundWindow(hwnd);
        SetActiveWindow(hwnd);
        AttachThreadInput(me, fg_thread, 0);
    }
}

unsafe fn activate() {
    let a = app();
    if a.indexed_at.is_some_and(|t| t.elapsed() > REINDEX_AFTER) {
        reindex();
    }
    // Recent apps may have changed since the box was last open.
    if a.query.trim().is_empty() {
        rebuild_rows();
        a.sel = 0;
        a.scroll = 0;
    }
    place_window(); // the mouse, monitor or DPI may have changed while hidden
    ShowWindow(a.hwnd, SW_SHOW);
    force_foreground(a.hwnd);
    SetFocus(a.edit);
    SendMessageW(a.edit, EM_SETSEL, 0, -1);
    InvalidateRect(a.hwnd, null(), 0);
}

/// Ctrl+Backspace: the classic EDIT control inserts 0x7F instead of
/// deleting a word.
unsafe fn delete_word_back(edit: HWND) {
    let mut start: u32 = 0;
    let mut end: u32 = 0;
    SendMessageW(edit, EM_GETSEL, &mut start as *mut u32 as usize, &mut end as *mut u32 as isize);
    let text: Vec<u16> = edit_text().encode_utf16().collect();
    let mut i = (end as usize).min(text.len());
    while i > 0 && text[i - 1] == b' ' as u16 {
        i -= 1;
    }
    while i > 0 && text[i - 1] != b' ' as u16 {
        i -= 1;
    }
    SendMessageW(edit, EM_SETSEL, i, end as isize);
    SendMessageW(edit, EM_REPLACESEL, 1, wide("").as_ptr() as isize);
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
            let shift = GetKeyState(VK_SHIFT as i32) < 0;
            let handled = match wparam as u16 {
                VK_DOWN => {
                    move_selection(1);
                    true
                }
                VK_UP => {
                    move_selection(-1);
                    true
                }
                VK_NEXT => {
                    move_selection(VISIBLE_ROWS);
                    true
                }
                VK_PRIOR => {
                    move_selection(-VISIBLE_ROWS);
                    true
                }
                VK_RETURN => {
                    open_row(app().sel, ctrl, shift);
                    true
                }
                VK_ESCAPE => {
                    dismiss();
                    true
                }
                _ => false,
            };
            if handled {
                return 0;
            }
        }
        WM_CHAR => match wparam {
            0x0D | 0x1B | 0x0A => return 0, // no beep for Enter / Escape / Ctrl+Enter
            0x7F => {
                delete_word_back(hwnd);
                return 0;
            }
            _ => {}
        },
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

unsafe fn startup_enabled() -> bool {
    let mut len: u32 = 0;
    RegGetValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide("hop").as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut len) == 0
}

unsafe fn set_startup(on: bool) {
    let (key, name) = (wide(RUN_KEY), wide("hop"));
    if on {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let cmd = wide(&format!("\"{exe}\" --hidden"));
        RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, cmd.as_ptr() as *const c_void, (cmd.len() * 2) as u32);
    } else {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let a = app();
    let menu = CreatePopupMenu();
    let label = match &a.hotkey {
        Some(h) => format!("{h} to open"),
        None => "No hotkey (see --hotkey)".to_string(),
    };
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&label).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let check = if startup_enabled() { MF_CHECKED } else { MF_UNCHECKED };
    AppendMenuW(menu, MF_STRING | check, ID_MENU_STARTUP, wide("Start with Windows").as_ptr());
    AppendMenuW(menu, MF_STRING, ID_MENU_REINDEX, wide(&format!("Refresh app list ({} apps)", a.apps.len())).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_MENU_EXIT, wide("Exit hop").as_ptr());
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
    DestroyMenu(menu);
}

fn row_at(y: i32) -> i32 {
    let a = app();
    let top = list_top() + scale(PAD);
    if y < top {
        return -1;
    }
    let r = (y - top) / scale(ROW_H);
    let i = a.scroll + r;
    if r < visible_rows() && i < a.rows.len() as i32 {
        i
    } else {
        -1
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let a = app();
    match msg {
        ipc::WM_COPYDATA => {
            // Only the newest query's reply is accepted; older ones are stale.
            if let Some(payload) = ipc::reply_payload(msg, lparam, a.latest_id) {
                on_files(payload);
            }
            1
        }
        WM_APP_NOT_RUNNING => {
            if wparam as u32 == a.latest_id {
                a.files.clear();
                a.file_status = FileStatus::NotRunning;
                rebuild_rows();
                relayout();
                if a.render.is_some() {
                    render_and_quit();
                }
            }
            0
        }
        WM_APP_APPS => {
            let list = Box::from_raw(lparam as *mut Vec<AppEntry>);
            a.indexing = false;
            a.indexed_at = Some(Instant::now());
            a.apps = *list;
            let sel = a.sel;
            rebuild_rows();
            a.sel = sel.clamp(0, (a.rows.len() as i32 - 1).max(0));
            relayout();
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let code = (wparam >> 16) as u32 & 0xFFFF;
            if id == ID_EDIT {
                if code == EN_CHANGE {
                    on_text_changed();
                }
            } else if lparam == 0 {
                match id {
                    ID_MENU_EXIT => {
                        DestroyWindow(hwnd);
                    }
                    ID_MENU_STARTUP => set_startup(!startup_enabled()),
                    ID_MENU_REINDEX => reindex(),
                    _ => {}
                }
            }
            0
        }
        WM_HOTKEY => {
            if wparam as i32 == HOTKEY_ID {
                // The hotkey toggles.
                if IsWindowVisible(hwnd) != 0 && GetForegroundWindow() == hwnd {
                    dismiss();
                } else {
                    activate();
                }
            }
            0
        }
        WM_ACTIVATE => {
            if (wparam & 0xFFFF) as u32 == WA_INACTIVE {
                // Clicked away. The right-click menu takes focus too, but
                // closing on it would make its commands unreachable.
                if lparam == 0 || GetWindowThreadProcessId(lparam as HWND, null_mut()) != GetCurrentThreadId() {
                    dismiss();
                }
            } else {
                SetFocus(a.edit);
            }
            0
        }
        WM_MOUSEMOVE => {
            if !a.tracking_mouse {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
                a.tracking_mouse = true;
            }
            let row = row_at(((lparam >> 16) & 0xFFFF) as i16 as i32);
            if row != a.hover {
                a.hover = row;
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            a.tracking_mouse = false;
            a.hover = -1;
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let row = row_at(((lparam >> 16) & 0xFFFF) as i16 as i32);
            if row >= 0 {
                a.sel = row;
                open_row(row, GetKeyState(VK_CONTROL as i32) < 0, GetKeyState(VK_SHIFT as i32) < 0);
            } else {
                SetFocus(a.edit);
            }
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32;
            let max_scroll = (a.rows.len() as i32 - VISIBLE_ROWS).max(0);
            a.scroll = (a.scroll - delta.signum() * 3).clamp(0, max_scroll);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_CONTEXTMENU | WM_RBUTTONUP => {
            show_menu(hwnd);
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = wparam as HDC;
            SetTextColor(dc, a.theme.text);
            SetBkColor(dc, a.theme.bg);
            a.bg_brush as isize
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            paint(hwnd, dc);
            EndPaint(hwnd, &ps);
            0
        }
        WM_TIMER if wparam == RENDER_TIMER => {
            render_and_quit();
            0
        }
        WM_SETTINGCHANGE => {
            a.theme = Theme::current(a.force_light);
            DeleteObject(a.bg_brush);
            a.bg_brush = CreateSolidBrush(a.theme.bg);
            style_frame(hwnd);
            InvalidateRect(hwnd, null(), 1);
            InvalidateRect(a.edit, null(), 1);
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            place_window();
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// System image list index for a file's icon, by extension. Uses
/// SHGFI_USEFILEATTRIBUTES so the shell never opens the file.
unsafe fn file_icon(item: &Item) -> i32 {
    let a = app();
    let key = if item.is_folder() {
        "<folder>".to_string()
    } else {
        match item.name.rfind('.') {
            Some(i) => item.name[i..].to_lowercase(),
            None => "<file>".to_string(),
        }
    };
    if let Some(&i) = a.ext_icons.get(&key) {
        return i;
    }
    let (probe, attrs) = if item.is_folder() {
        ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY)
    } else {
        (format!("file{}", if key.starts_with('.') { &key } else { "" }), FILE_ATTRIBUTE_NORMAL)
    };
    let mut sfi: SHFILEINFOW = zeroed();
    let himl = SHGetFileInfoW(
        wide(&probe).as_ptr(),
        attrs,
        &mut sfi,
        size_of::<SHFILEINFOW>() as u32,
        SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_LARGEICON,
    );
    if himl != 0 {
        a.image_list = himl as isize;
    }
    a.ext_icons.insert(key, sfi.iIcon);
    sfi.iIcon
}

/// The app's icon, fetched on first paint and kept for the session.
unsafe fn app_icon(id: &str) -> HBITMAP {
    let a = app();
    if let Some(&b) = a.app_icons.get(id) {
        return b;
    }
    let b = apps::icon(id, scale(ICON));
    a.app_icons.insert(id.to_string(), b);
    b
}

/// Draws a 32-bit alpha bitmap scaled into a square.
unsafe fn draw_bitmap(dc: HDC, bmp: HBITMAP, x: i32, y: i32, size: i32) {
    let mut bm: BITMAP = zeroed();
    if GetObjectW(bmp, size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) == 0 {
        return;
    }
    let src = CreateCompatibleDC(dc);
    let old = SelectObject(src, bmp);
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    SetStretchBltMode(dc, HALFTONE);
    GdiAlphaBlend(dc, x, y, size, size, src, 0, 0, bm.bmWidth, bm.bmHeight.abs(), blend);
    SelectObject(src, old);
    DeleteDC(src);
}

unsafe fn text_width(dc: HDC, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut sz: SIZE = zeroed();
    GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut sz);
    sz.cx
}

unsafe fn draw_text(dc: HDC, s: &str, rc: &mut RECT, flags: u32) {
    // An empty Vec's pointer is a dangling placeholder, and DrawTextW with
    // ellipsis flags reads it even for a zero count. Draw nothing instead.
    if s.is_empty() {
        return;
    }
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    DrawTextW(dc, w.as_ptr() as _, w.len() as i32 - 1, rc, flags | DT_SINGLELINE | DT_NOPREFIX);
}

unsafe fn draw_glyph(dc: HDC, font: HFONT, glyph: u16, color: COLORREF, mut rc: RECT) {
    SelectObject(dc, font);
    SetTextColor(dc, color);
    DrawTextW(dc, [glyph].as_ptr(), 1, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
}

/// Draws `(text, matched)` runs on one line, matched runs bold in the accent
/// colour; the run that overflows ends the line with an ellipsis.
unsafe fn draw_runs(dc: HDC, f: &Fonts, t: Theme, runs: &[(String, bool)], mut x: i32, top: i32, right: i32, bottom: i32) {
    for (run, hit) in runs {
        SelectObject(dc, if *hit { f.name_bold } else { f.name });
        SetTextColor(dc, if *hit { t.accent } else { t.text });
        let width = text_width(dc, run);
        let mut r = RECT { left: x, top, right, bottom };
        draw_text(dc, run, &mut r, DT_LEFT | DT_BOTTOM | DT_END_ELLIPSIS);
        x += width;
        if x >= right {
            break;
        }
    }
}

/// Title runs, subtitle and the Enter action for a row.
fn describe(row: &Row) -> (Vec<(String, bool)>, String, &'static str) {
    let a = app();
    let plain = |s: String| vec![(s, false)];
    match row {
        Row::Calc(v) => (vec![(format!("= {}", calc::pretty(*v)), true)], format!("{}  ·  Enter copies the result", a.query.trim()), "Copy"),
        Row::App { idx, positions } => {
            let e = &a.apps[*idx];
            let sub = match &e.path {
                Some(p) => p.clone(),
                None if e.id.starts_with("http") => e.id.clone(),
                None => "App".to_string(),
            };
            (fuzzy::runs(&e.name, positions), sub, "Open")
        }
        Row::File(i) => {
            let item = &a.files[*i];
            (highlight::segments(item.highlighted_name.as_deref().unwrap_or(&item.name)), item.path.clone(), "Open")
        }
        Row::Command(cmd) if cmd.is_empty() => (plain("Type a command to run".into()), "For example: > ipconfig /all".into(), ""),
        Row::Command(cmd) => (plain(cmd.clone()), "Run in Command Prompt".into(), "Run"),
        Row::Web(q) if is_url(q) => (plain(format!("Open {q}")), "In your default browser".into(), "Open"),
        Row::Web(q) => (plain(format!("Search the web for \u{201C}{q}\u{201D}")), "Google, in your default browser".into(), "Search"),
    }
}

/// Footer key hints for the selected row.
fn hints(row: Option<&Row>) -> &'static str {
    match row {
        Some(Row::Calc(_)) => "Enter copy   Esc close",
        Some(Row::App { idx, .. }) if app().apps[*idx].path.is_some() => {
            "Enter open   Ctrl+Enter file location   Ctrl+Shift+Enter as admin"
        }
        Some(Row::App { .. }) => "Enter open   Ctrl+Shift+Enter as admin   Esc close",
        Some(Row::File(_)) => "Enter open   Ctrl+Enter show in folder   Esc close",
        Some(Row::Command(_)) => "Enter run   Ctrl+Shift+Enter as admin   Esc close",
        _ => "Enter open   Esc close",
    }
}

/// Double-buffered paint of the box, the rows and the footer.
unsafe fn paint(hwnd: HWND, target: HDC) {
    let a = app();
    let Some(f) = &a.fonts else { return };
    let t = a.theme;
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    let dc = CreateCompatibleDC(target);
    let bmp = CreateCompatibleBitmap(target, w, h);
    let old_bmp = SelectObject(dc, bmp);
    FillRect(dc, &rc, a.bg_brush);
    SetBkMode(dc, TRANSPARENT as _);

    let box_h = scale(BOX_H);
    draw_glyph(dc, f.glyph, GLYPH_SEARCH, t.dim, RECT { left: scale(14), top: 0, right: scale(44), bottom: box_h });

    if !a.rows.is_empty() {
        let line = CreateSolidBrush(t.border);
        FillRect(dc, &RECT { left: 0, top: box_h, right: w, bottom: box_h + 1 }, line);
        DeleteObject(line);

        let (pad, row_h, icon) = (scale(PAD), scale(ROW_H), scale(ICON));
        let text_x = pad + scale(14) + icon + scale(14);
        let sel_brush = CreateSolidBrush(t.sel);
        let accent_brush = CreateSolidBrush(t.accent);
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        for r in 0..visible_rows() {
            let i = a.scroll + r;
            let Some(row) = a.rows.get(i as usize) else { break };
            let top = list_top() + pad + r * row_h;
            let bottom = top + row_h;
            if i == a.sel || i == a.hover {
                let old = SelectObject(dc, sel_brush);
                RoundRect(dc, pad, top, w - pad, bottom, scale(8), scale(8));
                SelectObject(dc, old);
            }
            if i == a.sel {
                // Windows 11 style selection pill.
                let old = SelectObject(dc, accent_brush);
                RoundRect(dc, pad + scale(2), top + scale(16), pad + scale(5), bottom - scale(16), scale(3), scale(3));
                SelectObject(dc, old);
            }

            let ix = pad + scale(14);
            let iy = top + (row_h - icon) / 2;
            let glyph_rc = RECT { left: ix, top, right: ix + icon, bottom };
            match row {
                Row::App { idx, .. } => {
                    let b = app_icon(&a.apps[*idx].id);
                    if b.is_null() {
                        draw_glyph(dc, f.glyph_big, GLYPH_APP, t.dim, glyph_rc);
                    } else {
                        draw_bitmap(dc, b, ix, iy, icon);
                    }
                }
                Row::File(fi) => {
                    let idx = file_icon(&a.files[*fi]);
                    if a.image_list != 0 {
                        let sys = GetSystemMetrics(SM_CXICON);
                        ImageList_Draw(a.image_list as _, idx, dc, ix + (icon - sys) / 2, top + (row_h - sys) / 2, ILD_TRANSPARENT);
                    }
                }
                Row::Calc(_) => draw_glyph(dc, f.glyph_big, GLYPH_CALC, t.accent, glyph_rc),
                Row::Command(_) => draw_glyph(dc, f.glyph_big, GLYPH_COMMAND, t.accent, glyph_rc),
                Row::Web(_) => draw_glyph(dc, f.glyph_big, GLYPH_GLOBE, t.accent, glyph_rc),
            }

            let (title, sub, action) = describe(row);
            let mut text_right = w - pad - scale(14);
            if i == a.sel && !action.is_empty() {
                SelectObject(dc, f.small);
                SetTextColor(dc, t.dim);
                let label = format!("{action}  \u{21B5}");
                let lw = text_width(dc, &label);
                let mut ar = RECT { left: text_right - lw, top, right: text_right, bottom };
                draw_text(dc, &label, &mut ar, DT_RIGHT | DT_VCENTER);
                text_right -= lw + scale(16);
            }
            let name_bottom = top + row_h / 2 + scale(3);
            draw_runs(dc, f, t, &title, text_x, top + scale(4), text_right, name_bottom);
            SelectObject(dc, f.small);
            SetTextColor(dc, t.dim);
            let mut sr = RECT { left: text_x, top: name_bottom + scale(1), right: text_right, bottom: bottom - scale(3) };
            draw_text(dc, &sub, &mut sr, DT_LEFT | DT_TOP | DT_PATH_ELLIPSIS | DT_END_ELLIPSIS);
        }
        SelectObject(dc, old_pen);
        DeleteObject(sel_brush);
        DeleteObject(accent_brush);

        // Footer: key hints, and file search status on the right.
        SelectObject(dc, f.small);
        SetTextColor(dc, t.dim);
        let mut fr = RECT { left: pad + scale(14), top: h - scale(FOOTER_H), right: w - pad - scale(14), bottom: h - scale(4) };
        let status = match a.file_status {
            FileStatus::NotRunning => "Everything isn't running: no file results".to_string(),
            _ if a.query.trim().is_empty() => "Recent".to_string(),
            _ if a.rows.len() as i32 > VISIBLE_ROWS => format!("{} results", a.rows.len()),
            _ => String::new(),
        };
        let status_w = if status.is_empty() { 0 } else { text_width(dc, &status) + scale(16) };
        let mut hr = RECT { right: fr.right - status_w, ..fr };
        draw_text(dc, hints(a.rows.get(a.sel as usize)), &mut hr, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        draw_text(dc, &status, &mut fr, DT_RIGHT | DT_VCENTER);
    }

    BitBlt(target, 0, 0, w, h, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteDC(dc);
}

/// Paints the hidden window into an offscreen bitmap, with the rounded
/// corners and border DWM would add, writes it as a .bmp plus a text summary
/// of the rows, and exits.
unsafe fn render_and_quit() {
    let a = app();
    let Some(job) = a.render.take() else { return };
    relayout();
    let mut wr: RECT = zeroed();
    GetWindowRect(a.hwnd, &mut wr);
    let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
    let margin = scale(24);
    let (w, h) = (ww + margin * 2, wh + margin * 2);

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, w, h);
    ReleaseDC(null_mut(), screen);
    let old = SelectObject(dc, bmp);
    let backdrop = CreateSolidBrush(if a.theme.bg > rgb(128, 128, 128) { rgb(196, 208, 222) } else { rgb(24, 32, 44) });
    FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, backdrop);
    DeleteObject(backdrop);

    let rgn = CreateRoundRectRgn(margin, margin, margin + ww + 1, margin + wh + 1, scale(16), scale(16));
    SelectClipRgn(dc, rgn);
    SetViewportOrgEx(dc, margin, margin, null_mut());
    paint(a.hwnd, dc);
    let mut er: RECT = zeroed();
    GetWindowRect(a.edit, &mut er);
    SetViewportOrgEx(dc, margin + er.left - wr.left, margin + er.top - wr.top, null_mut());
    SendMessageW(a.edit, WM_PRINT, dc as usize, (PRF_CLIENT | PRF_ERASEBKGND) as isize);
    SetViewportOrgEx(dc, 0, 0, null_mut());
    SelectClipRgn(dc, null_mut());
    let border = CreateSolidBrush(a.theme.border);
    FrameRgn(dc, rgn, border, 1, 1);
    DeleteObject(border);
    DeleteObject(rgn);
    SelectObject(dc, old);
    save_bmp(dc, bmp, w, h, &job.out);

    let mut summary = format!(
        "dpi={} apps={} files={} status={}\n",
        a.dpi,
        a.apps.len(),
        a.files.len(),
        match a.file_status {
            FileStatus::Off => "off",
            FileStatus::Pending => "pending",
            FileStatus::Ready => "ready",
            FileStatus::NotRunning => "not-running",
        }
    );
    for row in &a.rows {
        let (title, sub, _) = describe(row);
        let title: String = title.iter().map(|(s, hit)| if *hit { format!("[{s}]") } else { s.clone() }).collect();
        summary.push_str(&format!("{title} | {sub}\n"));
    }
    let _ = std::fs::write(format!("{}.txt", job.out), summary);
    DeleteObject(bmp);
    DeleteDC(dc);
    PostQuitMessage(0);
}

unsafe fn save_bmp(dc: HDC, bmp: HBITMAP, w: i32, h: i32, path: &str) {
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: h, // bottom-up, as .bmp files store it
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..zeroed()
    };
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    GetDIBits(dc, bmp, 0, h as u32, pixels.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
    let header_len = 14 + size_of::<BITMAPINFOHEADER>();
    let mut file = Vec::with_capacity(header_len + pixels.len());
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&((header_len + pixels.len()) as u32).to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&(header_len as u32).to_le_bytes());
    let ih = std::slice::from_raw_parts(&bi.bmiHeader as *const BITMAPINFOHEADER as *const u8, size_of::<BITMAPINFOHEADER>());
    file.extend_from_slice(ih);
    file.extend_from_slice(&pixels);
    let _ = std::fs::write(path, file);
}
