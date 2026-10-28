//! The app list, read from the shell's Applications folder (`shell:AppsFolder`).
//!
//! Flow Launcher builds its list from three sources: it scans Start-menu
//! folders for `.lnk`/`.exe`/`.url` files, reads `App Paths` from the
//! registry, and parses `AppxManifest.xml` for every UWP package. Then it
//! filters out uninstallers by name and caches the result to disk. The
//! Applications folder is the list the Start menu itself shows. It already
//! merges desktop and Store apps, and each item's parsing name is a launch
//! target: `shell:AppsFolder\<parsing name>` starts it whether the name is an
//! AUMID (`Microsoft.WindowsCalculator_8wekyb3d8bbwe!App`) or a path. So one
//! enumeration replaces all three scanners, and the launch path is the same
//! one Flow uses for UWP apps.

use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HANDLE, SIZE};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    IEnumShellItems, IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SHGetKnownFolderItem,
    SHGetKnownFolderPath,
    BHID_EnumItems, FOLDERID_AppsFolder, KF_FLAG_DEFAULT, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
    SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};

use crate::wide;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub name: String,
    /// Parsing name inside the Applications folder; also the history key.
    pub id: String,
    /// The file it runs, when the id names one (desktop apps).
    pub path: Option<String>,
}

impl App {
    /// What to pass to `ShellExecute` to start it.
    pub fn launch_target(&self) -> String {
        target(&self.id)
    }

    /// True for Store/packaged apps (an AUMID like `Pkg_hash!App`).
    pub fn is_packaged(&self) -> bool {
        self.id.contains('!') && !self.id.contains('\\')
    }
}

pub fn target(id: &str) -> String {
    format!("shell:AppsFolder\\{id}")
}

/// Desktop apps' ids are file paths, usually relative to a known folder:
/// `{6D809377-6AF0-444B-8957-A3773F02200E}\Vendor\app.exe` is under Program
/// Files. Returns the full path, or `None` for packaged apps, URLs and other
/// ids that are not files.
pub fn resolve_path(id: &str) -> Option<String> {
    if id.len() > 3 && id.as_bytes()[1] == b':' && id.as_bytes()[2] == b'\\' {
        return Some(id.to_string());
    }
    let (guid, rest) = id.strip_prefix('{')?.split_once("}\\")?;
    // windows-core's GUID::from(&str) panics on bad input, so parse it here:
    // 8-4-4-4-12 hex digits, read in the same order from_u128 expects.
    let dashes = [8, 13, 18, 23];
    let shape_ok = guid.len() == 36
        && guid.char_indices().all(|(i, c)| if dashes.contains(&i) { c == '-' } else { c.is_ascii_hexdigit() });
    if !shape_ok {
        return None;
    }
    let guid = windows::core::GUID::from_u128(u128::from_str_radix(&guid.replace('-', ""), 16).ok()?);
    unsafe {
        let p = SHGetKnownFolderPath(&guid, KF_FLAG_DEFAULT, HANDLE::default()).ok()?;
        let base = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        Some(format!("{}\\{rest}", base?))
    }
}

/// Shortcuts to documents and uninstallers that Start-menu folders often
/// contain. Flow drops uninstallers by name in over 20 languages; English
/// covers what turns up in the Applications folder in practice.
pub fn is_noise(name: &str, id: &str) -> bool {
    const DOCS: &[&str] = &[".txt", ".chm", ".pdf", ".rtf", ".htm", ".html", ".md", ".ini", ".log", ".doc", ".docx", ".hlp"];
    let (n, i) = (name.to_lowercase(), id.to_lowercase());
    n.contains("uninstall") || DOCS.iter().any(|ext| i.ends_with(ext))
}

unsafe fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    let p: PWSTR = item.GetDisplayName(kind).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// Lists the Applications folder. The calling thread must have COM
/// initialised. Takes up to a second with a cold cache, so hop runs it off
/// the UI thread.
pub fn enumerate() -> Vec<App> {
    let mut apps = Vec::new();
    unsafe {
        let Ok(folder) = SHGetKnownFolderItem::<_, IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, HANDLE::default()) else {
            return apps;
        };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else {
            return apps;
        };
        let mut seen = std::collections::HashSet::new();
        loop {
            let mut batch: [Option<IShellItem>; 32] = Default::default();
            let mut fetched = 0u32;
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            for item in batch.iter().take(fetched as usize).flatten() {
                let (Some(name), Some(id)) = (display_name(item, SIGDN_NORMALDISPLAY), display_name(item, SIGDN_PARENTRELATIVEPARSING)) else {
                    continue;
                };
                if name.is_empty() || is_noise(&name, &id) || !seen.insert(id.to_lowercase()) {
                    continue;
                }
                let path = resolve_path(&id);
                apps.push(App { name, id, path });
            }
        }
    }
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps
}

/// The app's icon as a 32-bit alpha bitmap of about `px` pixels, or null.
/// The caller owns the bitmap.
pub unsafe fn icon(id: &str, px: i32) -> windows_sys::Win32::Graphics::Gdi::HBITMAP {
    let path = wide(&target(id));
    let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(path.as_ptr()), None) else {
        return std::ptr::null_mut();
    };
    let Ok(factory) = item.cast::<IShellItemImageFactory>() else {
        return std::ptr::null_mut();
    };
    match factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK) {
        Ok(HBITMAP(h)) => h,
        Err(_) => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_docs_and_uninstallers() {
        assert!(is_noise("Uninstall Foo", "{7C5A40EF-A0FB-4BFC-874A-C0F2E0B9FA8E}\\Foo\\unins000.exe"));
        assert!(is_noise("Foo Readme", "C:\\Program Files\\Foo\\readme.txt"));
        assert!(!is_noise("Foo", "{6D809377-6AF0-444B-8957-A3773F02200E}\\Foo\\foo.exe"));
        assert!(!is_noise("Calculator", "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
    }

    #[test]
    fn packaged_ids() {
        let app = |id: &str| App { name: String::new(), id: id.to_string(), path: None };
        assert!(app("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App").is_packaged());
        assert!(!app("Chrome").is_packaged());
        assert!(!app("C:\\Tools\\a!b.exe").is_packaged());
        assert_eq!(app("Chrome").launch_target(), "shell:AppsFolder\\Chrome");
    }

    #[test]
    fn resolves_known_folder_paths() {
        let windir = std::env::var("SystemRoot").unwrap();
        // FOLDERID_System
        let p = resolve_path("{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\notepad.exe").unwrap();
        assert!(p.eq_ignore_ascii_case(&format!("{windir}\\System32\\notepad.exe")), "{p}");
        assert_eq!(resolve_path("C:\\Tools\\x.exe").as_deref(), Some("C:\\Tools\\x.exe"));
        assert_eq!(resolve_path("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"), None);
        assert_eq!(resolve_path("{not-a-guid}\\x.exe"), None);
        assert_eq!(resolve_path("https://example.com"), None);
    }

    #[test]
    fn enumerates_the_real_folder() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let apps = enumerate();
        // Every Windows install lists at least a few apps (Settings, File Explorer...).
        assert!(apps.len() > 3, "only {} apps", apps.len());
        assert!(apps.iter().all(|a| !a.name.is_empty() && !a.id.is_empty()));
    }
}
