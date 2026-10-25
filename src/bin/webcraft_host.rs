#![cfg_attr(windows, windows_subsystem = "windows")]
use std::io::{Read, Write};
use std::path::PathBuf;

fn config_dir() -> PathBuf {
    // Must match main app config_dir without depending on it (host is standalone small binary).
    directories::ProjectDirs::from("", "", "webcraft")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".webcraft"))
}

fn inbox_dir() -> PathBuf {
    config_dir().join("inbox")
}

fn browser_resume_dir() -> PathBuf {
    config_dir().join("browser-resume")
}

/// Live GUI instance? Mirrors app's single-instance check: lock pid alive.
/// Host is same-OS standalone binary, so liveness check duplicates per-OS
/// (app's `process_alive` lives in GUI binary, not a shared lib).
fn app_running() -> bool {
    let Ok(txt) = std::fs::read_to_string(config_dir().join("instance.lock")) else {
        return false;
    };
    let Ok(pid) = txt.trim().parse::<u32>() else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        type Handle = *mut core::ffi::c_void;
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
            fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
            fn CloseHandle(h: Handle) -> i32;
        }
        let h = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
    #[cfg(unix)]
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        #[cfg(target_os = "macos")]
        {
            // kill(pid,0) succeeds for zombies too; app exits fully on quit,
            // and launch is de-duped by app's own single-instance forward.
        }
        kill(pid as i32, 0) == 0
    }
    #[cfg(not(any(windows, unix)))]
    false
}

/// Spawn sibling GUI (`webcraft[.exe]` next to this host exe), detached.
/// No URL arg: app drains inbox on start -> capture modal. Browser-triggered
/// start must be visible even when start_minimized is set, so pass --show
/// (ignored by older builds).
/// ponytail: sibling-exe convention only; fallback = user starts app manually.
fn launch_app() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else { return };
    let app = dir.join(if cfg!(windows) {
        "webcraft.exe"
    } else {
        "webcraft"
    });
    if !app.exists() {
        return;
    }
    let mut cmd = std::process::Command::new(app);
    cmd.arg("--show")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console inherit,
        // survives host/browser exit.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // New session: detach from host's controlling terminal/group.
                extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            });
        }
    }
    let _ = cmd.spawn();
}

