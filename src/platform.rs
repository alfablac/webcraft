//! Single platform module: all OS differences live here.
use std::path::{Path, PathBuf};
use std::process::Command;

/// CREATE_NO_WINDOW: console-subsystem children (yt-dlp, ffmpeg, tasklist)
/// of this GUI-subsystem app otherwise flash a console window on Windows,
/// even with piped stdio. Grandchildren inherit hidden console.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn no_window_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

pub const HOST_NAME: &str = "com.webcraft.host";
/// ponytail: legacy host/protocol kept one rename cycle for installed extensions.
pub const LEGACY_HOST_NAME: &str = "com.downloadcraft.host";
pub const PROTOCOL: &str = "webcraft";
pub const LEGACY_PROTOCOL: &str = "downloadcraft";
pub const FIREFOX_EXT_ID: &str = "downloadcraft@downloadcraft.local";
pub const CHROME_EXT_ID: &str = "ajkokpbgjajmkenllekaokjbglbcingk";

/// Write capture-filter.json from Settings: native host reads it per
/// captured download; extension never stores filter (reflects app).
pub fn write_capture_filter(settings: &crate::model::Settings) {
    let exts: Vec<String> = settings
        .capture_exts
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        config_dir().join("capture-filter.json"),
        serde_json::json!({"exts": exts, "minBytes": settings.capture_min_mb.saturating_mul(1024 * 1024)}).to_string(),
    );
}

/// Write button-style.json from Settings: native host serves the resolved
/// video-button preset to extensions (getButtonStyle); app is source of truth.
pub fn write_button_style(settings: &crate::model::Settings) {
    let p = crate::model::resolve(&settings.button_preset);
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        config_dir().join("button-style.json"),
        serde_json::json!({"preset": p.id, "icon": p.icon, "w": p.w, "h": p.h, "radius": p.radius})
            .to_string(),
    );
}

/// Startup trace step: always appended (cheap, bounded growth by rotation
/// at 64KB) so "no window" reports carry last reached step in crash.log.
pub fn trace(step: &str) {
    let path = config_dir().join("startup.log");
    if std::fs::metadata(&path)
        .map(|m| m.len() > 65536)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "{ms} pid={} {step}", std::process::id());
    }
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "webcraft")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".webcraft"))
}
pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "webcraft")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".webcraft"))
}
pub fn cache_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "webcraft")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".webcraft-cache"))
}
pub fn default_download_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Downloads")
        })
}
pub fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

pub fn crash_log_path() -> PathBuf {
    config_dir().join("crash.log")
}

/// Append one record to crash.log. Never panics; safe inside panic hook.
pub fn log_crash(record: &str) {
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crash_log_path())
    {
        use std::io::Write;
        let _ = writeln!(f, "{record}");
    }
}

/// GUI-subsystem builds have no stderr: record every panic (GUI + workers)
/// to %APPDATA%/webcraft/crash.log with location + forced backtrace.
pub fn install_crash_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".into()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "?".into());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        log_crash(&format!(
            "--- panic ts={ts} pid={} ver={}\n  at {loc}\n  msg: {msg}\n{}",
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
            std::backtrace::Backtrace::force_capture()
        ));
    }));
}

