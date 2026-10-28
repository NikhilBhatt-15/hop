//! Parses hotkey strings like `Win+Alt+Space` or `ctrl+shift+f` into
//! `RegisterHotKey` modifiers and a virtual-key code.

/// `MOD_ALT`, `MOD_CONTROL`, `MOD_SHIFT`, `MOD_WIN` as `RegisterHotKey` takes them.
pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

/// Alt+Space: the launcher convention (PowerToys Run, Flow Launcher).
/// RegisterHotKey takes it over from the window menu. If PowerToys Run or
/// Flow already holds it, registration fails and hop says so.
pub const DEFAULT: &str = "Alt+Space";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

pub fn parse(s: &str) -> Result<Hotkey, String> {
    let mut modifiers = 0;
    let mut vk = None;
    for part in s.split('+').map(str::trim) {
        let p = part.to_ascii_lowercase();
        let m = match p.as_str() {
            "ctrl" | "control" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" | "windows" | "super" => MOD_WIN,
            _ => 0,
        };
        if m != 0 {
            modifiers |= m;
            continue;
        }
        if vk.is_some() {
            return Err(format!("\"{s}\" has more than one key"));
        }
        vk = Some(key_code(&p).ok_or_else(|| format!("unknown key \"{part}\" in \"{s}\""))?);
    }
    let vk = vk.ok_or_else(|| format!("\"{s}\" has no key, only modifiers"))?;
    if modifiers == 0 {
        return Err(format!("\"{s}\" needs at least one of Ctrl, Alt, Shift, Win"));
    }
    Ok(Hotkey { modifiers, vk })
}

fn key_code(k: &str) -> Option<u32> {
    let b = k.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_alphanumeric()) {
        return Some(b[0].to_ascii_uppercase() as u32); // VK_A..VK_Z, VK_0..VK_9
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1); // VK_F1..VK_F24
        }
    }
    Some(match k {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "`" | "backtick" | "grave" => 0xC0,
        "/" | "slash" => 0xBF,
        "." | "period" => 0xBE,
        "," | "comma" => 0xBC,
        ";" | "semicolon" => 0xBA,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(parse("Win+Alt+Space"), Ok(Hotkey { modifiers: MOD_WIN | MOD_ALT, vk: 0x20 }));
        assert_eq!(parse("ctrl + shift + f"), Ok(Hotkey { modifiers: MOD_CONTROL | MOD_SHIFT, vk: b'F' as u32 }));
        assert_eq!(parse("Alt+F12"), Ok(Hotkey { modifiers: MOD_ALT, vk: 0x7B }));
        assert_eq!(parse("Win+`"), Ok(Hotkey { modifiers: MOD_WIN, vk: 0xC0 }));
        assert!(parse(DEFAULT).is_ok());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("Space").is_err()); // no modifier
        assert!(parse("Ctrl+Alt").is_err()); // no key
        assert!(parse("Ctrl+A+B").is_err());
        assert!(parse("Ctrl+F25").is_err());
        assert!(parse("Ctrl+Banana").is_err());
    }
}