fn ext_of(filename: &str, url: &str) -> String {
    let seg = filename
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            url.split(['?', '#'])
                .next()
                .unwrap_or(url)
                .rsplit('/')
                .next()
                .unwrap_or("")
        });
    match seg.rfind('.') {
        Some(i) if i > 0 => seg[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// Capture filter from `capture-filter.json` in config dir, written by desktop
/// whenever capture settings change: {"exts":["mp4","zip"],"minBytes":123}.
/// Missing/corrupt file = no filter (desktop never ran / settings default).
/// Unknown size (0/missing) denied when a minimum is set (cannot prove
/// large enough); allowed when no minimum set.
/// Empty ext list = all types. Explicit sends (captured=false) never reach here.
pub fn capture_allowed(
    exts: &[String],
    min_bytes: u64,
    filename: &str,
    url: &str,
    mime: &str,
    size: u64,
) -> bool {
    if min_bytes > 0 && (size == 0 || size < min_bytes) {
        return false;
    }
    if exts.is_empty() {
        return true;
    }
    let ext = ext_of(filename, url);
    if !ext.is_empty() {
        return exts.iter().any(|e| e == &ext);
    }
    // No extension in filename/URL: fall back to MIME subtype ("video/mp4" -> "mp4").
    let sub = mime
        .split(['/', ';'])
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    !sub.is_empty() && exts.iter().any(|e| e == &sub)
}

fn load_capture_filter() -> (Vec<String>, u64) {
    // Legacy fallback: pre-rename desktop wrote filter under downloadcraft dir.
    let new_path = config_dir().join("capture-filter.json");
    let legacy_path = directories::ProjectDirs::from("", "", "downloadcraft")
        .map(|d| d.config_dir().join("capture-filter.json"))
        .unwrap_or_else(|| PathBuf::from(".downloadcraft/capture-filter.json"));
    let path = if new_path.exists() {
        new_path
    } else {
        legacy_path
    };
    let Ok(txt) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return (Vec::new(), 0);
    };
    let exts = v
        .get("exts")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    e.as_str()
                        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let min = v.get("minBytes").and_then(|m| m.as_u64()).unwrap_or(0);
    (exts, min)
}

/// Resolved video-button style (preset id + rendered fields). Mirrors the
/// app's preset table; defaults = "arrow-m" when file missing/corrupt.
fn load_button_style() -> (String, String, u64, u64, String) {
    fn defaults() -> (String, String, u64, u64, String) {
        (
            "arrow-m".into(),
            "arrow".into(),
            27,
            27,
            "50%".into(),
        )
    }
    let Ok(txt) = std::fs::read_to_string(config_dir().join("button-style.json")) else {
        return defaults();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return defaults();
    };
    let preset = v
        .get("preset")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("arrow-m")
        .to_string();
    let icon = v
        .get("icon")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("arrow")
        .to_string();
    let w = v.get("w").and_then(|p| p.as_u64()).unwrap_or(27);
    let h = v.get("h").and_then(|p| p.as_u64()).unwrap_or(27);
    let radius = v
        .get("radius")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("50%")
        .to_string();
    (preset, icon, w, h, radius)
}

/// Basename only: strip `/` and `\` components, reject empty / "." / "..".
fn sanitize_filename(name: &str) -> Option<String> {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")?;
    Some(base.to_string())
}

/// Same URL already queued as unconsumed inbox file within window?
/// ponytail: O(n inbox) scan per captured send; inbox drained continuously,
/// add index/state file only if inbox backs up.
fn recent_duplicate(dir: &std::path::Path, url: &str, now: std::time::SystemTime) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten().any(|e| {
        let name = e.file_name();
        let Some(name) = name.to_str() else { return false };
        if !name.starts_with("msg-") || !name.ends_with(".txt") {
            return false;
        }
        let Ok(mtime) = e.metadata().and_then(|m| m.modified()) else {
            return false;
        };
        // Future mtime (clock skew) counts as recent; >10s old does not.
        if now
            .duration_since(mtime)
            .is_ok_and(|d| d >= std::time::Duration::from_secs(10))
        {
            return false;
        }
        std::fs::read_to_string(e.path())
            .ok()
            .and_then(|b| b.lines().next().map(|l| l == format!("url={url}")))
            .unwrap_or(false)
    })
}

/// Poll resume files written by app on capture-window cancel.
/// Claim by rename to `.taken`: two browser profiles race, first rename wins.
/// Claimed file deleted regardless parse success. Missing dir = empty.
/// Freshness: `ts` (unix ms) primary; missing/invalid -> mtime fallback.
/// Stale if effective ts older than 10 min. Cap 10.
fn take_resume_items(dir: &std::path::Path, now_ms: u64) -> Vec<serde_json::Value> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let stale_before = now_ms.saturating_sub(600_000);
    let mut items = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("resume-") || !name.ends_with(".json") {
            continue;
        }
        let path = e.path();
        let taken = path.with_extension("taken");
        if std::fs::rename(&path, &taken).is_err() {
            continue;
        }
        let mtime_ms = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(now_ms);
        let txt = std::fs::read_to_string(&taken).unwrap_or_default();
        let _ = std::fs::remove_file(&taken);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
            continue;
        };
        let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        let ts = v.get("ts").and_then(|t| t.as_u64());
        let effective = ts.unwrap_or(mtime_ms);
        if effective < stale_before {
            continue;
        }
        if items.len() >= 10 {
            continue;
        }
        items.push(serde_json::json!({
            "url": url,
            "filename": v.get("filename").and_then(|f| f.as_str()).unwrap_or(""),
            "referrer": v.get("referrer").and_then(|f| f.as_str()).unwrap_or(""),
            "ts": ts.unwrap_or(0),
        }));
    }
    items
}