/// Real main-window visibility (visible && !minimized), maintained by watcher
/// on Windows from HWND state; app writes it on egui-driven show/hide. Tray
/// label + hidden flag sync from this when native side changes state alone.
pub static WINDOW_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Restore/hide this process window when tray/second launch writes `show.request`.
/// Hidden viewport stops egui update, so request cannot be handled by app poll on Windows.
pub fn spawn_show_watcher() {
    #[cfg(windows)]
    std::thread::spawn(|| {
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        extern "system" {
            fn EnumWindows(cb: extern "system" fn(Hwnd, isize) -> Bool, p: isize) -> Bool;
            fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
            fn ShowWindow(hwnd: Hwnd, cmd: i32) -> Bool;
            fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
            fn BringWindowToTop(hwnd: Hwnd) -> Bool;
            fn SetWindowPos(
                hwnd: Hwnd,
                after: Hwnd,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> Bool;
            fn GetForegroundWindow() -> Hwnd;
            fn GetCurrentThreadId() -> u32;
            fn AttachThreadInput(a: u32, b: u32, attach: Bool) -> Bool;
            fn PostMessageW(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> Bool;
            fn IsWindowVisible(hwnd: Hwnd) -> Bool;
            fn IsIconic(hwnd: Hwnd) -> Bool;
            fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
        }
        // Windows denies SetForegroundWindow from non-foreground processes
        // (tray/host-triggered restore): attach to foreground thread so
        // restore actually takes keyboard focus, not just taskbar flash.
        // ponytail: no AllowSetForegroundWindow on tray thread; attach covers tray+host paths.
        unsafe fn force_foreground(hwnd: Hwnd) {
            ShowWindow(hwnd, 9); // SW_RESTORE
            let fg = GetForegroundWindow();
            let mut pid = 0;
            let fg_tid = if fg.is_null() {
                0
            } else {
                GetWindowThreadProcessId(fg, &mut pid)
            };
            let our_tid = GetCurrentThreadId();
            let attached =
                fg_tid != 0 && fg_tid != our_tid && AttachThreadInput(our_tid, fg_tid, 1) != 0;
            BringWindowToTop(hwnd);
            // HWND_TOPMOST then NOTOPMOST: raises above foreground app without staying always-on-top.
            SetWindowPos(hwnd, -1isize as Hwnd, 0, 0, 0, 0, 0x0002 | 0x0001 | 0x0040);
            SetWindowPos(hwnd, -2isize as Hwnd, 0, 0, 0, 0, 0x0002 | 0x0001 | 0x0040);
            SetForegroundWindow(hwnd);
            if attached {
                AttachThreadInput(our_tid, fg_tid, 0);
            }
        }
        extern "system" fn find(hwnd: *mut core::ffi::c_void, p: isize) -> i32 {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid != std::process::id() {
                    return 1;
                }
                // First PID window is not main window: tray-icon + winit own
                // hidden helper HWNDs in-process. Match eframe title instead.
                let mut buf = [0u16; 64];
                let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 64);
                if n > 0 && String::from_utf16_lossy(&buf[..n as usize]).starts_with("webcraft") {
                    *(p as *mut Hwnd) = hwnd;
                    return 0;
                }
            }
            1
        }
        fn our_hwnd() -> Option<Hwnd> {
            let mut hwnd: Hwnd = std::ptr::null_mut();
            unsafe {
                EnumWindows(find, &mut hwnd as *mut Hwnd as isize);
            }
            if hwnd.is_null() {
                None
            } else {
                Some(hwnd)
            }
        }
        fn shown(hwnd: Hwnd) -> bool {
            unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 }
        }
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let Some(hwnd) = our_hwnd() else { continue };
            let req = config_dir().join("show.request");
            if let Ok(content) = std::fs::read_to_string(&req) {
                if content.trim() == "toggle" {
                    // Toggle is non-idempotent: claim atomically so watcher
                    // xor app poll flips, never both (= no-op). Claiming here
                    // hides instantly even when egui idles at 5s repaint.
                    if claim_show_request().is_some() {
                        unsafe {
                            if shown(hwnd) {
                                ShowWindow(hwnd, 0);
                            } else {
                                force_foreground(hwnd);
                            }
                            PostMessageW(hwnd, 0x8001, 0, 0);
                        }
                    }
                } else {
                    // Do not claim show/add: app poll consumes it to clear
                    // hidden state / open Add dialog. Just show + wake.
                    unsafe {
                        // Always force: browser window holds foreground, plain
                        // ShowWindow on visible-but-background app won't focus.
                        force_foreground(hwnd);
                        PostMessageW(hwnd, 0x8001, 0, 0);
                    }
                }
            }
            // Track native changes (taskbar minimize/restore, toggle above):
            // wake app on change so tray label follows real visibility.
            let vis = shown(hwnd);
            if WINDOW_VISIBLE.swap(vis, std::sync::atomic::Ordering::Relaxed) != vis {
                unsafe {
                    PostMessageW(hwnd, 0x8001, 0, 0);
                }
            }
        }
    });
}
/// Atomically claim `show.request` (rename wins): exactly one of watcher /
/// app poll acts on a request. Returns content when claimed.
pub fn claim_show_request() -> Option<String> {
    let req = config_dir().join("show.request");
    let taken = config_dir().join("show.request.taken");
    std::fs::rename(&req, &taken).ok()?;
    let content = std::fs::read_to_string(&taken).unwrap_or_default();
    let _ = std::fs::remove_file(&taken);
    Some(content)
}

pub fn inbox_dir() -> PathBuf {
    config_dir().join("inbox")
}
/// Capture-cancel fallback files polled by native host (extension resumes browser download).
pub fn browser_resume_dir() -> PathBuf {
    config_dir().join("browser-resume")
}
pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [config_dir(), data_dir(), cache_dir(), inbox_dir()] {
        std::fs::create_dir_all(d)?;
    }
    Ok(())
}

