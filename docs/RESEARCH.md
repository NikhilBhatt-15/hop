# Research: how Flow Launcher finds apps and handles its hotkey

Flow Launcher 2.1.4 (`Flow-Launcher-Portable.zip` from the project's GitHub release) was inspected
statically with REA's managed-code tools. Nothing from it was executed. These notes record what the
shipped binaries contain and how hop's design follows from them. No Flow Launcher code was copied;
hop is written from scratch in Rust.

| Artifact | SHA-256 |
| --- | --- |
| `ChefKeys.dll` | `329fd9af0c2e3956b8d57d25899e0c65195564eec031d74b526e001e43a2c975` |
| `Flow.Launcher.Infrastructure.dll` | `01bf63052e88989dfde0eac646ae2752afa6b677a0e45b5296c6e52e05e04c6d` |
| `Plugins/Flow.Launcher.Plugin.Program/Flow.Launcher.Plugin.Program.dll` | `ba261ba8c704e8cc304903e5db01ea05b7239a41f13232f26d2ed9275ece3922` |

Install size: 197 MB unpacked (the .NET runtime, WPF, and 12 bundled plugins).

## Hotkey

- `NHotkey.dll` and `NHotkey.Wpf.dll` reference `RegisterHotKey`/`UnregisterHotKey` and
  `HwndSource`, the standard WPF route to `WM_HOTKEY`.
- `ChefKeys.dll` declares P/Invokes for `SetWindowsHookEx`, `CallNextHookEx`, `UnhookWindowsHookEx`,
  `GetAsyncKeyState` and `keybd_event` (all in `user32.dll`), plus `GetModuleHandle`. This is a
  low-level keyboard hook. That is the usual way to make a bare Win key, or a combination with it, a hotkey,
  since `RegisterHotKey` can't claim the Win key alone. The injected `keybd_event` stops Windows from
  opening the Start menu when the hook swallows the Win key.
- `Flow.Launcher.Infrastructure.dll` declares a second `SetWindowsHookExW`, `SetWinEventHook`
  (tracking the foreground window), `SetForegroundWindow`, and `GetKeyboardLayoutList`,
  `ActivateKeyboardLayout`, `UnloadKeyboardLayout` (it can switch to an English layout while you type
  a query).

**hop:** `RegisterHotKey` only, default `` Win+` `` (with fallbacks if it's taken). No Win-key-alone hotkeys, so no keyboard hook runs
on every key press system-wide.

## Finding apps

The Program plugin's `#US` string literals show three sources:

- **Win32:** the suffixes `exe`, `lnk`, `url`, `appref-ms`, the registry key
  `SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths`, `Path` (the PATH variable), a
  `ShellLinkReader` that resolves shortcut targets, descriptions and arguments, and
  `InternetShortcut`/`URL`/`IconFile` for `.url` files. It also has protocol handlers for
  `steam://run/`, `steam://rungameid/` and `com.epicgames.launcher://apps/`.
- **UWP:** `CurrentUserPackages`, `AppxManifest.xml`, the appx manifest namespaces (2010, 2013,
  windows10, uap10, rescap), `Square44x44Logo`/`Square150x150Logo` logo lookup, and
  `shell:AppsFolder\` as the launch prefix, with `runas` for elevation.
- **Uninstaller filter:** `uninst.exe`, `unins000.exe`, `uninstall.exe`, and 22 words for "uninstall" in
  over 20 languages (`卸载`, `удалить`, `désinstaller`, `アンインストール`, …).
- **Cache:** `Win32.cache`, `UWP.cache`, and timing logs `Win32Program index cost` and
  `UWPProgram index cost`.

**hop:** a single enumeration of the shell's Applications folder (`FOLDERID_AppsFolder`), the list
the Start menu shows, which already merges desktop and Store apps. Every item's parsing name is a
launch target under `shell:AppsFolder\`, the prefix Flow uses for Store apps. On the test machine it
listed 249 apps in about one second with a cold cache, so hop indexes in the background and needs no
disk cache.

## Ranking

`Flow.Launcher.Infrastructure.StringMatcher` has the methods `FuzzySearch`, `FuzzyMatch`,
`IsAcronym`, `IsAcronymCount`, `IsAcronymChar`, `IsAcronymNumber`, `CountDistinctAcronymGroups`,
`CalculateClosestSpaceIndex`, `AllPreviousCharsMatched`, `AllQuerySubstringsMatched`,
`CalculateSearchScore`, and a `UserSettingSearchPrecision` property. That is a subsequence matcher
with acronym and word-boundary bonuses and a configurable cut-off. The core also has
`UserSelectedRecord` (selection history) and `TopMostRecord` (results pinned to the top for a query).

**hop:** fixed score tiers (exact, prefix, word start, all words, acronym, substring, letters in
order) plus a capped launch-count and recency bonus. The tiers keep the ranking predictable and easy
to test.

## Calculator and files

- The Calculator plugin bundles `Mages.Core.dll`. **hop:** a ~300-line recursive-descent parser.
- Flow ships `EverythingSDK/x64/Everything.dll`. **hop:** reuses evbar's implementation of the
  Everything SDK2 `WM_COPYDATA` protocol, so it needs no DLL.