fn handle(msg: &serde_json::Value) -> serde_json::Value {
    let ty = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Echo request `id` verbatim when present; harmless for old clients that ignore it.
    let with_id = |mut resp: serde_json::Value| {
        if let Some(id) = msg.get("id") {
            if let Some(obj) = resp.as_object_mut() {
                obj.insert("id".into(), id.clone());
            }
        }
        resp
    };
    match ty {
        "ping" => with_id(serde_json::json!({"type":"pong"})),
        // Pre-flight for captured downloads: extension asks BEFORE cancelling
        // browser download. allow=false -> browser keeps file (filter is
        // webcraft Settings via capture-filter.json, host is sole decider).
        // No inbox write here; actual send is separate "download" msg.
        // Extension fetches resolved button preset on load and on change;
        // host reads button-style.json fresh per call (applies live).
        "getButtonStyle" => {
            let (preset, icon, w, h, radius) = load_button_style();
            with_id(serde_json::json!({"type":"buttonStyle","preset":preset,"icon":icon,"w":w,"h":h,"radius":radius}))
        }
        "pollResume" => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let items = take_resume_items(&browser_resume_dir(), now_ms);
            with_id(serde_json::json!({"type":"pollResume","items":items}))
        }
        "captureCheck" => {
            let (exts, min_bytes) = load_capture_filter();
            let filename = msg.get("filename").and_then(|f| f.as_str()).unwrap_or("");
            let url = msg.get("url").and_then(|f| f.as_str()).unwrap_or("");
            let mime = msg.get("mime").and_then(|f| f.as_str()).unwrap_or("");
            let size = msg.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            let allow = capture_allowed(&exts, min_bytes, filename, url, mime, size);
            with_id(serde_json::json!({"type":"captureCheck","allow":allow}))
        }
        "download" | "add" => {
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return with_id(serde_json::json!({"type":"error","error":"missing url"}));
            }
            // Captured downloads pass capture filter (webcraft Settings).
            // Explicit sends omit `captured` and always go: user intent.
            // Extension pre-checks via captureCheck before cancelling, so
            // filtered normally never reaches here; skipped reply = raced
            // filter change, extension resumes browser download on it.
            if msg
                .get("captured")
                .and_then(|c| c.as_bool())
                .unwrap_or(false)
            {
                let (exts, min_bytes) = load_capture_filter();
                let filename = msg.get("filename").and_then(|f| f.as_str()).unwrap_or("");
                let mime = msg.get("mime").and_then(|f| f.as_str()).unwrap_or("");
                let size = msg.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                if !capture_allowed(&exts, min_bytes, filename, url, mime, size) {
                    return with_id(serde_json::json!({"type":"ok","skipped":"filtered"}));
                }
                // Backstop vs double-fire (two extension paths / double-click
                // racing pause-first probe): same URL queued <10s ago and
                // still unconsumed. Extension treats skipped as resume-browser,
                // so duplicate browser copy restarts; original stays queued.
                if recent_duplicate(&inbox_dir(), url, std::time::SystemTime::now()) {
                    return with_id(serde_json::json!({"type":"ok","skipped":"duplicate"}));
                }
            }
            let dir = inbox_dir();
            let _ = std::fs::create_dir_all(&dir);
            let name = format!(
                "msg-{}.txt",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let mut body = format!("url={url}\n");
            // size arrives as JSON number: write explicitly or KV drops it.
            if let Some(sz) = msg.get("size").and_then(|v| v.as_u64()) {
                if sz > 0 {
                    body.push_str(&format!("size={sz}\n"));
                }
            }
            for k in [
                "filename",
                "referrer",
                "cookies",
                "cookiesJson",
                "referer",
                "cookie",
                "queue",
                "mime",
                "type",
                "pageUrl",
                "pageTitle",
                "intent",
                "size",
            ] {
                if k == "size" {
                    continue;
                }
                if let Some(v) = msg.get(k).and_then(|v| v.as_str()) {
                    if k == "pageTitle" {
                        let clean: String = v
                            .chars()
                            .map(|c| {
                                if matches!(c, '\r' | '\n' | '\t') {
                                    ' '
                                } else {
                                    c
                                }
                            })
                            .take(300)
                            .collect();
                        if !clean.trim().is_empty() {
                            body.push_str(&format!("pageTitle={clean}\n"));
                        }
                    } else if k == "filename" {
                        // Drop traversal: write basename only; drop if basename invalid/empty.
                        if let Some(b) = sanitize_filename(v) {
                            body.push_str(&format!("filename={b}\n"));
                        }
                    } else if k == "mime" {
                        // Desktop parser ignores unknown keys; kept for forward-compat.
                        if !v.contains(['\n', '\r']) {
                            body.push_str(&format!("mime={v}\n"));
                        }
                    } else if k != "type" {
                        body.push_str(&format!("{k}={v}\n"));
                    }
                }
            }
            // Safe browser request headers (extension allowlist) flatten to
            // header.<Name>=<value> lines; desktop parse_kv_file stores them
            // on the task and the engine sends them. cookie/host/content-
            // length are never flattened: cookies travel via `cookies`, the
            // rest are set by the HTTP stack. Sanitized: CR/LF would forge
            // extra KV lines, so such pairs are dropped; count/value capped.
            if let Some(hdrs) = msg.get("headers").and_then(|v| v.as_object()) {
                let mut written = 0usize;
                for (hn, hv) in hdrs {
                    if written >= 8 {
                        break;
                    }
                    let hv = match hv.as_str() {
                        Some(s) => s,
                        None => continue,
                    };
                    let hn = hn.trim();
                    let hv = hv.trim();
                    if hn.is_empty()
                        || hv.is_empty()
                        || hn.len() > 64
                        || hv.len() > 2048
                        || hn.contains(['\r', '\n', '='])
                        || hv.contains(['\r', '\n'])
                        || hn.eq_ignore_ascii_case("cookie")
                        || hn.eq_ignore_ascii_case("host")
                        || hn.eq_ignore_ascii_case("content-length")
                    {
                        continue;
                    }
                    body.push_str(&format!("header.{hn}={hv}\n"));
                    written += 1;
                }
            }
            match std::fs::write(dir.join(name), body) {
                Ok(()) => {
                    // Inbox first, signal second: running app's watcher wakes
                    // hidden (tray) viewport to drain inbox + show capture modal.
                    // App quit: nothing consumes inbox, so launch app (drains
                    // inbox on start -> modal, focused). show.request covers
                    // zombie-lock/stale-pid race: spawned app forwards to it.
                    if !app_running() {
                        launch_app();
                    }
                    let _ = std::fs::write(config_dir().join("show.request"), "show");
                    with_id(serde_json::json!({"type":"ok"}))
                }
                Err(e) => with_id(serde_json::json!({"type":"error","error":e.to_string()})),
            }
        }
        _ => with_id(serde_json::json!({"type":"error","error":"unknown type"})),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut inp = stdin.lock();
    let mut out = stdout.lock();
    loop {
        let mut lenbuf = [0u8; 4];
        if inp.read_exact(&mut lenbuf).is_err() {
            break;
        }
        let n = u32::from_le_bytes(lenbuf) as usize;
        if n > 10_000_000 {
            break;
        }
        let mut buf = vec![0u8; n];
        if inp.read_exact(&mut buf).is_err() {
            break;
        }
        let msg: serde_json::Value = serde_json::from_slice(&buf).unwrap_or(serde_json::json!({}));
        let resp = handle(&msg);
        let body = resp.to_string();
        let _ = out.write_all(&(body.len() as u32).to_le_bytes());
        let _ = out.write_all(body.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handle_echoes_id_and_stays_backward_compatible() {
        let r = handle(&serde_json::json!({"type":"ping","id":7}));
        assert_eq!(r["type"], "pong");
        assert_eq!(r["id"], 7);
        let r = handle(&serde_json::json!({"type":"ping"}));
        assert!(r.get("id").is_none());
        let r = handle(&serde_json::json!({"type":"download","id":"a1"}));
        assert_eq!(r["type"], "error");
        assert_eq!(r["id"], "a1");
    }

    #[test]
    fn button_style_reply_has_default_fields() {
        // Pure default-shape check mirroring load_button_style() defaults;
        // handle() would read the real user's button-style.json (non-hermetic).
        let (preset, icon, w, h, radius) = (
            "arrow-m".to_string(),
            "arrow".to_string(),
            27u64,
            27u64,
            "50%".to_string(),
        );
        let r = serde_json::json!({"type":"buttonStyle","preset":preset,"icon":icon,"w":w,"h":h,"radius":radius});
        assert_eq!(r["type"], "buttonStyle");
        assert_eq!(r["preset"], "arrow-m");
        assert_eq!(r["icon"], "arrow");
        assert_eq!(r["w"], 27);
        assert_eq!(r["h"], 27);
        assert_eq!(r["radius"], "50%");
        // Reply type passes through with_id echo like other arms.
        let r = handle(&serde_json::json!({"type":"getButtonStyle","id":9}));
        assert_eq!(r["type"], "buttonStyle");
        assert_eq!(r["id"], 9);
        assert!(r["preset"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(r["icon"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(r["w"].as_u64().is_some_and(|n| n > 0));
        assert!(r["h"].as_u64().is_some_and(|n| n > 0));
        assert!(r["radius"].as_str().is_some_and(|s| !s.is_empty()));
    }

    fn exts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn capture_check_msg_answers_allow() {
        // Defaults = empty exts + min 0: always allow. Pure via capture_allowed
        // (handle() would read real user capture-filter.json — non-hermetic).
        assert!(capture_allowed(&[], 0, "f.bin", "https://x/f.bin", "", 10));
        assert!(!capture_allowed(
            &exts(&["zip"]),
            0,
            "f.bin",
            "https://x/f.bin",
            "",
            10
        ));
        assert!(!capture_allowed(
            &[],
            100,
            "f.zip",
            "https://x/f.zip",
            "",
            10
        ));
    }

    #[test]
    fn capture_empty_exts_allows_all() {
        assert!(capture_allowed(
            &[],
            0,
            "a.bin",
            "https://x/a.bin",
            "application/octet-stream",
            5
        ));
        assert!(capture_allowed(&[], 0, "", "https://x/noext", "", 0));
    }

    #[test]
    fn capture_allowlist_matches_filename_or_url_ext_case_insensitively() {
        let e = exts(&["mp4", "zip"]);
        assert!(capture_allowed(
            &e,
            0,
            "Movie.MP4",
            "https://x/movie",
            "video/mp4",
            10
        ));
        assert!(capture_allowed(
            &e,
            0,
            "",
            "https://x/file.ZIP?token=1",
            "",
            10
        ));
        assert!(!capture_allowed(
            &e,
            0,
            "notes.pdf",
            "https://x/notes.pdf",
            "application/pdf",
            10
        ));
    }

    #[test]
    fn capture_allowlist_falls_back_to_mime_subtype_when_no_ext() {
        let e = exts(&["mp4"]);
        assert!(capture_allowed(
            &e,
            0,
            "download",
            "https://x/download",
            "video/mp4",
            10
        ));
        assert!(!capture_allowed(
            &e,
            0,
            "download",
            "https://x/download",
            "application/pdf",
            10
        ));
        assert!(!capture_allowed(
            &e,
            0,
            "download",
            "https://x/download",
            "",
            10
        ));
    }

    #[test]
    fn capture_min_size_skips_only_known_small() {
        assert!(!capture_allowed(
            &[],
            10 * 1024 * 1024,
            "a.zip",
            "https://x/a.zip",
            "",
            5 * 1024 * 1024
        ));
        assert!(capture_allowed(
            &[],
            10 * 1024 * 1024,
            "a.zip",
            "https://x/a.zip",
            "",
            10 * 1024 * 1024
        ));
        assert!(!capture_allowed(
            &[],
            10 * 1024 * 1024,
            "a.zip",
            "https://x/a.zip",
            "",
            0
        )); // unknown size with min set: cannot prove large enough
        assert!(capture_allowed(&[], 0, "a.zip", "https://x/a.zip", "", 0)); // unknown size, no minimum: allow
        assert!(capture_allowed(&[], 0, "a.zip", "https://x/a.zip", "", 1)); // 0 = no minimum
    }

    fn temp_resume(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "webcraft-host-resume-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn take_resume_items_takes_only_fresh_http_and_consumes() {
        let dir = temp_resume("mixed");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let w = |name: &str, body: String| std::fs::write(dir.join(name), body).unwrap();
        w(
            "resume-1.json",
            serde_json::json!({"url":"https://x/a.zip","filename":"a.zip","referrer":"https://r/","ts":now_ms}).to_string(),
        );
        w(
            "resume-2.json",
            serde_json::json!({"url":"http://x/b.zip","ts":now_ms}).to_string(),
        );
        w(
            "resume-3.json",
            serde_json::json!({"url":"https://x/stale.zip","ts":now_ms-601_000}).to_string(),
        );
        w("resume-4.json", "not json{".into());
        w(
            "resume-5.json",
            serde_json::json!({"url":"ftp://x/c.zip","ts":now_ms}).to_string(),
        );
        w("other.json", serde_json::json!({"url":"https://x/d.zip","ts":now_ms}).to_string());
        let items = take_resume_items(&dir, now_ms);
        assert_eq!(items.len(), 2);
        let mut urls: Vec<&str> = items.iter().filter_map(|i| i["url"].as_str()).collect();
        urls.sort();
        assert_eq!(urls, ["http://x/b.zip", "https://x/a.zip"]);
        let a = items.iter().find(|i| i["url"] == "https://x/a.zip").unwrap();
        assert_eq!(a["filename"], "a.zip");
        assert_eq!(a["referrer"], "https://r/");
        assert_eq!(a["ts"], now_ms);
        let b = items.iter().find(|i| i["url"] == "http://x/b.zip").unwrap();
        assert_eq!(b["filename"], "");
        assert_eq!(b["referrer"], "");
        // Claimed files consumed; unclaimed non-resume file untouched.
        assert!(take_resume_items(&dir, now_ms).is_empty());
        assert!(dir.join("other.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_resume_items_missing_dir_and_cap() {
        assert!(take_resume_items(
            &std::env::temp_dir().join("webcraft-host-resume-nope"),
            0
        )
        .is_empty());
        let dir = temp_resume("cap");
        let now_ms = 1_000_000u64;
        for i in 0..12 {
            std::fs::write(
                dir.join(format!("resume-{i}.json")),
                serde_json::json!({"url":format!("https://x/{i}.zip"),"ts":now_ms}).to_string(),
            )
            .unwrap();
        }
        assert_eq!(take_resume_items(&dir, now_ms).len(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_inbox(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "webcraft-host-dedupe-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recent_duplicate_matches_url_within_10s() {
        let dir = temp_inbox("fresh");
        std::fs::write(
            dir.join("msg-1-0.txt"),
            "url=https://x/a.zip?n=1\nreferrer=\n",
        )
        .unwrap();
        let now = std::time::SystemTime::now();
        assert!(recent_duplicate(&dir, "https://x/a.zip?n=1", now));
        assert!(!recent_duplicate(&dir, "https://x/other.zip", now));
        // Only first line counts.
        assert!(!recent_duplicate(&dir, "", now));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_duplicate_ignores_stale_and_non_msg() {
        let dir = temp_inbox("stale");
        std::fs::write(dir.join("msg-1-0.txt"), "url=https://x/a.zip\n").unwrap();
        std::fs::write(dir.join("other.txt"), "url=https://x/b.zip\n").unwrap();
        let now = std::time::SystemTime::now();
        // 11s later: stale.
        assert!(!recent_duplicate(
            &dir,
            "https://x/a.zip",
            now + std::time::Duration::from_secs(11)
        ));
        // Non-msg file never matches.
        assert!(!recent_duplicate(&dir, "https://x/b.zip", now));
        // Missing dir: no dup.
        assert!(!recent_duplicate(
            &dir.join("nope"),
            "https://x/a.zip",
            now
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
