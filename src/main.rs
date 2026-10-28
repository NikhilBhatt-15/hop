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
const REINDEX_AFTER: Duration = Duration::from_secs(120);
const FILE_FLAGS: u32 = request::FILE_NAME | request::PATH | request::HIGHLIGHTED_FILE_NAME;

// Layout in 96-DPI pixels.
const WIDTH: i32 = 680;
const BOX_H: i32 = 60;
const ROW_H: i32 = 50;
const HEADER_H: i32 = 28;
/// The list scrolls beyond this height: about eight result rows.
const LIST_MAX_H: i32 = 420;
const PAD: i32 = 6;
const FOOTER_H: i32 = 38;
const ICON: i32 = 32;
const KEYCAP_H: i32 = 20;

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
const GLYPH_BULB: u16 = 0xE82F;

/// Shown in the empty box until there is launch history. Choosing one types
/// the example in, so the user sees what it does.
const TIPS: &[(&str, &str)] = &[
    ("notepad", "Type part of an app's name to open it"),
    ("report pdf", "Find files by name (with Everything running)"),
    ("24*18+7", "Do math. Enter copies the result"),
    ("> ipconfig", "Start with > to run a command"),
    ("www.github.com", "Open a website, or search the web for anything"),
];

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[derive(Clone, Copy)]
struct Theme {
    bg: COLORREF,
    border: COLORREF,
    text: COLORREF,
    dim: COLORREF,
    hover: COLORREF,
    sel: COLORREF,
    key: COLORREF,
    accent: COLORREF,
}