/// One-time copy of legacy config (state.json + inbox + capture-filter.json)
/// into webcraft dir. Tries old downloadcraft dir first, then older idmcraft.
/// Runs only when new state absent and legacy state exists; never overwrites new data.
pub fn migrate_legacy() {
    let new_dir = config_dir();
    let legacy = directories::ProjectDirs::from("", "", "downloadcraft")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".downloadcraft"));
    migrate_legacy_from(&legacy, &new_dir);
    let older = directories::ProjectDirs::from("", "", "idmcraft")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".idmcraft"));
    migrate_legacy_from(&older, &new_dir);
}
pub fn migrate_legacy_from(legacy: &Path, new_dir: &Path) {
    if new_dir.join("state.json").exists() || !legacy.join("state.json").exists() {
        return;
    }
    let _ = std::fs::create_dir_all(new_dir);
    let _ = std::fs::copy(legacy.join("state.json"), new_dir.join("state.json"));
    if legacy.join("capture-filter.json").exists() && !new_dir.join("capture-filter.json").exists()
    {
        let _ = std::fs::copy(
            legacy.join("capture-filter.json"),
            new_dir.join("capture-filter.json"),
        );
    }
    let old_inbox = legacy.join("inbox");
    if old_inbox.is_dir() {
        let new_inbox = new_dir.join("inbox");
        let _ = std::fs::create_dir_all(&new_inbox);
        if let Ok(rd) = std::fs::read_dir(&old_inbox) {
            for e in rd.flatten() {
                let _ = std::fs::copy(e.path(), new_inbox.join(e.file_name()));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browser {
    Chrome,
    Edge,
    Brave,
    Chromium,
    Vivaldi,
    Firefox,
}

pub fn all_browsers() -> [Browser; 6] {
    [
        Browser::Chrome,
        Browser::Edge,
        Browser::Brave,
        Browser::Chromium,
        Browser::Vivaldi,
        Browser::Firefox,
    ]
}

/// Native-messaging manifest paths relative to a home dir. Pure: unit-tested with mocked home.
pub fn native_host_manifest_paths(home: &Path) -> Vec<(Browser, PathBuf)> {
    let mut v = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // Windows uses registry pointing at manifest in install dir; path fn returns manifest file location convention.
        let base = home.join("AppData").join("Local").join("webcraft");
        for b in all_browsers() {
            v.push((
                b,
                base.join(if b == Browser::Firefox {
                    "host-firefox.json"
                } else {
                    "host.json"
                }),
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let s = home.join("Library").join("Application Support");
        for name in [HOST_NAME, LEGACY_HOST_NAME] {
            v.push((
                Browser::Chrome,
                s.join("Google/Chrome/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Edge,
                s.join("Microsoft Edge/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Brave,
                s.join("BraveSoftware/Brave-Browser/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Chromium,
                s.join("Chromium/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Vivaldi,
                s.join("Vivaldi/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Firefox,
                s.join("Mozilla/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
        }
    }
    #[cfg(target_os = "linux")]
    {
        let c = home.join(".config");
        for name in [HOST_NAME, LEGACY_HOST_NAME] {
            v.push((
                Browser::Chrome,
                c.join("google-chrome/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Chromium,
                c.join("chromium/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Brave,
                c.join("BraveSoftware/Brave-Browser/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Edge,
                c.join("microsoft-edge/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Vivaldi,
                c.join("vivaldi/NativeMessagingHosts")
                    .join(host_manifest_filename(name)),
            ));
            v.push((
                Browser::Firefox,
                home.join(".mozilla/native-messaging-hosts")
                    .join(host_manifest_filename(name)),
            ));
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        for name in [HOST_NAME, LEGACY_HOST_NAME] {
            for b in all_browsers() {
                v.push((b, home.join(format!("{name}.json"))));
            }
        }
    }
    v
}

pub fn host_manifest_json(host_exe: &Path, firefox: bool) -> String {
    host_manifest_json_for(HOST_NAME, host_exe, firefox)
}
pub fn host_manifest_json_for(host_name: &str, host_exe: &Path, firefox: bool) -> String {
    if firefox {
        format!("{{\n  \"name\": \"{host_name}\",\n  \"description\": \"webcraft native host\",\n  \"type\": \"stdio\",\n  \"allowed_extensions\": [\"{FIREFOX_EXT_ID}\"],\n  \"path\": \"{}\"\n}}\n", host_exe.display().to_string().replace('\\', "\\\\"))
    } else {
        format!("{{\n  \"name\": \"{host_name}\",\n  \"description\": \"webcraft native host\",\n  \"type\": \"stdio\",\n  \"allowed_origins\": [\"chrome-extension://{CHROME_EXT_ID}/\"],\n  \"path\": \"{}\"\n}}\n", host_exe.display().to_string().replace('\\', "\\\\"))
    }
}
/// Manifest filename inside a NativeMessagingHosts dir for a host name.
/// ponytail: legacy variant only differs by suffix; drop with LEGACY_HOST_NAME.
#[cfg(not(target_os = "windows"))]
fn host_manifest_filename(host_name: &str) -> String {
    format!("{host_name}.json")
}

/// Register native messaging host for all browsers, per-user, no admin.
pub fn register_native_hosts(host_exe: &Path) -> std::io::Result<Vec<PathBuf>> {
    #[cfg(not(target_os = "windows"))]
    let home = std::env::home_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
    let mut written = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // Manifest lives next to host exe; registry points to it.
        let dir = host_exe
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let chrome_manifest = dir.join("host.json");
        let moz_manifest = dir.join("host-firefox.json");
        let legacy_chrome = dir.join("host-legacy.json");
        let legacy_moz = dir.join("host-firefox-legacy.json");
        std::fs::write(&chrome_manifest, host_manifest_json(host_exe, false))?;
        std::fs::write(&moz_manifest, host_manifest_json(host_exe, true))?;
        std::fs::write(
            &legacy_chrome,
            host_manifest_json_for(LEGACY_HOST_NAME, host_exe, false),
        )?;
        std::fs::write(
            &legacy_moz,
            host_manifest_json_for(LEGACY_HOST_NAME, host_exe, true),
        )?;
        register_windows_native_host(&chrome_manifest, &moz_manifest);
        register_windows_native_host_named(LEGACY_HOST_NAME, &legacy_chrome, &legacy_moz);
        written.push(chrome_manifest);
        written.push(moz_manifest);
        written.push(legacy_chrome);
        written.push(legacy_moz);
    }
    #[cfg(not(target_os = "windows"))]
    {
        for (browser, path) in native_host_manifest_paths(&home) {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            let name = if path
                .file_name()
                .map(|f| f.to_string_lossy().contains(LEGACY_HOST_NAME))
                .unwrap_or(false)
            {
                LEGACY_HOST_NAME
            } else {
                HOST_NAME
            };
            std::fs::write(
                &path,
                host_manifest_json_for(name, host_exe, browser == Browser::Firefox),
            )?;
            written.push(path);
        }
    }
    Ok(written)
}

pub fn unregister_native_hosts() -> std::io::Result<()> {
    #[cfg(not(target_os = "windows"))]
    let home = std::env::home_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
    #[cfg(not(target_os = "windows"))]
    for (_, path) in native_host_manifest_paths(&home) {
        let _ = std::fs::remove_file(path);
    }
    #[cfg(target_os = "windows")]
    unregister_windows_native_host();
    Ok(())
}

#[cfg(target_os = "windows")]
fn register_windows_native_host(chrome_manifest: &Path, moz_manifest: &Path) {
    register_windows_native_host_named(HOST_NAME, chrome_manifest, moz_manifest);
}
#[cfg(target_os = "windows")]
fn register_windows_native_host_named(
    host_name: &str,
    chrome_manifest: &Path,
    moz_manifest: &Path,
) {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for root in [
        "Software\\Google\\Chrome\\NativeMessagingHosts",
        "Software\\Microsoft\\Edge\\NativeMessagingHosts",
        "Software\\BraveSoftware\\Brave-Browser\\NativeMessagingHosts",
        "Software\\Chromium\\NativeMessagingHosts",
        "Software\\Vivaldi\\NativeMessagingHosts",
    ] {
        if let Ok((k, _)) = hkcu.create_subkey(format!("{root}\\{host_name}")) {
            let _ = k.set_value("", &chrome_manifest.to_string_lossy().as_ref());
        }
    }
    if let Ok((k, _)) = hkcu.create_subkey(format!(
        "Software\\Mozilla\\NativeMessagingHosts\\{host_name}"
    )) {
        let _ = k.set_value("", &moz_manifest.to_string_lossy().as_ref());
    }
}
#[cfg(target_os = "windows")]
fn register_windows_protocol() -> std::io::Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = std::env::current_exe()?;
    for scheme in [PROTOCOL, LEGACY_PROTOCOL] {
        let (proto, _) = hkcu.create_subkey(format!("Software\\Classes\\{scheme}"))?;
        proto.set_value("", &format!("URL:{scheme} Protocol"))?;
        proto.set_value("URL Protocol", &"")?;
        let (cmd, _) = proto.create_subkey("shell\\open\\command")?;
        cmd.set_value("", &format!("\"{}\" \"%1\"", exe.display()).as_str())?;
    }
    Ok(())
}
#[cfg(target_os = "windows")]
fn unregister_windows_native_host() {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for root in [
        "Software\\Google\\Chrome\\NativeMessagingHosts",
        "Software\\Microsoft\\Edge\\NativeMessagingHosts",
        "Software\\BraveSoftware\\Brave-Browser\\NativeMessagingHosts",
        "Software\\Chromium\\NativeMessagingHosts",
        "Software\\Vivaldi\\NativeMessagingHosts",
        "Software\\Mozilla\\NativeMessagingHosts",
    ] {
        if let Ok(k) = hkcu.open_subkey_with_flags(root, winreg::enums::KEY_WRITE) {
            let _ = k.delete_subkey_all(HOST_NAME);
            let _ = k.delete_subkey_all(LEGACY_HOST_NAME);
        }
    }
    if let Ok(k) = hkcu.open_subkey_with_flags("Software\\Classes", winreg::enums::KEY_WRITE) {
        let _ = k.delete_subkey_all(PROTOCOL);
        let _ = k.delete_subkey_all(LEGACY_PROTOCOL);
    }
}

// ---------- autostart ----------
pub fn autostart_path_for(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/LaunchAgents/com.webcraft.plist")
    }
    #[cfg(target_os = "linux")]
    {
        home.join(".config/autostart/webcraft.desktop")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        home.join("webcraft-autostart")
    }
}

pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "windows")]
    {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (k, _) = hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")?;
        if enabled {
            k.set_value(
                "webcraft",
                &format!("\"{}\" --autostart", exe.display()).as_str(),
            )?;
        } else {
            let _ = k.delete_value("webcraft");
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::home_dir()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
        let path = autostart_path_for(&home);
        if enabled {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&path, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>com.webcraft</string><key>ProgramArguments</key><array><string>{}</string><string>--autostart</string></array><key>RunAtLoad</key><true/></dict></plist>\n", exe.display()))?;
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::home_dir()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
        let path = autostart_path_for(&home);
        if enabled {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&path, format!("[Desktop Entry]\nType=Application\nName=webcraft\nExec=\"{}\" --autostart\nX-GNOME-Autostart-enabled=true\nNoDisplay=false\n", exe.display()))?;
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

pub fn autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        return hkcu
            .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .and_then(|k| k.get_value::<String, _>("webcraft"))
            .is_ok();
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if let Some(home) = std::env::home_dir() {
            return autostart_path_for(&home).exists();
        }
        return false;
    }
    #[allow(unreachable_code)]
    false
}

pub fn register_protocol() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let home = std::env::home_dir()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
        let dir = home.join(".local/share/applications");
        std::fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()?;
        std::fs::write(dir.join("webcraft.desktop"), format!("[Desktop Entry]\nType=Application\nName=webcraft\nExec=\"{}\" %u\nMimeType=x-scheme-handler/{PROTOCOL};x-scheme-handler/{LEGACY_PROTOCOL};\nNoDisplay=true\n", exe.display()))?;
        let _ = Command::new("xdg-mime")
            .args([
                "default",
                "webcraft.desktop",
                &format!("x-scheme-handler/{PROTOCOL}"),
            ])
            .status();
        let _ = Command::new("xdg-mime")
            .args([
                "default",
                "webcraft.desktop",
                &format!("x-scheme-handler/{LEGACY_PROTOCOL}"),
            ])
            .status();
    }
    #[cfg(target_os = "windows")]
    {
        register_windows_protocol()?;
    }
    // macOS: URL schemes declared in app bundle Info.plist (packaging/Info.plist).
    Ok(())
}

/// Open external URL in default browser (OAuth sign-in).
/// Windows: `cmd start` splits unquoted URLs at `&` (drops client_id= etc).
/// Use rundll32 FileProtocolHandler: URL passed as one argv, no shell.
pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        if no_window(&mut cmd).spawn().is_err() {
            // Fallback: cmd start with caret-escaped ampersands.
            let escaped = url.replace('&', "^&");
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "start", "", &escaped]);
            let _ = no_window(&mut cmd).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
    }
}

pub fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", &path.to_string_lossy()]);
        no_window(&mut cmd).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}
pub fn reveal_path(path: &Path) -> std::io::Result<()> {
    // File moved/deleted after completion: open parent dir instead of
    // letting explorer fall back to Documents on nonexistent path.
    let target;
    let path = if path.exists() {
        path
    } else {
        target = path
            .parent()
            .filter(|p| p.is_dir())
            .map(|p| p.to_path_buf());
        match &target {
            Some(p) => p.as_path(),
            None => return open_path(path),
        }
    };
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // raw_arg: quotes must wrap path only. Command::arg would quote whole
        // "/select,<path>" arg when path has spaces; explorer rejects that
        // form and opens Documents instead.
        Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").args(["-R"]).arg(path).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn()?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Shutdown,
    Sleep,
}
/// Build power command without executing. Unit-tested per OS construction.
pub fn power_command(action: PowerAction) -> (String, Vec<String>) {
    #[cfg(target_os = "windows")]
    {
        match action {
            PowerAction::Shutdown => (
                "shutdown.exe".into(),
                vec!["/s".into(), "/t".into(), "0".into()],
            ),
            PowerAction::Sleep => (
                "rundll32.exe".into(),
                vec![
                    "powrprof.dll,SetSuspendState".into(),
                    "0".into(),
                    "1".into(),
                    "0".into(),
                ],
            ),
        }
    }
    #[cfg(target_os = "macos")]
    {
        match action {
            PowerAction::Shutdown => (
                "osascript".into(),
                vec![
                    "-e".into(),
                    "tell application \"System Events\" to shut down".into(),
                ],
            ),
            PowerAction::Sleep => ("pmset".into(), vec!["sleepnow".into()]),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match action {
            PowerAction::Shutdown => ("systemctl".into(), vec!["poweroff".into()]),
            PowerAction::Sleep => ("systemctl".into(), vec!["suspend".into()]),
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        ("".into(), vec![])
    }
}
pub fn execute_power(action: PowerAction) -> std::io::Result<()> {
    let (p, a) = power_command(action);
    let mut cmd = Command::new(p);
    cmd.args(a);
    no_window(&mut cmd).spawn()?;
    Ok(())
}

/// Desktop notification (Windows toast / macOS / Linux dbus via notify-rust).
/// Best-effort: failure logged nowhere, never blocks download flow.
/// ponytail: no focus suppression; OS toast fires even when window focused.
/// Add WINDOW_VISIBLE/focus check here if that becomes annoying.
pub fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .appname("webcraft")
        .show();
}

// ---------- embedded child window (mpv --wid alternative: reparent) ----------

/// Style transform for embed_child: pure, unit-tested.
/// Windows-only production caller; test keeps it live elsewhere.
#[cfg(any(windows, test))]
fn embedded_style(style: u32) -> u32 {
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_THICKFRAME: u32 = 0x0004_0000;
    (style & !WS_POPUP & !WS_CAPTION & !WS_THICKFRAME) | WS_CHILD | WS_VISIBLE
}

/// First top-level window owned by `pid` whose title starts with
/// `title_prefix`. HWND returned as isize. None on non-Windows / not found.
#[allow(unreachable_code)] // windows arm returns directly; tail is non-windows path
pub fn top_level_hwnd(pid: u32, title_prefix: &str) -> Option<isize> {
    #[cfg(windows)]
    {
        if pid == 0 {
            return None;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        extern "system" {
            fn EnumWindows(cb: extern "system" fn(Hwnd, isize) -> Bool, p: isize) -> Bool;
            fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
            fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
        }
        struct Search {
            pid: u32,
            prefix: Vec<u16>,
            found: Hwnd,
        }
        extern "system" fn find(hwnd: Hwnd, p: isize) -> Bool {
            unsafe {
                let s = &mut *(p as *mut Search);
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid != s.pid {
                    return 1;
                }
                let mut buf = [0u16; 256];
                let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 256);
                if n > 0 && buf[..n as usize].starts_with(&s.prefix) {
                    s.found = hwnd;
                    return 0;
                }
            }
            1
        }
        let mut search = Search {
            pid,
            prefix: title_prefix.encode_utf16().collect(),
            found: std::ptr::null_mut(),
        };
        unsafe {
            EnumWindows(find, &mut search as *mut Search as isize);
        }
        if search.found.is_null() {
            return None;
        }
        Some(search.found as isize)
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, title_prefix);
        None
    }
}

/// Raise top-level window of `pid` whose title starts with `title_prefix`
/// above foreground app (browser holds foreground; egui Focus alone denied).
/// Same force sequence as spawn_show_watcher. False when not found.
/// No-op on non-Windows.
#[allow(unreachable_code)] // windows arm returns directly; tail is non-windows path
pub fn raise_window(pid: u32, title_prefix: &str) -> bool {
    #[cfg(windows)]
    {
        if pid == 0 || title_prefix.is_empty() {
            return false;
        }
        let Some(found) = top_level_hwnd(pid, title_prefix) else {
            return false;
        };
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        extern "system" {
            fn ShowWindow(hwnd: Hwnd, cmd: i32) -> Bool;
            fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
            fn BringWindowToTop(hwnd: Hwnd) -> Bool;
            fn SetWindowPos(
                hwnd: Hwnd,
                after: Hwnd,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> Bool;
            fn GetForegroundWindow() -> Hwnd;
            fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
            fn GetCurrentThreadId() -> u32;
            fn AttachThreadInput(a: u32, b: u32, attach: Bool) -> Bool;
            fn SetFocus(hwnd: Hwnd) -> Hwnd;
        }
        unsafe {
            let hwnd = found as Hwnd;
            ShowWindow(hwnd, 9); // SW_RESTORE
            let fg = GetForegroundWindow();
            let mut fg_pid = 0;
            let fg_tid = if fg.is_null() {
                0
            } else {
                GetWindowThreadProcessId(fg, &mut fg_pid)
            };
            let our_tid = GetCurrentThreadId();
            let attached =
                fg_tid != 0 && fg_tid != our_tid && AttachThreadInput(our_tid, fg_tid, 1) != 0;
            BringWindowToTop(hwnd);
            // HWND_TOPMOST then NOTOPMOST: raises above foreground app without staying always-on-top.
            SetWindowPos(hwnd, -1isize as Hwnd, 0, 0, 0, 0, 0x0002 | 0x0001 | 0x0040);
            SetWindowPos(hwnd, -2isize as Hwnd, 0, 0, 0, 0, 0x0002 | 0x0001 | 0x0040);
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
            if attached {
                AttachThreadInput(our_tid, fg_tid, 0);
            }
        }
        return true;
    }
    let _ = (pid, title_prefix);
    false
}

/// Reparent `child` into `parent`: strip popup/caption/frame, set
/// WS_CHILD|WS_VISIBLE, SetParent, initial 1x1 rect (caller resizes via
/// set_embedded_rect). No-op on non-Windows / null handles.
pub fn embed_child(child: isize, parent: isize) {
    #[cfg(windows)]
    {
        if child == 0 || parent == 0 {
            return;
        }
        type Hwnd = *mut core::ffi::c_void;
        extern "system" {
            fn GetWindowLongW(hwnd: Hwnd, idx: i32) -> i32;
            fn SetWindowLongW(hwnd: Hwnd, idx: i32, val: i32) -> i32;
            fn SetParent(child: Hwnd, parent: Hwnd) -> Hwnd;
        }
        const GWL_STYLE: i32 = -16;
        unsafe {
            let hwnd = child as Hwnd;
            let style = embedded_style(GetWindowLongW(hwnd, GWL_STYLE) as u32);
            SetWindowLongW(hwnd, GWL_STYLE, style as i32);
            SetParent(hwnd, parent as Hwnd);
        }
        set_embedded_rect(child, 0, 0, 1, 1);
    }
    #[cfg(not(windows))]
    {
        let _ = (child, parent);
    }
}

/// Move/resize an embedded child without activating or changing z-order.
pub fn set_embedded_rect(hwnd: isize, x: i32, y: i32, w: i32, h: i32) {
    #[cfg(windows)]
    {
        if hwnd == 0 {
            return;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        extern "system" {
            fn SetWindowPos(
                hwnd: Hwnd,
                after: Hwnd,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> Bool;
        }
        const SWP_NOACTIVATE: u32 = 0x0010;
        const SWP_NOZORDER: u32 = 0x0004;
        unsafe {
            SetWindowPos(
                hwnd as Hwnd,
                std::ptr::null_mut(),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, x, y, w, h);
    }
}

pub fn focus_window(hwnd: isize) {
    #[cfg(windows)]
    {
        if hwnd == 0 {
            return;
        }
        type Hwnd = *mut core::ffi::c_void;
        extern "system" {
            fn SetFocus(hwnd: Hwnd) -> Hwnd;
        }
        unsafe {
            SetFocus(hwnd as Hwnd);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

// ---------- dedicated video child window (mpv --wid target) ----------
// Plain Win32 child, not egui GL surface: Windows composites child HWND
// above parent client area, mpv paints into it undisturbed; egui keeps
// painting only surrounding panels, whose rect caller excludes from child.

/// Create 320x240 `WebcraftVideoChild` (WS_CHILD|WS_VISIBLE|WS_CLIPSIBLINGS)
/// under `parent` HWND. Its HWND goes to mpv `--wid`. None on invalid
/// parent, creation failure, or non-Windows.
#[allow(unreachable_code)] // windows arm returns directly; tail is non-windows path
pub fn create_video_child(parent: isize) -> Option<isize> {
    #[cfg(windows)]
    {
        if parent == 0 {
            return None;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;
        #[repr(C)]
        struct WndClassW {
            style: u32,
            lpfn_wnd_proc: Option<WndProc>,
            cb_cls_extra: i32,
            cb_wnd_extra: i32,
            h_instance: Hwnd,
            h_icon: Hwnd,
            h_cursor: Hwnd,
            hbr_background: Hwnd,
            lpsz_menu_name: *const u16,
            lpsz_class_name: *const u16,
        }
        extern "system" {
            fn RegisterClassW(class: *const WndClassW) -> u16;
            fn CreateWindowExW(
                ex_style: u32,
                class: *const u16,
                name: *const u16,
                style: u32,
                x: i32,
                y: i32,
                w: i32,
                h: i32,
                parent: Hwnd,
                menu: Hwnd,
                instance: Hwnd,
                param: *mut core::ffi::c_void,
            ) -> Hwnd;
            fn DefWindowProcW(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> isize;
            fn GetModuleHandleW(name: *const u16) -> Hwnd;
            fn IsWindow(hwnd: Hwnd) -> Bool;
        }
        const WS_CHILD: u32 = 0x4000_0000;
        const WS_VISIBLE: u32 = 0x1000_0000;
        const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
        static REGISTERED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        let class: Vec<u16> = "WebcraftVideoChild\0".encode_utf16().collect();
        let title: [u16; 1] = [0];
        unsafe {
            if IsWindow(parent as Hwnd) == 0 {
                return None;
            }
            let instance = GetModuleHandleW(std::ptr::null());
            if !REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let wc = WndClassW {
                    style: 0,
                    lpfn_wnd_proc: Some(DefWindowProcW),
                    cb_cls_extra: 0,
                    cb_wnd_extra: 0,
                    h_instance: instance,
                    h_icon: std::ptr::null_mut(),
                    h_cursor: std::ptr::null_mut(),
                    // Null brush: no GDI erase, mpv owns every pixel.
                    hbr_background: std::ptr::null_mut(),
                    lpsz_menu_name: std::ptr::null(),
                    lpsz_class_name: class.as_ptr(),
                };
                // 0 with ERROR_CLASS_ALREADY_EXISTS is fine; CreateWindowExW decides.
                let _ = RegisterClassW(&wc);
            }
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                0,
                0,
                320,
                240,
                parent as Hwnd,
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return None;
            }
            return Some(hwnd as isize);
        }
    }
    let _ = parent;
    None
}

/// Move/resize video child in parent client coords (physical px). Hidden
/// when `w <= 0 || h <= 0`. No-op on null handle / non-Windows.
pub fn set_video_rect(child: isize, x: i32, y: i32, w: i32, h: i32) {
    #[cfg(windows)]
    {
        if child == 0 {
            return;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        extern "system" {
            fn SetWindowPos(
                hwnd: Hwnd,
                after: Hwnd,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> Bool;
            fn ShowWindow(hwnd: Hwnd, cmd: i32) -> Bool;
        }
        const SWP_NOACTIVATE: u32 = 0x0010;
        const SWP_SHOWWINDOW: u32 = 0x0040;
        unsafe {
            if w <= 0 || h <= 0 {
                ShowWindow(child as Hwnd, 0); // SW_HIDE
            } else {
                // HWND_TOP (0) + SWP_SHOWWINDOW: place above parent GL
                // surface and ensure visible in one call.
                SetWindowPos(
                    child as Hwnd,
                    std::ptr::null_mut(),
                    x,
                    y,
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (child, x, y, w, h);
    }
}

/// Destroy video child. Caller must stop mpv rendering into it first.
/// No-op on null handle / non-Windows.
pub fn destroy_video_child(child: isize) {
    #[cfg(windows)]
    {
        if child == 0 {
            return;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        extern "system" {
            fn DestroyWindow(hwnd: Hwnd) -> Bool;
        }
        unsafe {
            DestroyWindow(child as Hwnd);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = child;
    }
}

/// Screen coords of `hwnd` client-area origin (ClientToScreen on 0,0).
/// Caller places child at `rect_screen - client_origin(parent)`. None on
/// invalid handle / failure / non-Windows.
#[allow(unreachable_code)] // windows arm returns directly; tail is non-windows path
pub fn client_origin(hwnd: isize) -> Option<(i32, i32)> {
    #[cfg(windows)]
    {
        if hwnd == 0 {
            return None;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        #[repr(C)]
        struct Point {
            x: i32,
            y: i32,
        }
        extern "system" {
            fn ClientToScreen(hwnd: Hwnd, pt: *mut Point) -> Bool;
            fn IsWindow(hwnd: Hwnd) -> Bool;
        }
        unsafe {
            if IsWindow(hwnd as Hwnd) == 0 {
                return None;
            }
            let mut pt = Point { x: 0, y: 0 };
            if ClientToScreen(hwnd as Hwnd, &mut pt) == 0 {
                return None;
            }
            return Some((pt.x, pt.y));
        }
    }
    let _ = hwnd;
    None
}

/// Raw GetWindowRect as `(left, top, right, bottom)` screen coords
/// (right/bottom exclusive). None on invalid handle / failure / non-Windows.
#[allow(unreachable_code)] // windows arm returns directly; tail is non-windows path
pub fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    #[cfg(windows)]
    {
        if hwnd == 0 {
            return None;
        }
        type Hwnd = *mut core::ffi::c_void;
        type Bool = i32;
        #[repr(C)]
        struct Rect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        extern "system" {
            fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> Bool;
            fn IsWindow(hwnd: Hwnd) -> Bool;
        }
        unsafe {
            if IsWindow(hwnd as Hwnd) == 0 {
                return None;
            }
            let mut r = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd as Hwnd, &mut r) == 0 {
                return None;
            }
            return Some((r.left, r.top, r.right, r.bottom));
        }
    }
    let _ = hwnd;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_paths_mock_home() {
        let paths = native_host_manifest_paths(Path::new("/mockhome"));
        assert_eq!(paths.len(), if cfg!(windows) { 6 } else { 12 });
        for (_, p) in &paths {
            let s = p.to_string_lossy().replace('\\', "/");
            assert!(s.contains("/mockhome/") || s.contains("mockhome"), "{s}");
            assert!(
                s.contains(HOST_NAME) || s.contains(LEGACY_HOST_NAME) || s.contains("host"),
                "{s}"
            );
        }
    }
    #[test]
    fn migrate_legacy_copies_only_when_new_empty() {
        let base = std::env::temp_dir().join(format!("webcraft-mig-{}", std::process::id()));
        let legacy = base.join("old");
        let newd = base.join("new");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(legacy.join("inbox")).unwrap();
        std::fs::write(legacy.join("state.json"), "{}").unwrap();
        std::fs::write(legacy.join("inbox").join("a.txt"), "url=x").unwrap();
        migrate_legacy_from(&legacy, &newd);
        assert!(newd.join("state.json").exists());
        assert!(newd.join("inbox").join("a.txt").exists());
        // second run must not overwrite new state
        std::fs::write(newd.join("state.json"), "{\"v\":2}").unwrap();
        migrate_legacy_from(&legacy, &newd);
        assert_eq!(
            std::fs::read_to_string(newd.join("state.json")).unwrap(),
            "{\"v\":2}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn autostart_path_mock() {
        let p = autostart_path_for(Path::new("/h"));
        assert!(!p.to_string_lossy().is_empty());
    }
    #[test]
    fn power_cmd_nonempty() {
        let (p, _) = power_command(PowerAction::Shutdown);
        assert!(!p.is_empty());
    }
    #[test]
    fn embedded_style_math() {
        // WS_POPUP(0x80000000)|WS_CAPTION(0x00C00000)|WS_VISIBLE|other bit
        let s = embedded_style(0x8000_0000 | 0x00C0_0000 | 0x0004_0000 | 0x0000_0002);
        assert_eq!(s & 0x8000_0000, 0, "popup cleared");
        assert_eq!(s & 0x00C0_0000, 0, "caption cleared");
        assert_eq!(s & 0x0004_0000, 0, "thickframe cleared");
        assert_eq!(s & 0x4000_0000, 0x4000_0000, "child set");
        assert_eq!(s & 0x1000_0000, 0x1000_0000, "visible set");
        assert_eq!(s & 0x0000_0002, 0x0000_0002, "unrelated bits kept");
        assert_eq!(embedded_style(0), 0x4000_0000 | 0x1000_0000);
    }
    #[test]
    fn top_level_hwnd_zero_pid_none() {
        assert_eq!(top_level_hwnd(0, ""), None);
        assert_eq!(top_level_hwnd(0, "webcraft"), None);
    }
    #[test]
    fn manifest_json_content() {
        let j = host_manifest_json(Path::new("/tmp/host"), false);
        assert!(j.contains(HOST_NAME));
        assert!(j.contains(&format!("chrome-extension://{CHROME_EXT_ID}/")));
        let jf = host_manifest_json(Path::new("/tmp/host"), true);
        assert!(jf.contains(FIREFOX_EXT_ID));
    }
    #[test]
    fn chrome_ext_id_matches_manifest_key() {
        fn b64(s: &str) -> Vec<u8> {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let vals: Vec<u32> = s
                .bytes()
                .filter(|&b| b != b'=')
                .map(|c| u32::try_from(T.iter().position(|&x| x == c).unwrap()).unwrap())
                .collect();
            let full = vals.len() * 6 / 8;
            vals.chunks(4)
                .flat_map(|c| {
                    let n = c.iter().fold(0u32, |a, &v| a * 64 + v);
                    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
                })
                .take(full)
                .collect()
        }
        let m = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/extensions/chromium/manifest.json"
        ))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        let key = v["key"].as_str().expect("pinned key");
        let der = b64(key);
        use sha2::Digest;
        let d = sha2::Sha256::digest(&der);
        let id: String = d[..16]
            .iter()
            .flat_map(|b| [b >> 4, b & 15])
            .map(|n| (b'a' + n) as char)
            .collect();
        assert_eq!(id, CHROME_EXT_ID, "update CHROME_EXT_ID or manifest key");
    }
    #[test]
    fn video_child_invalid_handles_no_panic() {
        assert_eq!(create_video_child(0), None);
        // Non-null but bogus parent: Windows IsWindow gate -> None, no panic.
        assert_eq!(create_video_child(1), None);
        set_video_rect(0, 0, 0, 10, 10);
        set_video_rect(0, 0, 0, 0, 0);
        destroy_video_child(0);
        assert_eq!(client_origin(0), None);
        assert_eq!(window_rect(0), None);
    }
}