impl Theme {
    fn current(force_light: Option<bool>) -> Theme {
        if force_light.unwrap_or_else(system_uses_light_theme) {
            Theme {
                bg: rgb(250, 250, 250),
                border: rgb(218, 218, 218),
                text: rgb(26, 26, 26),
                dim: rgb(104, 104, 104),
                hover: rgb(241, 241, 241),
                sel: rgb(232, 232, 232),
                key: rgb(255, 255, 255),
                accent: rgb(0, 95, 184),
            }
        } else {
            Theme {
                bg: rgb(39, 39, 39),
                border: rgb(66, 66, 66),
                text: rgb(242, 242, 242),
                dim: rgb(160, 160, 160),
                hover: rgb(49, 49, 49),
                sel: rgb(58, 58, 58),
                key: rgb(52, 52, 52),
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
    /// A section title; never selected.
    Header(&'static str),
    Tip(usize),
    Calc(f64),
    App { idx: usize, positions: Vec<usize> },
    File(usize),
    Command(String),
    Web(String),
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Row::Header(_))
    }

    fn height(&self) -> i32 {
        scale(if self.selectable() { ROW_H } else { HEADER_H })
    }
}

struct Fonts {
    edit: HFONT,
    name: HFONT,
    name_bold: HFONT,
    result: HFONT,
    small: HFONT,
    header: HFONT,
    key: HFONT,
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
    profile: String,
    query: String,
    rows: Vec<Row>,
    files: Vec<Item>,
    file_status: FileStatus,
    sel: i32,
    /// Scroll offset of the list, in pixels.
    scroll: i32,
    hover: i32,
    tracking_mouse: bool,
    latest_id: u32,
    worker: Arc<Worker>,
    /// App icons by id, at the current DPI.
    app_icons: HashMap<String, HBITMAP>,
    image_list: isize,
    /// System image list indexes by extension, or by full path for files
    /// whose icon is their own (.exe, .lnk...).
    file_icons: HashMap<String, i32>,
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
    /// `--hotkey`; without it hop takes the first free one of `hotkey::DEFAULTS`.
    hotkey: Option<String>,
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
        hotkey: value("--hotkey", 1),
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
        profile: std::env::var("USERPROFILE").unwrap_or_default(),
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
        file_icons: HashMap::new(),
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
        if q.is_empty() {
            on_text_changed(); // the empty box: recent apps or tips
        } else {
            SetWindowTextW(a.edit, wide(&q).as_ptr()); // EN_CHANGE runs the query
        }
        if a.file_status != FileStatus::Pending {
            render_and_quit();
        }
    } else {
        reindex();
        match &opts.hotkey {
            Some(spec) => {
                register_hotkey(spec, true);
            }
            None => {
                // The first default nobody else holds.
                if !hotkey::DEFAULTS.iter().any(|spec| register_hotkey(spec, false)) {
                    register_hotkey(hotkey::DEFAULTS[0], true);
                }
            }
        }
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
/// (RegisterHotKey is first come, first served); with `report`, say so
/// instead of failing silently.
unsafe fn register_hotkey(spec: &str, report: bool) -> bool {
    let a = app();
    let problem = match hotkey::parse(spec) {
        Err(e) => Some(format!("The hotkey {e}.")),
        Ok(hk) => {
            if RegisterHotKey(a.hwnd, HOTKEY_ID, hk.modifiers | MOD_NOREPEAT, hk.vk) != 0 {
                a.hotkey = Some(spec.to_string());
                return true;
            }
            Some(format!("{spec} is already used by another app, so hop can't use it."))
        }
    };
    if let (Some(problem), true) = (problem, report) {
        let text = format!(
            "{problem}\n\nRun hop.exe again to open it, or restart it with a different hotkey, e.g.\n\n    hop.exe --hotkey Ctrl+Alt+H"
        );
        MessageBoxW(a.hwnd, wide(&text).as_ptr(), wide("hop").as_ptr(), MB_OK | MB_ICONWARNING);
    }
    false
}

/// The hotkey as shown to the user.
fn hotkey_label() -> &'static str {
    let a = app();
    match &a.hotkey {
        Some(h) => h.as_str(),
        None => hotkey::DEFAULTS[0],
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
/// results that have arrived so far, in sections: a calculation, apps,
/// files, and last a web search.
fn rebuild_rows() {
    let a = app();
    let q = a.query.trim();
    let mut rows = Vec::new();
    if let Some(cmd) = q.strip_prefix('>') {
        rows.push(Row::Header("Command"));
        rows.push(Row::Command(cmd.trim().to_string()));
    } else if q.is_empty() {
        let recent: Vec<usize> = a
            .history
            .recent()
            .into_iter()
            .filter_map(|id| a.apps.iter().position(|x| x.id == id))
            .take(MAX_RECENT)
            .collect();
        if recent.is_empty() {
            rows.push(Row::Header("Try"));
            rows.extend((0..TIPS.len()).map(Row::Tip));
        } else {
            rows.push(Row::Header("Recent"));
            rows.extend(recent.into_iter().map(|idx| Row::App { idx, positions: Vec::new() }));
        }
    } else {
        if let Some(v) = calc::eval(q) {
            rows.push(Row::Header("Calculator"));
            rows.push(Row::Calc(v));
        }
        let mut scored: Vec<(i32, usize, Vec<usize>)> = a
            .apps
            .iter()
            .enumerate()
            .filter_map(|(i, x)| fuzzy::score(&x.name, q).map(|m| (m.score + a.history.boost(&x.id), i, m.positions)))
            .collect();
        scored.sort_by(|x, y| y.0.cmp(&x.0).then(a.apps[x.1].name.len().cmp(&a.apps[y.1].name.len())));
        if !scored.is_empty() {
            rows.push(Row::Header("Apps"));
            rows.extend(scored.into_iter().take(MAX_APPS).map(|(_, idx, positions)| Row::App { idx, positions }));
        }
        if !a.files.is_empty() {
            rows.push(Row::Header("Files"));
            rows.extend((0..a.files.len()).map(Row::File));
        }
        rows.push(Row::Header("Web"));
        rows.push(Row::Web(q.to_string()));
    }
    a.rows = rows;
}

fn first_selectable() -> i32 {
    app().rows.iter().position(Row::selectable).map_or(0, |i| i as i32)
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
    a.sel = first_selectable();
    a.scroll = 0;
    a.hover = -1;
    relayout();
}

unsafe fn on_files(payload: &[u8]) {
    let a = app();
    let Some(list) = ipc::decode_list2(payload) else { return };
    let on_web = matches!(a.rows.get(a.sel as usize), Some(Row::Web(_)));
    a.files = files::rank(list.items, a.query.trim(), &a.profile);
    a.file_status = FileStatus::Ready;
    rebuild_rows();
    if on_web {
        a.sel = a.rows.len() as i32 - 1; // the web row moved down
    } else if !a.rows.get(a.sel as usize).is_some_and(Row::selectable) {
        a.sel = first_selectable();
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
        for h in [f.edit, f.name, f.name_bold, f.result, f.small, f.header, f.key, f.glyph, f.glyph_big] {
            DeleteObject(h);
        }
    }
    a.fonts = Some(Fonts {
        edit: make_font(21, 400, "Segoe UI"),
        name: make_font(15, 400, "Segoe UI"),
        name_bold: make_font(15, 600, "Segoe UI"),
        result: make_font(20, 600, "Segoe UI"),
        small: make_font(12, 400, "Segoe UI"),
        header: make_font(12, 600, "Segoe UI"),
        key: make_font(11, 400, "Segoe UI"),
        glyph: make_font(18, 400, "Segoe MDL2 Assets"),
        glyph_big: make_font(22, 400, "Segoe MDL2 Assets"),
    });
    SendMessageW(a.edit, WM_SETFONT, a.fonts.as_ref().unwrap().edit as usize, 1);
    // Icons are rendered for one size; fetch them again at the new DPI.
    for (_, bmp) in a.app_icons.drain() {
        DeleteObject(bmp);
    }
}

/// Width of the hotkey keycap shown at the right of the empty box.
unsafe fn hotkey_cap_width() -> i32 {
    let a = app();
    let Some(f) = &a.fonts else { return 0 };
    let dc = GetDC(null_mut());
    let old = SelectObject(dc, f.key);
    let w = text_width(dc, hotkey_label()) + scale(16);
    SelectObject(dc, old);
    ReleaseDC(null_mut(), dc);
    w
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
    let left = scale(54);
    let right = scale(20) + hotkey_cap_width() + scale(12);
    SetWindowPos(a.edit, null_mut(), left, (scale(BOX_H) - eh) / 2, w - left - right, eh, SWP_NOZORDER | SWP_NOACTIVATE);
    SendMessageW(a.edit, EM_SETMARGINS, 3, 0);
    style_frame(a.hwnd);
}

/// Total height of all rows.
fn content_height() -> i32 {
    app().rows.iter().map(Row::height).sum()
}

/// Height of the visible part of the list: everything if it fits, else as
/// many whole rows as fit, so the last one isn't cut in half.
fn view_height() -> i32 {
    let max = scale(LIST_MAX_H);
    let mut h = 0;
    for row in &app().rows {
        if h + row.height() > max {
            break;
        }
        h += row.height();
    }
    h
}

/// Top of row `i` within the list content.
fn row_top(i: usize) -> i32 {
    app().rows[..i].iter().map(Row::height).sum()
}

/// Window y of the list content's top edge, before scrolling.
fn list_top() -> i32 {
    scale(BOX_H) + 1 + scale(PAD)
}

fn window_height() -> i32 {
    if app().rows.is_empty() {
        scale(BOX_H)
    } else {
        list_top() + view_height() + scale(PAD) + scale(FOOTER_H)
    }
}

/// Resizes the window for the current rows, keeping its top edge fixed.
unsafe fn relayout() {
    let a = app();
    a.scroll = a.scroll.clamp(0, (content_height() - view_height()).max(0));
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

/// Scrolls so row `i` is in view. A section's first row brings its header
/// along.
fn ensure_visible(i: usize) {
    let a = app();
    let mut top = row_top(i);
    if i > 0 && !a.rows[i - 1].selectable() {
        top -= a.rows[i - 1].height();
    }
    let bottom = row_top(i) + a.rows[i].height();
    let view = view_height();
    if top < a.scroll {
        a.scroll = top;
    } else if bottom > a.scroll + view {
        a.scroll = bottom - view;
    }
}

/// Moves the selection by `steps` selectable rows, skipping headers.
unsafe fn move_selection(steps: i32) {
    let a = app();
    let n = a.rows.len() as i32;
    if n == 0 {
        return;
    }
    let dir = steps.signum();
    let mut i = a.sel;
    for _ in 0..steps.abs() {
        let mut j = i + dir;
        while j >= 0 && j < n && !a.rows[j as usize].selectable() {
            j += dir;
        }
        if j < 0 || j >= n {
            break;
        }
        i = j;
    }
    a.sel = i;
    ensure_visible(i as usize);
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
        Row::Header(_) => {}
        Row::Tip(i) => {
            // Type the example in; the search runs and shows what it does.
            let example = TIPS[*i].0;
            SetWindowTextW(a.edit, wide(example).as_ptr());
            let end = example.encode_utf16().count();
            SendMessageW(a.edit, EM_SETSEL, end, end as isize);
            SetFocus(a.edit);
        }
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
            let home = (!a.profile.is_empty()).then(|| a.profile.clone());
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
        a.sel = first_selectable();
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
                    move_selection(6);
                    true
                }
                VK_PRIOR => {
                    move_selection(-6);
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

/// The selectable row under window y, or -1.
fn row_at(y: i32) -> i32 {
    let a = app();
    let top = list_top();
    if y < top || y >= top + view_height() {
        return -1;
    }
    let cy = y - top + a.scroll;
    let mut acc = 0;
    for (i, row) in a.rows.iter().enumerate() {
        let h = row.height();
        if cy < acc + h {
            return if row.selectable() { i as i32 } else { -1 };
        }
        acc += h;
    }
    -1
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
                if !a.rows.get(a.sel as usize).is_some_and(Row::selectable) {
                    a.sel = first_selectable();
                }
                a.sel = a.sel.clamp(0, (a.rows.len() as i32 - 1).max(0));
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
            rebuild_rows();
            if !a.rows.get(a.sel as usize).is_some_and(Row::selectable) {
                a.sel = first_selectable();
            }
            a.sel = a.sel.clamp(0, (a.rows.len() as i32 - 1).max(0));
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
            let max_scroll = (content_height() - view_height()).max(0);
            a.scroll = (a.scroll - delta * scale(ROW_H) * 2 / 120).clamp(0, max_scroll);
            a.hover = -1;
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

/// System image list index for a file's icon. Programs, shortcuts and icon
/// files have icons of their own, read from the file (only for rows on
/// screen). Everything else gets its type's icon by extension, with
/// SHGFI_USEFILEATTRIBUTES so the shell never opens the file.
unsafe fn file_icon(item: &Item) -> i32 {
    let a = app();
    let ext = match item.name.rfind('.') {
        Some(i) if !item.is_folder() => item.name[i..].to_lowercase(),
        _ => String::new(),
    };
    let own = matches!(ext.as_str(), ".exe" | ".lnk" | ".url" | ".ico" | ".appref-ms");
    let key = if own {
        item.full_path()
    } else if item.is_folder() {
        "<folder>".to_string()
    } else if ext.is_empty() {
        "<file>".to_string()
    } else {
        ext.clone()
    };
    if let Some(&i) = a.file_icons.get(&key) {
        return i;
    }
    let mut sfi: SHFILEINFOW = zeroed();
    let mut himl = 0;
    if own {
        himl = SHGetFileInfoW(wide(&key).as_ptr(), 0, &mut sfi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_LARGEICON);
    }
    if himl == 0 {
        let (probe, attrs) = if item.is_folder() {
            ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY)
        } else {
            (format!("file{ext}"), FILE_ATTRIBUTE_NORMAL)
        };
        himl = SHGetFileInfoW(
            wide(&probe).as_ptr(),
            attrs,
            &mut sfi,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_LARGEICON,
        );
    }
    if himl != 0 {
        a.image_list = himl as isize;
    }
    a.file_icons.insert(key, sfi.iIcon);
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

unsafe fn fill_round(dc: HDC, rc: RECT, radius: i32, fill: COLORREF, edge: Option<COLORREF>) {
    let brush = CreateSolidBrush(fill);
    let pen = match edge {
        Some(c) => CreatePen(PS_SOLID, 1, c),
        None => GetStockObject(NULL_PEN) as HPEN,
    };
    let (ob, op) = (SelectObject(dc, brush), SelectObject(dc, pen));
    RoundRect(dc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
    SelectObject(dc, ob);
    SelectObject(dc, op);
    DeleteObject(brush);
    if edge.is_some() {
        DeleteObject(pen);
    }
}

/// Draws a keyboard key label like `[Enter]` with its left edge at `x`,
/// vertically centred on `cy`, and returns its width.
unsafe fn draw_keycap(dc: HDC, f: &Fonts, t: Theme, label: &str, x: i32, cy: i32) -> i32 {
    SelectObject(dc, f.key);
    let w = text_width(dc, label) + scale(12);
    let h = scale(KEYCAP_H);
    let rc = RECT { left: x, top: cy - h / 2, right: x + w, bottom: cy - h / 2 + h };
    fill_round(dc, rc, scale(6), t.key, Some(t.border));
    SetTextColor(dc, t.dim);
    let mut tr = rc;
    draw_text(dc, label, &mut tr, DT_CENTER | DT_VCENTER);
    w
}

/// Draws `(text, matched)` runs on one line, matched runs semibold in the
/// accent colour; the run that overflows ends the line with an ellipsis.
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
        Row::Header(h) => (plain(h.to_string()), String::new(), ""),
        Row::Tip(i) => (plain(TIPS[*i].0.to_string()), TIPS[*i].1.to_string(), "Try"),
        Row::Calc(v) => (vec![(calc::pretty(*v), true)], format!("{} =", a.query.trim()), "Copy"),
        Row::App { idx, positions } => {
            let e = &a.apps[*idx];
            let sub = match &e.path {
                Some(p) => files::tilde(p, &a.profile),
                None if e.id.starts_with("http") => e.id.clone(),
                None if e.is_packaged() => "Windows app".to_string(),
                None => "App".to_string(),
            };
            (fuzzy::runs(&e.name, positions), sub, "Open")
        }
        Row::File(i) => {
            let item = &a.files[*i];
            (highlight::segments(item.highlighted_name.as_deref().unwrap_or(&item.name)), files::tilde(&item.path, &a.profile), "Open")
        }
        Row::Command(cmd) if cmd.is_empty() => (plain("Type a command to run".into()), "For example: > ipconfig /all".into(), ""),
        Row::Command(cmd) => (plain(cmd.clone()), "Run in Command Prompt".into(), "Run"),
        Row::Web(q) if is_url(q) => (plain(format!("Open {q}")), "In your default browser".into(), "Open"),
        Row::Web(q) => (plain(format!("Search the web for \u{201C}{q}\u{201D}")), "Google, in your default browser".into(), "Search"),
    }
}

/// Footer key hints for the selected row: (key, what it does).
fn hints(row: Option<&Row>) -> Vec<(&'static str, &'static str)> {
    let mut h = vec![("\u{2191}\u{2193}", "Select")];
    match row {
        Some(Row::Tip(_)) => h.push(("Enter", "Try it")),
        Some(Row::Calc(_)) => h.push(("Enter", "Copy")),
        Some(Row::App { idx, .. }) => {
            h.push(("Enter", "Open"));
            if app().apps[*idx].path.is_some() {
                h.push(("Ctrl+Enter", "File location"));
            }
            h.push(("Ctrl+Shift+Enter", "Admin"));
        }
        Some(Row::File(_)) => {
            h.push(("Enter", "Open"));
            h.push(("Ctrl+Enter", "Show in folder"));
        }
        Some(Row::Command(_)) => {
            h.push(("Enter", "Run"));
            h.push(("Ctrl+Shift+Enter", "Admin"));
        }
        _ => h.push(("Enter", "Open")),
    }
    h.push(("Esc", "Close"));
    h
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

    // The box: magnifier (accent once something is typed) and, while empty,
    // the hotkey as a keycap on the right.
    let box_h = scale(BOX_H);
    let typed = !a.query.is_empty();
    draw_glyph(dc, f.glyph, GLYPH_SEARCH, if typed { t.accent } else { t.dim }, RECT { left: scale(16), top: 0, right: scale(46), bottom: box_h });
    if !typed {
        let cw = hotkey_cap_width();
        draw_keycap(dc, f, t, hotkey_label(), w - scale(20) - cw, box_h / 2);
    }

    if !a.rows.is_empty() {
        let line = CreateSolidBrush(t.border);
        FillRect(dc, &RECT { left: 0, top: box_h, right: w, bottom: box_h + 1 }, line);
        DeleteObject(line);

        let (pad, icon) = (scale(PAD), scale(ICON));
        let text_x = pad + scale(14) + icon + scale(14);
        let (view_top, view_h) = (list_top(), view_height());
        let content = content_height();
        let scrollable = content > view_h;
        let row_right = w - pad - if scrollable { scale(8) } else { 0 };

        let saved = SaveDC(dc);
        IntersectClipRect(dc, 0, view_top, w, view_top + view_h);
        let mut y = view_top - a.scroll;
        for (i, row) in a.rows.iter().enumerate() {
            let rh = row.height();
            let (top, bottom) = (y, y + rh);
            y += rh;
            if bottom <= view_top || top >= view_top + view_h {
                continue;
            }
            let i = i as i32;
            if let Row::Header(label) = row {
                SelectObject(dc, f.header);
                SetTextColor(dc, t.dim);
                let mut hr = RECT { left: pad + scale(14), top, right: row_right, bottom: bottom - scale(5) };
                draw_text(dc, label, &mut hr, DT_LEFT | DT_BOTTOM);
                continue;
            }
            if i == a.sel || i == a.hover {
                let fill = if i == a.sel { t.sel } else { t.hover };
                fill_round(dc, RECT { left: pad, top, right: row_right, bottom }, scale(8), fill, None);
            }
            if i == a.sel {
                // Windows 11 style selection pill.
                fill_round(dc, RECT { left: pad + scale(1), top: top + scale(15), right: pad + scale(4), bottom: bottom - scale(15) }, scale(3), t.accent, None);
            }

            let ix = pad + scale(14);
            let iy = top + (rh - icon) / 2;
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
                        ImageList_Draw(a.image_list as _, idx, dc, ix + (icon - sys) / 2, top + (rh - sys) / 2, ILD_TRANSPARENT);
                    }
                }
                Row::Tip(_) => draw_glyph(dc, f.glyph_big, GLYPH_BULB, t.accent, glyph_rc),
                Row::Calc(_) => draw_glyph(dc, f.glyph_big, GLYPH_CALC, t.accent, glyph_rc),
                Row::Command(_) => draw_glyph(dc, f.glyph_big, GLYPH_COMMAND, t.accent, glyph_rc),
                Row::Web(_) => draw_glyph(dc, f.glyph_big, GLYPH_GLOBE, t.accent, glyph_rc),
                Row::Header(_) => {}
            }

            let (title, sub, action) = describe(row);
            let mut text_right = row_right - scale(14);
            if i == a.sel && !action.is_empty() {
                // "Open [Enter]" on the right of the selected row.
                SelectObject(dc, f.key);
                let cap_w = text_width(dc, "Enter") + scale(12);
                draw_keycap(dc, f, t, "Enter", text_right - cap_w, (top + bottom) / 2);
                SelectObject(dc, f.small);
                SetTextColor(dc, t.dim);
                let lw = text_width(dc, action);
                let mut ar = RECT { left: text_right - cap_w - scale(6) - lw, top, right: text_right - cap_w - scale(6), bottom };
                draw_text(dc, action, &mut ar, DT_RIGHT | DT_VCENTER);
                text_right -= cap_w + lw + scale(22);
            }
            let name_bottom = top + rh / 2 + scale(3);
            if let Row::Calc(_) = row {
                // The result, large.
                SelectObject(dc, f.result);
                SetTextColor(dc, t.text);
                let mut r = RECT { left: text_x, top: top + scale(2), right: text_right, bottom: name_bottom + scale(2) };
                draw_text(dc, &title[0].0, &mut r, DT_LEFT | DT_BOTTOM | DT_END_ELLIPSIS);
            } else {
                draw_runs(dc, f, t, &title, text_x, top + scale(4), text_right, name_bottom);
            }
            SelectObject(dc, f.small);
            SetTextColor(dc, t.dim);
            let mut sr = RECT { left: text_x, top: name_bottom + scale(1), right: text_right, bottom: bottom - scale(3) };
            draw_text(dc, &sub, &mut sr, DT_LEFT | DT_TOP | DT_PATH_ELLIPSIS | DT_END_ELLIPSIS);
        }
        RestoreDC(dc, saved);

        if scrollable {
            // A thin scrollbar thumb.
            let track = view_h - scale(8);
            let thumb = (track * view_h / content).max(scale(24));
            let ty = view_top + scale(4) + (track - thumb) * a.scroll / (content - view_h);
            fill_round(dc, RECT { left: w - scale(8), top: ty, right: w - scale(5), bottom: ty + thumb }, scale(3), t.border, None);
        }

        // Footer: a divider, key hints as keycaps, status on the right.
        let fy = h - scale(FOOTER_H);
        let line = CreateSolidBrush(t.border);
        FillRect(dc, &RECT { left: 0, top: fy, right: w, bottom: fy + 1 }, line);
        DeleteObject(line);
        let cy = fy + scale(FOOTER_H) / 2;
        SelectObject(dc, f.small);
        let status = match a.file_status {
            FileStatus::NotRunning => "Everything isn't running: no file results".to_string(),
            _ if a.rows.iter().filter(|r| r.selectable()).count() > 8 => {
                format!("{} results", a.rows.iter().filter(|r| r.selectable()).count())
            }
            _ => String::new(),
        };
        let status_w = if status.is_empty() { 0 } else { text_width(dc, &status) + scale(16) };
        let limit = w - scale(16) - status_w;
        let mut x = scale(16);
        for (key, what) in hints(a.rows.get(a.sel as usize)) {
            SelectObject(dc, f.key);
            let need = text_width(dc, key) + scale(12) + scale(6);
            SelectObject(dc, f.small);
            let need = need + text_width(dc, what) + scale(16);
            if x + need > limit {
                break;
            }
            x += draw_keycap(dc, f, t, key, x, cy) + scale(6);
            SelectObject(dc, f.small);
            SetTextColor(dc, t.dim);
            let mut r = RECT { left: x, top: fy, right: limit, bottom: h };
            draw_text(dc, what, &mut r, DT_LEFT | DT_VCENTER);
            x += text_width(dc, what) + scale(16);
        }
        SelectObject(dc, f.small);
        SetTextColor(dc, t.dim);
        let mut sr = RECT { left: limit, top: fy, right: w - scale(16), bottom: h };
        draw_text(dc, &status, &mut sr, DT_RIGHT | DT_VCENTER);
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
        if row.selectable() {
            summary.push_str(&format!("  {title} | {sub}\n"));
        } else {
            summary.push_str(&format!("# {title}\n"));
        }
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
