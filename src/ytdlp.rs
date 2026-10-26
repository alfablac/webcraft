//! yt-dlp sidecar: resolve/download binary, list formats, download video pages.
use crate::engine::{
    Controls, PHASE_DOWNLOADING, PHASE_EXTRACTING, PHASE_FINALIZING, PHASE_PROCESSING,
};
use crate::model::*;
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

fn bin_dir() -> PathBuf {
    crate::platform::config_dir().join("bin")
}

/// (a) sidecar next to exe, (b) config_dir/bin, (c) PATH (bare name if on PATH).
pub fn ytdlp_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let p = bin_dir().join(exe_name());
    if p.is_file() {
        return Some(p);
    }
    // PATH probe: spawn `--version` cheap check via which-style search.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub async fn ensure_ytdlp() -> Result<PathBuf, String> {
    if let Some(p) = ytdlp_path() {
        return Ok(p);
    }
    let dir = bin_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let dest = dir.join(exe_name());
    let url = if cfg!(windows) {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
    } else {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp"
    };
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("yt-dlp download HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() < 1_000_000 {
        return Err(format!("yt-dlp download too small ({} bytes)", bytes.len()));
    }
    let part = dir.join(format!("{}.part", exe_name()));
    tokio::fs::write(&part, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).await;
    }
    tokio::fs::rename(&part, &dest)
        .await
        .map_err(|e| e.to_string())?;
    Ok(dest)
}

// ponytail: -U is upstream-supported self-updater; do not reimplement release asset swap.
pub fn ytdlp_update(path: &std::path::Path) -> Result<String, String> {
    let out = run_timeout(path, &["-U".to_string()], 120)?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(tail(&s))
    } else {
        Err(tail(&s))
    }
}

fn tail(s: &str) -> String {
    let t: Vec<&str> = s.lines().collect();
    t[t.len().saturating_sub(5)..].join("\n")
}

fn run_timeout(
    path: &std::path::Path,
    args: &[String],
    secs: u64,
) -> Result<std::process::Output, String> {
    let mut cmd = Command::new(path);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::platform::no_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // Drain pipes on threads: -J dumps exceed 64KB pipe buffer; undrained
    // child blocks on write and never exits (wait-then-read deadlocks).
    fn drain<R: std::io::Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = r.read_to_end(&mut v);
            v
        })
    }
    let out_th = child.stdout.take().map(drain);
    let err_th = child.stderr.take().map(drain);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let stdout = out_th.and_then(|h| h.join().ok()).unwrap_or_default();
                let stderr = err_th.and_then(|h| h.join().ok()).unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None if start.elapsed().as_secs() >= secs => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("yt-dlp timed out after {secs}s"));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct YtFormat {
    pub format_id: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default)]
    pub vcodec: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub tbr: Option<f64>,
    #[serde(default)]
    pub abr: Option<f64>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub format_note: Option<String>,
    #[serde(default)]
    pub filesize: Option<u64>,
    #[serde(default)]
    pub filesize_approx: Option<u64>,
}

impl YtFormat {
    pub fn size(&self) -> Option<u64> {
        self.filesize.or(self.filesize_approx)
    }
    pub fn has_video(&self) -> bool {
        // vcodec null = progressive format with unknown codec (X http-*): treat as video.
        match self.vcodec.as_deref() {
            None => true,
            Some(c) => c != "none",
        }
    }
    pub fn has_audio(&self) -> bool {
        self.acodec.as_deref().is_some_and(|c| c != "none")
    }
    pub fn is_audio_only(&self) -> bool {
        !self.has_video() && self.has_audio()
    }
    pub fn is_video_only(&self) -> bool {
        self.has_video() && !self.has_audio()
    }
    /// Picker label: "137 · 1080p 30fps · mp4 avc1 · ~812 MB".
    pub fn label(&self) -> String {
        let mut p: Vec<String> = vec![self.format_id.clone()];
        if let Some(h) = self.height {
            p.push(format!("{h}p"));
        } else if let Some(w) = self.width {
            p.push(format!("{w}w"));
        }
        if let Some(f) = self.fps {
            if f > 0.0 {
                p.push(format!("{}fps", f.round() as u32));
            }
        }
        if !self.ext.is_empty() {
            p.push(self.ext.clone());
        }
        let codec = if self.is_audio_only() {
            self.acodec.as_deref()
        } else {
            self.vcodec.as_deref()
        };
        if let Some(c) = codec {
            if c != "none" {
                p.push(c.split('.').next().unwrap_or(c).to_string());
            }
        }
        if let Some(l) = &self.language {
            p.push(format!("[{l}]"));
        }
        if let Some(s) = self.size() {
            p.push(format!("~{:.1} MB", s as f64 / 1_048_576.0));
        } else if let Some(t) = self.tbr {
            if t > 0.0 {
                p.push(format!("{:.0} kbps", t));
            }
        }
        if let Some(n) = &self.format_note {
            if !n.is_empty() {
                p.push(n.clone());
            }
        }
        p.join(" · ")
    }
}

/// Best: max height video-only wins over lower muxed; else tallest muxed.
/// ponytail: kept for label/tests only; download selectors use yt-dlp
/// sort order (`bv*+ba/b` + `-S`), not this pin.
pub fn pick_best(formats: &[YtFormat]) -> Option<YtFormat> {
    let real: Vec<&YtFormat> = formats.iter().filter(|f| f.has_video()).collect();
    real.iter()
        .max_by_key(|f| f.height.unwrap_or(0))
        .map(|f| (*f).clone())
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CookieEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    path: String,
    #[serde(default, rename = "expirationDate")]
    expiration_date: Option<f64>,
    #[serde(default)]
    secure: bool,
    #[serde(default, rename = "httpOnly")]
    http_only: bool,
    #[serde(default, rename = "hostOnly")]
    host_only: bool,
}

fn clean_field(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect()
}

/// Write task cookies_json as Netscape cookie jar for yt-dlp --cookies.
/// String Cookie header cannot carry httpOnly/domain/expiry; jar keeps them.
/// None when missing/empty/invalid (caller falls back to Cookie header).
/// RAII cookie jar: file removed on drop (all callers' child processes have
/// finished by then). Derefs to Path for --cookies args.
pub struct CookieJar(PathBuf);
impl std::ops::Deref for CookieJar {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl AsRef<std::path::Path> for CookieJar {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for CookieJar {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Sweep stale jars (killed-process leftovers) older than 24h. Cheap, errors ignored.
fn sweep_cookie_jars(dir: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let Some(cutoff) =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(24 * 3600))
    else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("cookies-")
            && name.ends_with(".txt")
            && e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < cutoff)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub fn write_cookie_jar(task: &DownloadTask) -> Option<CookieJar> {
    if task.cookies_json.trim().is_empty() {
        return None;
    }
    let entries: Vec<CookieEntry> = serde_json::from_str(&task.cookies_json).ok()?;
    if entries.is_empty() {
        return None;
    }
    let dir = crate::platform::config_dir().join("cookies");
    std::fs::create_dir_all(&dir).ok()?;
    sweep_cookie_jars(&dir);
    let path = dir.join(format!(
        "cookies-{}-{}.txt",
        std::process::id(),
        crate::model::new_id()
    ));
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    let mut written = 0usize;
    for c in &entries {
        let mut domain = clean_field(&c.domain);
        if domain.is_empty() {
            continue;
        }
        if !c.host_only && !domain.starts_with('.') {
            domain = format!(".{domain}");
        }
        let path_s = {
            let p = clean_field(&c.path);
            if p.is_empty() {
                "/".to_string()
            } else {
                p
            }
        };
        let expires = c
            .expiration_date
            .map(|e| if e > 0.0 { e as u64 } else { 0 })
            .unwrap_or(0);
        out.push_str(&format!(
            "{}{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            if c.http_only { "#HttpOnly_" } else { "" },
            domain,
            if c.host_only { "FALSE" } else { "TRUE" },
            path_s,
            if c.secure { "TRUE" } else { "FALSE" },
            expires,
            clean_field(&c.name),
            clean_field(&c.value),
        ));
        written += 1;
    }
    // Zero usable lines: jar would be header-only, yt-dlp --cookies suppresses
    // legacy Cookie-header fallback (base_args drops it when jar present).
    if written == 0 {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    std::fs::write(&path, out).ok()?;
    Some(CookieJar(path))
}

/// Direct manifest URL (.m3u8/.mpd path, query/fragment stripped).
/// Site extractors claim some manifest hosts (GloboArticle on *.globo.com)
/// and crash; generic extractor parses manifest itself.
pub fn is_manifest_url(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

pub fn is_youtube_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let h = host.strip_prefix("www.").unwrap_or(&host);
    let h = h.strip_prefix("m.").unwrap_or(h);
    matches!(h, "youtube.com" | "music.youtube.com" | "youtu.be")
}

fn has_node() -> bool {
    static HAS_NODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS_NODE.get_or_init(|| {
        let mut cmd = Command::new("node");
        cmd.arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::platform::no_window(&mut cmd);
        cmd.output().map(|o| o.status.success()).unwrap_or(false)
    })
}

fn anon_task(task: &DownloadTask) -> DownloadTask {
    let mut t = task.clone();
    t.cookies.clear();
    t.cookies_json.clear();
    t.headers.retain(|k, _| !k.eq_ignore_ascii_case("cookie"));
    t
}

fn task_has_cookies(task: &DownloadTask) -> bool {
    !task.cookies.trim().is_empty()
        || !task.cookies_json.trim().is_empty()
        || task
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("cookie") && !v.trim().is_empty())
}

/// (max video height, video-format count): anon-adoption comparison key.
fn dump_video_quality(dump: &YtDump) -> (u32, usize) {
    let v = dump.video_formats();
    (
        v.iter().map(|f| f.height.unwrap_or(0)).max().unwrap_or(0),
        v.len(),
    )
}

/// Degraded success: cookied YouTube probe returns only itag-18 360p +
/// storyboards (storyboards are not video-bearing), vs anon full ladder.
/// Genuine 360p-only videos also match; harmless, anon adopted only if better.
fn dump_is_degraded(dump: &YtDump) -> bool {
    let (max_h, n) = dump_video_quality(dump);
    (1..=2).contains(&n) && max_h <= 360
}

fn should_reprobe_anon_degraded(url: &str, task: &DownloadTask, dump: &YtDump) -> bool {
    is_youtube_url(url) && task_has_cookies(task) && dump_is_degraded(dump)
}

fn should_retry_anon(url: &str, task: &DownloadTask, err: &str) -> bool {
    if !is_youtube_url(url) {
        return false;
    }
    if !task_has_cookies(task) {
        return false;
    }
    let e = err.to_ascii_lowercase();
    [
        "page needs to be reloaded",
        "page needs be reloaded",
        "sign in to confirm",
        "sign in confirm",
        // Exact-id in cookied client pool missing (pool often itag 18 only):
        // selector falls through silently, but bare exact ids hard-error here.
        "requested format not available",
        "requested format is not available",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// Attempt order as `is_anon` flags. Adopted anon dump means download must
/// run in same (anon) session that listed those formats: cookied pool may
/// lack picked id and selector silently degrades to muxed 360p. Cookied is
/// fallback on any anon failure. Not adopted: cookied first (preserves
/// age-gated/private access), anon only on `should_retry_anon` error.
fn download_attempt_order(used_anon: bool) -> [bool; 2] {
    if used_anon {
        [true, false]
    } else {
        [false, true]
    }
}

fn base_args(url: &str, task: &DownloadTask, cookie_jar: Option<&std::path::Path>) -> Vec<String> {
    let mut a: Vec<String> = vec!["--no-playlist".into(), "--no-warnings".into()];
    if has_node() {
        a.push("--js-runtimes".into());
        a.push("node".into());
    }
    if is_manifest_url(url) {
        a.push("--force-generic-extractor".into());
    }
    if !task.referrer.is_empty() {
        a.push("--referer".into());
        a.push(task.referrer.clone());
    }
    // Effective proxy resolved into task.proxy_url by caller (start_task /
    // probe task); bare yt-dlp subprocess honors no global setting itself.
    if !task.proxy_url.trim().is_empty() {
        a.push("--proxy".into());
        a.push(task.proxy_url.trim().to_string());
    }
    if let Some(jar) = cookie_jar {
        a.push("--cookies".into());
        a.push(jar.to_string_lossy().into_owned());
    } else if !task.cookies.is_empty() {
        a.push("--add-header".into());
        a.push(format!("Cookie:{}", task.cookies));
    }
    for (k, v) in &task.headers {
        // Pseudo-headers are task config, never HTTP headers.
        // With a jar, Cookie comes from file; user Cookie header would duplicate/override.
        // Denied wire headers (Range, accept-encoding, ...) are stack-owned
        // or response/hop-by-hop names: yt-dlp must not replay them either.
        if YT_HEADER_KEYS.iter().any(|x| k.eq_ignore_ascii_case(x))
            || k.eq_ignore_ascii_case("x-page-url")
            || k.eq_ignore_ascii_case("x-page-title")
            || k.eq_ignore_ascii_case("x-page-stream")
            || k.eq_ignore_ascii_case("x-page-intent")
            || k.eq_ignore_ascii_case("x-rep-id")
            || k.eq_ignore_ascii_case("x-variant-index")
            || k.eq_ignore_ascii_case("x-nzb")
            || (cookie_jar.is_some() && k.eq_ignore_ascii_case("cookie"))
            || crate::engine::is_denied_wire_header(k)
        {
            continue;
        }
        a.push("--add-header".into());
        a.push(format!("{k}:{v}"));
    }
    a.push(url.to_string());
    a
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct YtSubTrack {
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct YtDump {
    #[serde(default)]
    pub formats: Vec<YtFormat>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub subtitles: std::collections::BTreeMap<String, Vec<YtSubTrack>>,
    #[serde(default)]
    pub automatic_captions: std::collections::BTreeMap<String, Vec<YtSubTrack>>,
    /// Playlist dumps nest video info in entries; empty for single videos.
    #[serde(default)]
    pub entries: Vec<YtDump>,
}

impl YtDump {
    /// Playlist dump with no own formats: descend into first entry carrying
    /// formats (recursively). Parent title wins when entry title empty.
    /// Entry subtitles/captions/metadata replace parent's (entry is video).
    pub fn flatten_entries(mut self) -> Self {
        if !self.formats.is_empty() || self.entries.is_empty() {
            return self;
        }
        let parent_title = std::mem::take(&mut self.title);
        for entry in std::mem::take(&mut self.entries) {
            let mut entry = entry.flatten_entries();
            if !entry.formats.is_empty() {
                if entry.title.is_empty() {
                    entry.title = parent_title;
                }
                return entry;
            }
        }
        self.title = parent_title;
        self
    }
    pub fn uploader(&self) -> &str {
        self.uploader
            .as_deref()
            .or(self.channel.as_deref())
            .unwrap_or("")
    }
    /// Video-bearing formats, tallest then highest bitrate first.
    pub fn video_formats(&self) -> Vec<&YtFormat> {
        let mut v: Vec<&YtFormat> = self.formats.iter().filter(|f| f.has_video()).collect();
        v.sort_by(|a, b| {
            (b.height.unwrap_or(0), b.tbr.unwrap_or(0.0) as u64)
                .cmp(&(a.height.unwrap_or(0), a.tbr.unwrap_or(0.0) as u64))
        });
        v
    }
    pub fn audio_formats(&self) -> Vec<&YtFormat> {
        let mut v: Vec<&YtFormat> = self.formats.iter().filter(|f| f.is_audio_only()).collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.abr.unwrap_or(0.0) as u64));
        v
    }
    pub fn sub_langs(&self) -> Vec<String> {
        self.subtitles.keys().cloned().collect()
    }
    pub fn auto_langs(&self) -> Vec<String> {
        self.automatic_captions
            .keys()
            .filter(|k| !self.subtitles.contains_key(*k))
            .cloned()
            .collect()
    }
}

/// Per-task yt-dlp choice, persisted as pseudo-headers (stripped in base_args,
/// never sent as HTTP headers): x-yt-format (format_id | audio:<id>),
/// x-yt-audio (format_id | best | none), x-yt-extract (mp3|m4a),
/// x-yt-quality (max height, e.g. 1080; absent = best),
/// x-yt-subs / x-yt-auto-subs (CSV langs), x-yt-sub-embed (1).
/// Precedence: exact `format` > quality tier > auto.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct YtChoice {
    pub format: Option<String>,
    pub audio: Option<String>,
    pub extract: Option<String>,
    pub subs: Vec<String>,
    pub auto_subs: Vec<String>,
    pub sub_embed: bool,
    /// Max video height for tiered selection; None = best.
    pub quality_height: Option<u32>,
}

pub const YT_HEADER_KEYS: [&str; 8] = [
    "x-ytdlp",
    "x-yt-format",
    "x-yt-audio",
    "x-yt-extract",
    "x-yt-subs",
    "x-yt-auto-subs",
    "x-yt-sub-embed",
    "x-yt-quality",
];

impl YtChoice {
    pub fn from_headers(h: &std::collections::BTreeMap<String, String>) -> Self {
        let get = |k: &str| {
            h.iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let csv = |k: &str| {
            get(k)
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            format: get("x-yt-format"),
            audio: get("x-yt-audio"),
            extract: get("x-yt-extract"),
            subs: csv("x-yt-subs"),
            auto_subs: csv("x-yt-auto-subs"),
            sub_embed: get("x-yt-sub-embed").is_some(),
            quality_height: get("x-yt-quality").and_then(|v| v.parse().ok()),
        }
    }
    pub fn is_audio_only(&self) -> bool {
        self.format
            .as_deref()
            .map(|f| f.starts_with("audio:"))
            .unwrap_or(false)
            || self.extract.is_some()
    }
    pub fn write_headers(&self, h: &mut std::collections::BTreeMap<String, String>) {
        for k in YT_HEADER_KEYS {
            if k != "x-ytdlp" {
                h.remove(k);
            }
        }
        let mut set = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                if !v.is_empty() {
                    h.insert(k.into(), v.clone());
                }
            }
        };
        set("x-yt-format", &self.format);
        set("x-yt-audio", &self.audio);
        set("x-yt-extract", &self.extract);
        if !self.subs.is_empty() {
            h.insert("x-yt-subs".into(), self.subs.join(","));
        }
        if !self.auto_subs.is_empty() {
            h.insert("x-yt-auto-subs".into(), self.auto_subs.join(","));
        }
        if self.sub_embed {
            h.insert("x-yt-sub-embed".into(), "1".into());
        }
        if let Some(q) = self.quality_height {
            if q > 0 {
                h.insert("x-yt-quality".into(), q.to_string());
            }
        }
        if self.format.is_some()
            || self.quality_height.map(|q| q > 0).unwrap_or(false)
            || self.extract.is_some()
            || !self.subs.is_empty()
            || !self.auto_subs.is_empty()
        {
            h.insert("x-ytdlp".into(), "1".into());
        }
    }
}

/// Build (-f selector, extra args, audio_mode). Every explicit selector keeps
/// /bv*+ba/b tail: chosen ids can vanish between probe and download.
pub fn build_selector(choice: &YtChoice, dump: Option<&YtDump>) -> (String, Vec<String>, bool) {
    let mut extra: Vec<String> = Vec::new();
    let audio_mode = choice.is_audio_only();
    let exact = choice.format.is_some();
    // Format sort for auto/tier picks only; explicit ids already pin order.
    if !exact && !audio_mode {
        extra.push("-S".into());
        extra.push("res,fps,vcodec:avc,acodec:aac".into());
    }
    // Plain format_id only valid for muxed formats. Video-only / unknown ids
    // need +bestaudio, else yt-dlp silently downloads video without sound.
    // Muxed-ness known only with dump; without dump fall back via / chain.
    let known = |id: &str| dump.and_then(|d| d.formats.iter().find(|f| f.format_id == id));
    let sel = match choice.format.as_deref() {
        Some("audio:ba") => "ba/b".to_string(),
        Some(f) if f.starts_with("audio:") => format!("{}/ba/b", &f[6..]),
        // Muxed known format: audio choice meaningless, ignore it.
        Some(f) if known(f).map(|x| x.has_audio() && x.has_video()) == Some(true) => {
            format!("{f}/bv*+ba/b")
        }
        Some(f) if choice.audio.is_none() => {
            format!("{f}+bestaudio/{f}/bv*+ba/b")
        }
        Some(f) => match choice.audio.as_deref() {
            // Explicit silent: no audio-bearing fallback.
            Some("none") => format!("{f}/bv*[acodec=none]/b[acodec=none]"),
            Some("best") => format!("{f}+bestaudio/bv*+ba/b"),
            Some(a) => format!("{f}+{a}/{f}+bestaudio/bv*+ba/b"),
            None => unreachable!("audio None handled above"),
        },
        None if audio_mode => "ba/b".to_string(),
        // Quality tier: cap height, fall back to uncapped then muxed.
        // [height>0] excludes height-less formats (yt-dlp drops None
        // heights from <= anyway; guard makes tier intent explicit).
        // No exact format pinned; -S sort picks best within tier.
        // Specific audio id replaces ba in first arm only.
        None if choice.quality_height.map(|q| q > 0).unwrap_or(false) => {
            let h = choice.quality_height.unwrap_or(0);
            match choice.audio.as_deref() {
                Some(a) if a != "best" && a != "none" => {
                    format!("bv*[height<={h}][height>0]+{a}/b[height<={h}][height>0]/bv*+ba/b")
                }
                _ => format!("bv*[height<={h}][height>0]+ba/b[height<={h}][height>0]/bv*+ba/b"),
            }
        }
        // Auto video + specific audio track picked.
        None if choice
            .audio
            .as_deref()
            .map(|a| a != "best" && a != "none")
            .unwrap_or(false) =>
        {
            let a = choice.audio.as_deref().unwrap_or("");
            format!("bv*+{a}/bv*+ba/b")
        }
        None if choice.audio.as_deref() == Some("none") => "bv*[acodec=none]/b".to_string(),
        // Auto: yt-dlp picks tallest video + best audio (muxed only if no
        // separate streams); no probe-based pinning.
        None => "bv*+ba/b".to_string(),
    };
    if let Some(ex) = &choice.extract {
        extra.push("-x".into());
        extra.push("--audio-format".into());
        extra.push(ex.clone());
        extra.push("--audio-quality".into());
        extra.push("0".into());
    }
    let mut langs: Vec<String> = choice.subs.clone();
    langs.extend(choice.auto_subs.iter().cloned());
    if !langs.is_empty() {
        if !choice.subs.is_empty() {
            extra.push("--write-subs".into());
        }
        if !choice.auto_subs.is_empty() {
            extra.push("--write-auto-subs".into());
        }
        extra.push("--sub-langs".into());
        extra.push(langs.join(","));
        extra.push("--sub-format".into());
        extra.push("srt/best".into());
        extra.push("--convert-subs".into());
        extra.push("srt".into());
        if choice.sub_embed && !audio_mode {
            extra.push("--embed-subs".into());
        }
    }
    (sel, extra, audio_mode)
}

pub async fn fetch_formats(url: &str, task: &DownloadTask) -> Result<YtDump, String> {
    let jar = write_cookie_jar(task);
    fetch_formats_jar(url, task, jar.as_deref())
        .await
        .map(|(d, _)| d)
}

/// Returns `(dump, used_anon)`: `used_anon` true when cookied dump missing
/// or degraded and anonymous dump adopted. Downloaders using dump must try
/// anon session first (see `download_attempt_order`).
async fn fetch_formats_jar(
    url: &str,
    task: &DownloadTask,
    jar: Option<&std::path::Path>,
) -> Result<(YtDump, bool), String> {
    match fetch_formats_jar_once(url, task, jar).await {
        Ok(dump) if should_reprobe_anon_degraded(url, task, &dump) => {
            let t = anon_task(task);
            let jar2 = write_cookie_jar(&t);
            match fetch_formats_jar_once(url, &t, jar2.as_deref()).await {
                Ok(anon_dump) if dump_video_quality(&anon_dump) > dump_video_quality(&dump) => {
                    Ok((anon_dump, true))
                }
                _ => Ok((dump, false)),
            }
        }
        Err(e) if should_retry_anon(url, task, &e) => {
            let t = anon_task(task);
            let jar2 = write_cookie_jar(&t);
            fetch_formats_jar_once(url, &t, jar2.as_deref())
                .await
                .map(|d| (d, true))
        }
        other => other.map(|d| (d, false)),
    }
}

async fn fetch_formats_jar_once(
    url: &str,
    task: &DownloadTask,
    jar: Option<&std::path::Path>,
) -> Result<YtDump, String> {
    let path = ytdlp_path().ok_or("yt-dlp not found")?;
    let mut args = vec!["-J".to_string()];
    args.extend(base_args(url, task, jar));
    let out = tokio::task::spawn_blocking(move || {
        // Bounded probe: hung extractor must not orphan forever (120s cap).
        let o = run_timeout(&path, &args, 120)?;
        if !o.status.success() {
            return Err(tail(&String::from_utf8_lossy(&o.stderr)));
        }
        Ok(o.stdout)
    })
    .await
    .map_err(|e| e.to_string())??;
    let dump: YtDump = serde_json::from_slice(&out).map_err(|e| e.to_string())?;
    Ok(dump.flatten_entries())
}

fn parse_progress(
    line: &str,
    ctrl: &Controls,
    video_done_base: &Arc<std::sync::atomic::AtomicU64>,
) {
    let rest = line.strip_prefix("download:").unwrap_or(line);
    // Template: download:<downloaded>|<total>|<estimate>|<speed>; total/estimate may be "NA".
    // ponytail: 4th field (speed) kept in template for slot stability, intentionally unparsed.
    let mut it = rest.split('|');
    let dl: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let tot: Option<u64> = it.next().and_then(|s| s.trim().parse().ok());
    let est: Option<u64> = it.next().and_then(|s| s.trim().parse().ok());
    // Estimate fills total only when real total NA/0; real total wins, never shrink.
    let effective = tot.filter(|t| *t > 0).or(est.filter(|e| *e > 0));
    // yt-dlp restarts counter per file (video then audio); keep monotonic total progress.
    let base = video_done_base.load(Ordering::Relaxed);
    ctrl.downloaded
        .store(base.saturating_add(dl), Ordering::Relaxed);
    if let Some(t) = effective {
        let cur = ctrl.total.load(Ordering::Relaxed);
        if base + t > cur {
            ctrl.total.store(base + t, Ordering::Relaxed);
        }
    }
}

/// Handle one yt-dlp stdout line: progress, postprocess, bracket phases.
/// `base` = per-file downloaded base (video_done_base shared with parse_progress).
pub fn handle_stdout_line(line: &str, ctrl: &Controls, base: &Arc<std::sync::atomic::AtomicU64>) {
    if line.starts_with("download:") || looks_like_bare_progress(line) {
        ctrl.phase.store(PHASE_DOWNLOADING, Ordering::Relaxed);
        parse_progress(line, ctrl, base);
    } else if let Some(status) = line.strip_prefix("postprocess:") {
        match status.trim() {
            "started" => ctrl.phase.store(PHASE_PROCESSING, Ordering::Relaxed),
            "finished" => ctrl.phase.store(PHASE_FINALIZING, Ordering::Relaxed),
            _ => {}
        }
    } else if line.starts_with("[Merger]")
        || line.starts_with("[VideoRemuxer]")
        || line.starts_with("[ExtractAudio]")
        || line.starts_with("[VideoConvertor]")
        || line.starts_with("[EmbedSubtitle]")
    {
        ctrl.phase.store(PHASE_PROCESSING, Ordering::Relaxed);
    }
}

/// Bare yt-dlp progress: `1024|11295255|NA|510940.67` (>=4 `|` fields,
/// first char ASCII digit). Guards template-prefix surprises.
fn looks_like_bare_progress(line: &str) -> bool {
    line.chars().next().is_some_and(|c| c.is_ascii_digit()) && line.split('|').count() >= 4
}

/// Reader filter: which stdout lines reach handle_stdout_line. Kept pure
/// so forwarding set testable without spawning yt-dlp.
fn stdout_line_forwarded(line: &str) -> bool {
    line.starts_with("download:")
        || line.starts_with("postprocess:")
        || line.starts_with("[Merger]")
        || line.starts_with("[VideoRemuxer]")
        || line.starts_with("[ExtractAudio]")
        || line.starts_with("[VideoConvertor]")
        || line.starts_with("[EmbedSubtitle]")
        || line.starts_with("[download] Destination:")
        || looks_like_bare_progress(line)
}

/// Retry attempts reuse same Controls: clear stale progress so next
/// attempt's Destination fold does not double-count prior attempt bytes.
fn reset_ctrl_for_retry(ctrl: &Controls, attempt_idx: usize) {
    if attempt_idx == 0 {
        return;
    }
    ctrl.downloaded.store(0, Ordering::SeqCst);
    ctrl.total.store(0, Ordering::SeqCst);
    ctrl.phase.store(PHASE_EXTRACTING, Ordering::SeqCst);
}

/// Adopt gate: extensionless seed (old rule), weak URL-derived stem
/// (`x.mp4` from x.com), or capture carried a page title (user came from
/// a page dialog; seeded name was not a deliberate rename).
fn title_adopt_gate(filename: &str, url: &str, has_page_title: bool) -> bool {
    !filename.contains('.')
        || crate::model::filename_stem_is_weak_for_title(filename, url)
        || has_page_title
}

/// Pure adopt decision: `Some("{title}.{ext}")` sanitized when title
/// non-empty and gate passes, else None (keep current filename).
fn title_adopt_name(
    filename: &str,
    url: &str,
    title: &str,
    ext: &str,
    has_page_title: bool,
) -> Option<String> {
    let title = title.trim();
    if title.is_empty() || !title_adopt_gate(filename, url, has_page_title) {
        return None;
    }
    Some(crate::model::sanitize_filename(&format!("{title}.{ext}")))
}

/// Cheap title re-derive when -J probe failed: yt-dlp download extractor
/// knows title even when dump path does not. First non-empty stdout line.
async fn fetch_title_fallback(
    path: &std::path::Path,
    task: &DownloadTask,
    jar: Option<&std::path::Path>,
) -> Option<String> {
    let mut full = base_args(&task.url, task, jar);
    let url = full.pop()?;
    full.extend([
        "--skip-download".to_string(),
        "--print".to_string(),
        "%(title)s".to_string(),
    ]);
    full.push(url);
    let path = path.to_path_buf();
    let out = tokio::task::spawn_blocking(move || run_timeout(&path, &full, 120))
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Download via yt-dlp. On success returns completed task; caller falls back to native on Err.
pub async fn download_with_ytdlp(
    task: DownloadTask,
    ctrl: Arc<Controls>,
) -> Result<DownloadTask, String> {
    ctrl.phase.store(PHASE_EXTRACTING, Ordering::Relaxed);
    let path = ensure_ytdlp().await?;
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    // Format choice: -J first; fallback selector lets yt-dlp decide.
    // Also adopt video title as filename when seeded name is extensionless ID.
    // One jar for probe + download (yt-dlp may rewrite jar between calls).
    let jar = write_cookie_jar(&task);
    let (dump, used_anon) = match fetch_formats_jar(&task.url, &task, jar.as_deref()).await {
        Ok((d, u)) => (Some(d), u),
        Err(_) => (None, false),
    };
    let mut task = task;
    // Explicit x-yt-* choice (picker) wins over auto selector.
    let choice = YtChoice::from_headers(&task.headers);
    // Probe-failure fallback: title for adopt below when -J dump unknown.
    // ponytail: extra -J-class call only on probe failure; skip if needed
    // later by timing it out shorter.
    let fallback_title = if dump.is_none() {
        fetch_title_fallback(&path, &task, jar.as_deref()).await
    } else {
        None
    };
    let (fmt_arg, extra_args, audio_mode) = build_selector(&choice, dump.as_ref());
    {
        let title = dump
            .as_ref()
            .map(|d| d.title.as_str())
            .or(fallback_title.as_deref())
            .unwrap_or("");
        let ext = if audio_mode {
            choice.extract.as_deref().unwrap_or("m4a")
        } else {
            "mp4"
        };
        let has_page_title = task
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("x-page-title"));
        if let Some(name) = title_adopt_name(&task.filename, &task.url, title, ext, has_page_title)
        {
            task.filename = name;
        }
    }
    if audio_mode {
        // Dest must carry audio ext or post-dl stem rename hunts wrong file.
        // No extract: keep user audio ext, else chosen format ext, else m4a.
        let audio_exts = ["mp3", "m4a", "opus", "webm", "ogg", "flac", "wav"];
        let cur0 = std::path::Path::new(&task.filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let want = if let Some(ex) = &choice.extract {
            ex.clone()
        } else if audio_exts.contains(&cur0.as_str()) {
            cur0.clone()
        } else if let Some(fid) = choice
            .format
            .as_deref()
            .and_then(|f| f.strip_prefix("audio:"))
        {
            dump.as_ref()
                .and_then(|d| d.formats.iter().find(|x| x.format_id == fid))
                .map(|x| x.ext.clone())
                .unwrap_or_else(|| "m4a".into())
        } else {
            "m4a".into()
        };
        if cur0 != want {
            let stem = std::path::Path::new(&task.filename)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("audio")
                .to_string();
            task.filename = format!("{stem}.{want}");
        }
    }
    // Video dest must carry .mp4: extensionless -o breaks --embed-subs
    // (ffmpeg cannot infer muxer for '<dest>.temp'). Covers probe-failed
    // path where title rename never ran.
    if !audio_mode && std::path::Path::new(&task.filename).extension().is_none() {
        task.filename = format!("{}.mp4", task.filename);
    }
    // dest AFTER filename mutation: -o target and returned task must agree.
    let dest = task.dest_dir.join(&task.filename);
    let mut args: Vec<String> = vec![
        "--newline".into(),
        "--encoding".into(),
        "utf-8".into(),
        "--progress-template".into(),
        // `download:` before colon is yt-dlp progress *type selector*, not
        // printed text; second `download:` is emitted prefix for parser.
        "download:download:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s".into(),
        "--progress-template".into(),
        "postprocess:postprocess:%(progress.status)s".into(),
        "-f".into(),
        fmt_arg,
        "--concurrent-fragments".into(),
        "4".into(),
    ];
    if !audio_mode {
        args.push("--merge-output-format".into());
        args.push("mp4".into());
    }
    args.extend(extra_args);
    if let Some(ff) = ffmpeg_location() {
        args.push("--ffmpeg-location".into());
        args.push(ff);
    }
    args.push("-o".into());
    args.push(dest.to_string_lossy().into_owned());
    // URL last (base_args appends it); insert flags before it.
    let mut full = base_args(&task.url, &task, jar.as_deref());
    let url = full.pop().expect("url");
    full.extend(args.clone());
    full.push(url);
    // Two sessions: cookied (`full`) and anonymous (`full2`). Order from
    // probe adoption: adopted anon dump -> anon first, cookied fallback on
    // any failure; else cookied first, anon only on retryable error.
    let t_anon = anon_task(&task);
    let jar2 = write_cookie_jar(&t_anon);
    let mut full2 = base_args(&t_anon.url, &t_anon, jar2.as_deref());
    let url2 = full2.pop().expect("url");
    full2.extend(args);
    full2.push(url2);

    let started = std::time::SystemTime::now();
    let order = download_attempt_order(used_anon);
    let attempts: [Vec<String>; 2] = [full, full2];
    // attempts indexed by order: false=cookied(0), true=anon(1)
    let mut last_err: Option<String> = None;
    for (idx, is_anon) in order.iter().enumerate() {
        reset_ctrl_for_retry(&ctrl, idx);
        let args_try = attempts[usize::from(*is_anon)].clone();
        let ctrl2 = ctrl.clone();
        let path2 = path.clone();
        let out =
            tokio::task::spawn_blocking(move || run_ytdlp_download(&path2, &args_try, &ctrl2))
                .await
                .map_err(|e| e.to_string())?;
        match out {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) if e == "cancelled" => {
                let mut t = task.clone();
                t.status = TaskStatus::Paused;
                t.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
                return Ok(t);
            }
            Err(e) => {
                let is_last = idx + 1 == order.len();
                // Cookied-first: anon gated on retryable error. Anon-first:
                // cookied fallback unconditional.
                let may_retry = !is_last && (used_anon || should_retry_anon(&task.url, &task, &e));
                if !may_retry {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }
    // yt-dlp may keep source ext despite -o exact path: find dest or newest stem.* sibling.
    // Exclude intermediates (.f<id>., .part/.ytdl) and files predating this run.
    if !dest.is_file() {
        let stem = dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        if let Ok(rd) = std::fs::read_dir(&task.dest_dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if name
                    .rsplit('.')
                    .next()
                    .is_some_and(|ext| ext == "part" || ext == "ytdl")
                {
                    continue;
                }
                if p.file_stem().and_then(|s| s.to_str()) == Some(stem.as_str()) && p != dest {
                    // stem.f401.mp4 = unmerged intermediate, skip; stem.webm = final renamed ext, keep.
                    if name[stem.len()..].starts_with(".f") {
                        continue;
                    }
                    if let Ok(m) = e.metadata().and_then(|mm| mm.modified()) {
                        if m >= started && best.as_ref().is_none_or(|(t, _)| m > *t) {
                            best = Some((m, p));
                        }
                    }
                }
            }
        }
        match best {
            Some((_, p)) => {
                std::fs::rename(&p, &dest).map_err(|e| e.to_string())?;
            }
            None => {
                // yt-dlp merger failed silently in past runs: mux leftover .f<id> tracks ourselves.
                let stem = dest
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let mut tracks: Vec<PathBuf> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&task.dest_dir) {
                    for e in rd.flatten() {
                        let p = e.path();
                        let name = p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string();
                        if name.starts_with(&format!("{stem}.f"))
                            && !name.ends_with(".part")
                            && !name.ends_with(".ytdl")
                        {
                            tracks.push(p);
                        }
                    }
                }
                tracks.sort();
                if tracks.len() >= 2 {
                    crate::engine::mux_tracks(&tracks, &dest)
                        .await
                        .map_err(|e| format!("yt-dlp merge failed; fallback mux failed: {e}"))?;
                    if dest.is_file() {
                        for t in &tracks {
                            let _ = std::fs::remove_file(t);
                        }
                    } else {
                        return Err("yt-dlp finished but output file not found".into());
                    }
                } else {
                    return Err("yt-dlp finished but output file not found".into());
                }
            }
        }
    }
    ctrl.phase.store(PHASE_FINALIZING, Ordering::Relaxed);
    // Zero-byte or wrong-magic output must not Complete.
    if !dest.is_file() {
        return Err("yt-dlp produced empty file".into());
    }
    let mut dest = dest;
    match crate::engine::validate_output(&dest, &task.filename) {
        Ok(_) => {}
        Err(crate::engine::ValidateFail::TsInMp4) => {
            dest = crate::engine::ensure_mp4_or_ts(&dest)
                .await
                .map_err(|e| format!("yt-dlp output not valid mp4: {e}"))?;
        }
        Err(e) => {
            return Err(
                if std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == 0 {
                    "yt-dlp produced empty file".into()
                } else {
                    format!("yt-dlp output not valid: {e}")
                },
            );
        }
    }
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err("yt-dlp produced empty file".into());
    }
    ctrl.downloaded.store(size, Ordering::Relaxed);
    ctrl.total.store(size, Ordering::Relaxed);
    let mut t = task;
    if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
        t.filename = name;
    }
    t.status = TaskStatus::Complete;
    t.downloaded = size;
    t.total = Some(size);
    Ok(t)
}

fn ffmpeg_location() -> Option<String> {
    // Must be absolute: yt-dlp resolves --ffmpeg-location relative to its own CWD,
    // and bare "ffmpeg" can miss when child CWD differs. PATH search mirrors ytdlp_path.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
        .map(|p| p.to_string_lossy().into_owned())
}

fn run_ytdlp_download(
    path: &std::path::Path,
    args: &[String],
    ctrl: &Arc<Controls>,
) -> Result<(), String> {
    let mut cmd = Command::new(path);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::platform::no_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("yt-dlp: no stdout")?;
    let stderr = child.stderr.take().ok_or("yt-dlp: no stderr")?;
    // Capture stderr tail for error surfacing (piped-but-undrained deadlocks).
    let err_tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let err_tail2 = err_tail.clone();
    let _err_thread = std::thread::spawn(move || {
        // Lossy read: yt-dlp stdout/stderr is cp1252 on Windows; strict
        // UTF-8 lines() errors, dropping pipe -> child broken-pipe crash.
        let mut r = BufReader::new(stderr);
        let mut raw: Vec<u8> = Vec::new();
        use std::io::Read;
        let _ = r.read_to_end(&mut raw);
        for line in String::from_utf8_lossy(&raw).lines() {
            let mut g = err_tail2.lock().unwrap_or_else(|e| e.into_inner());
            g.push(line.to_string());
            if g.len() > 20 {
                g.remove(0);
            }
        }
    });
    let video_base = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ctrl_c = ctrl.clone();
    // Reader thread: progress lines. New Destination folds prior file bytes into base (counters restart per file).
    let base2 = video_base.clone();
    let reader = std::thread::spawn(move || {
        // read_until + lossy: strict lines() dies on cp1252 title bytes
        // (Flávio), drops stdout pipe, yt-dlp crashes broken-pipe Errno 22
        // while printing subtitle path. Must drain to EOF regardless.
        let mut r = BufReader::new(stdout);
        let mut raw: Vec<u8> = Vec::new();
        loop {
            raw.clear();
            match r.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.contains("[download] Destination:") {
                let cur = ctrl_c.downloaded.load(Ordering::Relaxed);
                if cur > base2.load(Ordering::Relaxed) {
                    base2.store(cur, Ordering::Relaxed);
                }
            }
            if stdout_line_forwarded(line) {
                handle_stdout_line(line, &ctrl_c, &base2);
            }
        }
    });
    loop {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("cancelled".into());
        }
        if ctrl.paused.load(Ordering::Relaxed) {
            // ponytail: no cross-platform suspend; kill and let caller mark Paused. Resume re-invokes yt-dlp (keeps .part).
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("cancelled".into());
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let _ = reader.join();
                let _ = _err_thread.join();
                if status.success() {
                    return Ok(());
                }
                let t = err_tail
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .join("\n");
                return Err(if t.trim().is_empty() {
                    "yt-dlp exited with error".into()
                } else {
                    format!("yt-dlp failed: {}", tail(&t))
                });
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// Obvious non-item page: site root or bare feed/home root (host-agnostic).
/// Bare roots only (optional trailing slash); deep paths stay promotable.
pub fn is_feed_or_home_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let path = u.path().trim_end_matches('/').to_ascii_lowercase();
    matches!(
        path.as_str(),
        "" | "/home" | "/feed" | "/explore" | "/notifications" | "/messages" | "/search"
    )
}

pub fn is_video_page(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let path = u.path().to_ascii_lowercase();
    let h = host.strip_prefix("www.").unwrap_or(&host);
    let h = h.strip_prefix("m.").unwrap_or(h);
    let segs: Vec<&str> = path.split('/').collect();
    let seg = |i: usize| segs.get(i).copied().unwrap_or("");
    let is_digits = |s: &str, min: usize| {
        s.len() >= min && !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    };
    let host_match = match h {
        "youtube.com" => {
            path.starts_with("/watch")
                || path.starts_with("/shorts/")
                || path.starts_with("/live/")
                || ((seg(1) == "embed" || seg(1) == "v" || seg(1) == "clip")
                    && !seg(2).is_empty())
        }
        "youtube-nocookie.com" => {
            path.starts_with("/watch")
                || ((seg(1) == "embed" || seg(1) == "shorts" || seg(1) == "live" || seg(1) == "v")
                    && !seg(2).is_empty())
        }
        "youtu.be" => path.len() > 1,
        "music.youtube.com" => path.starts_with("/watch"),
        "vimeo.com" => {
            path.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
                || (seg(1) == "video" && is_digits(seg(2), 5))
                || ((seg(1) == "channels" || seg(1) == "groups")
                    && !seg(2).is_empty()
                    && (is_digits(seg(3), 5)
                        || (seg(3) == "videos" && is_digits(seg(4), 5))))
        }
        "dailymotion.com" => {
            path.starts_with("/video/")
                || (seg(1) == "embed" && seg(2) == "video" && !seg(3).is_empty())
        }
        "dai.ly" => path.len() > 1,
        "twitch.tv" => {
            path.starts_with("/videos/")
                || (!seg(1).is_empty() && seg(2) == "clip" && !seg(3).is_empty())
        }
        "clips.twitch.tv" => path.len() > 1 && !seg(1).is_empty(),
        "tiktok.com" => path.contains("/video/") || path.contains("/photo/"),
        "instagram.com" => {
            ["/reel/", "/reels/", "/p/", "/tv/"]
                .iter()
                .any(|p| path.starts_with(p) && path.len() > p.len())
                || (path.starts_with("/stories/")
                    && path.trim_end_matches('/').matches('/').count() >= 3)
        }
        "twitter.com" | "x.com" => path.contains("/status/"),
        "facebook.com" | "fb.watch" => {
            h == "fb.watch"
                || path.contains("/videos/")
                || path.starts_with("/watch")
                || path.starts_with("/reel/")
                || (seg(1) == "share"
                    && (seg(2) == "r" || seg(2) == "v")
                    && !seg(3).is_empty())
        }
        "threads.net" | "threads.com" => {
            seg(1).starts_with('@')
                && seg(1).len() > 1
                && seg(2) == "post"
                && seg(3).len() >= 5
                && seg(3)
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
        "reddit.com" => path.contains("/comments/"),
        "soundcloud.com" => path.matches('/').count() >= 2,
        // Globo (h already www./m.-stripped; ends_with covers g1./ge. subs):
        // .ghtml articles embed video (GloboArticle extractor); /v/<id> player.
        _ if h.ends_with("globo.com") => {
            path.ends_with(".ghtml") || (seg(1) == "v" && is_digits(seg(2), 1))
        }
        "bilibili.com" => {
            path.starts_with("/video/")
                || (seg(1) == "bangumi"
                    && seg(2) == "play"
                    && ["ep", "ss"].iter().any(|pre| {
                        seg(3)
                            .strip_prefix(pre)
                            .is_some_and(|rest| is_digits(rest, 1))
                    }))
        }
        "nicovideo.jp" => {
            seg(1) == "watch" && {
                let id = seg(2);
                let rest = ["sm", "nm", "so", "lv"]
                    .iter()
                    .find_map(|pre| id.strip_prefix(pre))
                    .unwrap_or(id);
                is_digits(rest, 1)
            }
        }
        "rumble.com" => {
            let rest = path
                .strip_prefix("/embed/")
                .unwrap_or_else(|| path.strip_prefix('/').unwrap_or(&path));
            rest.strip_prefix('v').is_some_and(|id| {
                id.len() >= 4 && id.bytes().take(4).all(|b| b.is_ascii_alphanumeric())
            })
        }
        _ => false,
    };
    if host_match {
        return true;
    }
    // PeerTube, any host: /w/<id>, /videos/watch/<id>, /videos/embed/<id>.
    let pt_id_ok =
        |s: &str| s.len() >= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    (seg(1) == "w" && pt_id_ok(seg(2)))
        || (seg(1) == "videos"
            && (seg(2) == "watch" || seg(2) == "embed")
            && pt_id_ok(seg(3)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn title_adopt_gate_cases() {
        // Weak host stem + dot: adopt (bug case).
        assert_eq!(
            title_adopt_name(
                "x.mp4",
                "https://x.com/user/status/123",
                "Cool Clip",
                "mp4",
                false
            ),
            Some("Cool Clip.mp4".into())
        );
        // Deliberate descriptive name: keep.
        assert_eq!(
            title_adopt_name(
                "My Video.mp4",
                "https://x.com/user/status/123",
                "Cool Clip",
                "mp4",
                false
            ),
            None
        );
        // Extensionless seed: old rule still adopts.
        assert_eq!(
            title_adopt_name(
                "1894729834",
                "https://example.com/watch?v=1",
                "Cool Clip",
                "mp4",
                false
            ),
            Some("Cool Clip.mp4".into())
        );
        // Blank title never adopts; page-title header forces adopt.
        assert_eq!(
            title_adopt_name("x.mp4", "https://x.com/a", "  ", "mp4", false),
            None
        );
        assert_eq!(
            title_adopt_name("My Video.mp4", "https://x.com/a", "Cool Clip", "mp4", true),
            Some("Cool Clip.mp4".into())
        );
        // Audio ext flows through unchanged stem.
        assert_eq!(
            title_adopt_name("x.mp4", "https://x.com/a", "Cool Clip", "mp3", false),
            Some("Cool Clip.mp3".into())
        );
    }
    #[test]
    fn stdout_line_phases_and_estimate() {
        let ctrl = Controls::new(0);
        let base = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Estimate fills total when real total NA.
        handle_stdout_line("download:500|NA|1000|123", &ctrl, &base);
        assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_DOWNLOADING);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), 1000);
        assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), 500);
        // Real total wins over estimate, never shrink.
        handle_stdout_line("download:600|2000|900|123", &ctrl, &base);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), 2000);
        handle_stdout_line("download:700|1500|900|123", &ctrl, &base);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), 2000);
        // NA estimate + NA total: total unchanged.
        handle_stdout_line("download:700|NA|NA|NA", &ctrl, &base);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), 2000);
        // Postprocess + bracket phases.
        handle_stdout_line("postprocess:started", &ctrl, &base);
        assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_PROCESSING);
        handle_stdout_line("[Merger] Merging formats", &ctrl, &base);
        assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_PROCESSING);
        handle_stdout_line("postprocess:finished", &ctrl, &base);
        assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_FINALIZING);
        // phase_name mapping.
        assert_eq!(crate::engine::phase_name(0), "Downloading");
        assert_eq!(crate::engine::phase_name(1), "Extracting video");
        assert_eq!(crate::engine::phase_name(2), "Merging & processing");
        assert_eq!(crate::engine::phase_name(4), "Reconstructing file");
        assert_eq!(crate::engine::phase_name(3), "Finalizing");
        assert_eq!(crate::engine::phase_name(99), "Downloading");
    }
    #[test]
    fn retry_reset_clears_progress_after_first_attempt() {
        let ctrl = Controls::new(0);
        ctrl.downloaded.store(500, Ordering::Relaxed);
        ctrl.total.store(1000, Ordering::Relaxed);
        ctrl.phase.store(PHASE_DOWNLOADING, Ordering::Relaxed);

        reset_ctrl_for_retry(&ctrl, 0); // first attempt: no-op
        assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), 500);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), 1000);
        assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_DOWNLOADING);

        for idx in [1, 2] {
            ctrl.downloaded.store(500, Ordering::Relaxed);
            ctrl.total.store(1000, Ordering::Relaxed);
            ctrl.phase.store(PHASE_FINALIZING, Ordering::Relaxed);
            reset_ctrl_for_retry(&ctrl, idx);
            assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), 0);
            assert_eq!(ctrl.total.load(Ordering::Relaxed), 0);
            assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_EXTRACTING);
        }

        // Post-reset progress folds only new attempt bytes, not stale base.
        let base = Arc::new(std::sync::atomic::AtomicU64::new(0));
        handle_stdout_line("download:500|1000|NA|1", &ctrl, &base);
        assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), 500);
        assert!(ctrl.downloaded.load(Ordering::Relaxed) <= 1000);
    }
    #[test]
    fn youtube_url_cases() {
        for u in [
            "https://youtube.com/watch?v=x",
            "https://www.youtube.com/watch?v=x",
            "https://m.youtube.com/watch?v=x",
            "https://music.youtube.com/watch?v=x",
            "https://youtu.be/x",
        ] {
            assert!(is_youtube_url(u), "{u}");
        }
        for u in [
            "https://evil-youtube.com/watch?v=x",
            "https://youtube.com.evil.com/watch?v=x",
            "https://notyoutu.be/x",
            "https://example.com/",
            "not a url",
        ] {
            assert!(!is_youtube_url(u), "{u}");
        }
    }
    #[test]
    fn retry_anon_cases() {
        let mut t = DownloadTask {
            cookies: "a=b".into(),
            ..Default::default()
        };
        let err = "ERROR [youtube]: page needs to be reloaded";
        assert!(should_retry_anon(
            "https://www.youtube.com/watch?v=x",
            &t,
            err
        ));
        assert!(should_retry_anon(
            "https://youtu.be/x",
            &t,
            "Sign in to confirm you're not a bot"
        ));
        assert!(should_retry_anon(
            "https://www.youtube.com/watch?v=x",
            &t,
            "page needs be reloaded"
        ));
        t.cookies.clear();
        t.cookies_json = r#"[{"name":"a"}]"#.into();
        assert!(should_retry_anon(
            "https://m.youtube.com/watch?v=x",
            &t,
            err
        ));
        t.cookies_json.clear();
        assert!(!should_retry_anon(
            "https://www.youtube.com/watch?v=x",
            &t,
            err
        ));
        t.cookies = "a=b".into();
        assert!(!should_retry_anon("https://example.com/", &t, err));
        assert!(!should_retry_anon(
            "https://www.youtube.com/watch?v=x",
            &t,
            "Video unavailable"
        ));
    }
    #[test]
    fn retry_anon_requested_format_not_available() {
        let t = DownloadTask {
            cookies_json: r#"[{"name":"a","value":"b"}]"#.into(),
            ..Default::default()
        };
        assert!(should_retry_anon(
            "https://www.youtube.com/watch?v=x",
            &t,
            "ERROR: Requested format not available. Use --list-formats"
        ));
        assert!(should_retry_anon(
            "https://www.youtube.com/watch?v=x",
            &t,
            "error: requested format not available"
        ));
    }
    #[test]
    fn download_attempt_order_anon_first_when_adopted() {
        assert_eq!(download_attempt_order(true), [true, false]);
        assert_eq!(download_attempt_order(false), [false, true]);
    }
    #[test]
    fn base_args_proxy_from_task() {
        let mut t = DownloadTask::default();
        let a = base_args("https://example.com/v", &t, None);
        assert!(!a.iter().any(|x| x == "--proxy"), "args {a:?}");
        t.proxy_url = " socks5://127.0.0.1:1080 ".into();
        let a = base_args("https://example.com/v", &t, None);
        assert!(
            a.windows(2)
                .any(|w| w == ["--proxy", "socks5://127.0.0.1:1080"]),
            "args {a:?}"
        );
    }
    #[test]
    fn base_args_js_runtimes_node() {
        let t = DownloadTask::default();
        let a = base_args("https://example.com/v", &t, None);
        let has = a.windows(2).any(|w| w == ["--js-runtimes", "node"]);
        assert_eq!(has, has_node(), "args {a:?}");
    }
    #[test]
    fn video_page_detection() {
        assert!(is_video_page("https://www.youtube.com/watch?v=aqz-KE-bpKQ"));
        assert!(is_video_page("https://youtu.be/aqz-KE-bpKQ"));
        assert!(is_video_page("https://vimeo.com/12345"));
        assert!(is_video_page("https://www.twitch.tv/videos/123"));
        assert!(!is_video_page("https://example.com/file.mp4"));
        assert!(is_video_page("https://www.instagram.com/reel/ABC123/"));
        assert!(is_video_page("https://www.instagram.com/reels/ABC123/"));
        assert!(is_video_page("https://www.instagram.com/p/ABC123/"));
        assert!(is_video_page("https://www.instagram.com/tv/ABC123/"));
        assert!(is_video_page(
            "https://www.instagram.com/stories/someuser/1234567890/"
        ));
        assert!(!is_video_page("https://www.instagram.com/someuser/"));
        assert!(!is_video_page("https://www.instagram.com/explore/"));
        assert!(!is_video_page(
            "https://www.instagram.com/stories/someuser/"
        ));
        assert!(!is_video_page("https://www.youtube.com/feed/subscriptions"));
        assert!(!is_video_page("https://vimeo.com/about"));
        assert!(!is_video_page("not a url"));
        // Parity widenings (mirror rank.js videoPageKind).
        for page in [
            "https://www.threads.net/@someuser/post/C8xAbCdEfG/",
            "https://www.threads.com/@someuser/post/C8xAbCdEfGh",
            "https://www.facebook.com/share/r/1aBcDeFgHiJ/",
            "https://www.facebook.com/share/v/1aBcDeFgHiJ/",
            "https://www.facebook.com/reel/123456789",
            "https://www.twitch.tv/somechannel/clip/FunnySlug123",
            "https://clips.twitch.tv/FunnySlug123",
            "https://www.tiktok.com/@someuser/photo/7331234567890123456",
            "https://www.tiktok.com/@someuser/video/7331234567890123456",
            "https://vimeo.com/channels/staffpicks/12345678",
            "https://vimeo.com/groups/someproj/videos/12345678",
            "https://vimeo.com/video/12345",
            "https://www.dailymotion.com/embed/video/x8abc12",
            "https://www.bilibili.com/bangumi/play/ep123456",
            "https://www.bilibili.com/bangumi/play/ss12345",
            "https://www.nicovideo.jp/watch/sm12345678",
            "https://www.nicovideo.jp/watch/12345678",
            "https://www.nicovideo.jp/watch/lv1234567",
            "https://rumble.com/v1a2b3c-example-title.html",
            "https://rumble.com/embed/v1a2b3c/",
            "https://peertube.example/w/p9jmLXa9q3xYvZk7mNqW2a",
            "https://peertube.example/videos/watch/p9jmLXa9q3xYvZk7mNqW2a",
            "https://peertube.example/videos/embed/p9jmLXa9q3xYvZk7mNqW2a",
            "https://example.com/w/p9jmLXa9q3xYvZk7mNqW2a",
            "https://g1.globo.com/mundo/noticia/2026/10/22/story.ghtml",
            "https://globoplay.globo.com/v/15107769/",
            "https://www.youtube-nocookie.com/embed/Eo38zL1kLko",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/DyvaWC4kIYw?feature=share",
            "https://www.youtube.com/shorts/DyvaWC4kIYw",
            "https://www.youtube-nocookie.com/shorts/DyvaWC4kIYw",
        ] {
            assert!(is_video_page(page), "page missed: {page}");
        }
        for not_page in [
            "https://www.threads.net/",
            "https://www.threads.net/@someuser",
            "https://www.twitch.tv/directory",
            "https://www.twitch.tv/directory/category/fortnite",
            "https://vimeo.com/channels",
            "https://vimeo.com/channels/staffpicks",
            "https://vimeo.com/groups/someproj",
            "https://www.facebook.com/groups/123456789/",
            "https://www.facebook.com/share/",
            "https://v.dailymotion.example/embed/video/x8abc12",
            "https://www.nicovideo.jp/watch/smx123",
            "https://www.nicovideo.jp/watch/nm",
            "https://rumble.com/v123",
            "https://example.com/w/short",
            "https://www.bilibili.com/bangumi/play/ep",
            "https://g1.globo.com/",
            "https://g1.globo.com/mundo/",
            "https://globo.com.evil.example/story.ghtml",
            "https://www.youtube-nocookie.com/",
            "https://www.youtube-nocookie.com.evil.example/embed/Eo38zL1kLko",
        ] {
            // Note: dailymotion/nicovideo/bilibili hosts are checked above only
            // for their real hosts; v.dailymotion.example must not match.
            assert!(!is_video_page(not_page), "non-page matched: {not_page}");
        }
        for feed in [
            "https://x.com/home",
            "https://x.com/",
            "https://x.com",
            "https://www.instagram.com/",
            "https://www.instagram.com/explore/",
            "https://example.com/feed",
            "https://example.com/FEED/?q=1",
            "https://example.com/notifications/",
            "https://example.com/messages",
            "https://example.com/search?q=x",
        ] {
            assert!(is_feed_or_home_url(feed), "feed missed: {feed}");
        }
        for item in [
            "https://x.com/u/status/123",
            "https://www.instagram.com/reel/ABC/",
            "https://unknown-site.example/watch/1",
            "https://example.com/home/video-1",
            "https://example.com/feedback",
            "not a url",
        ] {
            assert!(!is_feed_or_home_url(item), "item flagged: {item}");
        }
    }
    fn vf(height: Option<u32>, vcodec: &str, ext: &str) -> YtFormat {
        YtFormat {
            format_id: "x".into(),
            ext: ext.into(),
            width: None,
            height,
            fps: None,
            vcodec: Some(vcodec.into()),
            acodec: Some("mp4a".into()),
            tbr: None,
            abr: None,
            language: None,
            format_note: None,
            filesize: None,
            filesize_approx: None,
        }
    }

    #[test]
    fn degraded_360p_dump_triggers_anon_reprobe_only_with_cookies() {
        let degraded = YtDump {
            title: "t".into(),
            uploader: None,
            channel: None,
            thumbnail: None,
            duration: None,
            formats: vec![
                vf(Some(360), "avc1", "mp4"),
                vf(None, "none", "mhtml"),
                vf(None, "none", "mhtml"),
            ],
            entries: Vec::new(),
            subtitles: std::collections::BTreeMap::new(),
            automatic_captions: std::collections::BTreeMap::new(),
        };
        assert!(dump_is_degraded(&degraded));
        let url = "https://www.youtube.com/watch?v=abc";
        let cookied = DownloadTask {
            cookies: "SID=x".into(),
            ..Default::default()
        };
        assert!(should_reprobe_anon_degraded(url, &cookied, &degraded));
        assert!(!should_reprobe_anon_degraded(
            url,
            &DownloadTask::default(),
            &degraded
        ));
        let full = YtDump {
            formats: vec![
                vf(Some(360), "avc1", "mp4"),
                vf(Some(720), "avc1", "mp4"),
                vf(Some(2160), "vp9", "webm"),
            ],
            ..degraded.clone()
        };
        assert!(!dump_is_degraded(&full));
        assert!(!should_reprobe_anon_degraded(url, &cookied, &full));
        assert!(dump_video_quality(&full) > dump_video_quality(&degraded));
        let only720 = YtDump {
            formats: vec![vf(Some(720), "avc1", "mp4")],
            ..degraded.clone()
        };
        assert!(!dump_is_degraded(&only720));
        assert!(!should_reprobe_anon_degraded(url, &cookied, &only720));
        let many_low = YtDump {
            formats: vec![
                vf(Some(360), "avc1", "mp4"),
                vf(Some(240), "avc1", "mp4"),
                vf(Some(144), "avc1", "mp4"),
            ],
            ..degraded.clone()
        };
        assert!(!dump_is_degraded(&many_low));
        assert!(!should_reprobe_anon_degraded(
            "https://vimeo.com/1",
            &cookied,
            &degraded
        ));
    }

    #[test]
    fn pick_best_prefers_height_over_muxed() {
        let fs = vec![
            YtFormat {
                format_id: "a".into(),
                ext: "mp4".into(),
                width: None,
                height: Some(720),
                fps: None,
                vcodec: Some("avc".into()),
                acodec: Some("none".into()),
                tbr: None,
                abr: None,
                language: None,
                format_note: None,
                filesize: None,
                filesize_approx: None,
            },
            YtFormat {
                format_id: "b".into(),
                ext: "mp4".into(),
                width: None,
                height: Some(360),
                fps: None,
                vcodec: Some("avc".into()),
                acodec: Some("aac".into()),
                tbr: None,
                abr: None,
                language: None,
                format_note: None,
                filesize: None,
                filesize_approx: None,
            },
            YtFormat {
                format_id: "sb".into(),
                ext: "mhtml".into(),
                width: None,
                height: None,
                fps: None,
                vcodec: Some("none".into()),
                acodec: Some("none".into()),
                tbr: None,
                abr: None,
                language: None,
                format_note: None,
                filesize: None,
                filesize_approx: None,
            },
        ];
        assert_eq!(pick_best(&fs).unwrap().format_id, "a");
        let fs2 = vec![YtFormat {
            format_id: "a".into(),
            ext: "mp4".into(),
            width: None,
            height: Some(720),
            fps: None,
            vcodec: Some("avc".into()),
            acodec: Some("none".into()),
            tbr: None,
            abr: None,
            language: None,
            format_note: None,
            filesize: None,
            filesize_approx: None,
        }];
        assert_eq!(pick_best(&fs2).unwrap().format_id, "a");
    }

    #[test]
    fn selector_quality_tier_and_auto_sort() {
        // Auto: no pinning even with probe dump; -S sort in extra.
        let c = YtChoice::default();
        let (s, extra, _) = build_selector(&c, None);
        assert_eq!(s, "bv*+ba/b");
        assert!(extra.contains(&"-S".to_string()));
        assert!(extra.contains(&"res,fps,vcodec:avc,acodec:aac".to_string()));
        // Tier 1080: capped, generic fallback, sort applied.
        let c = YtChoice {
            quality_height: Some(1080),
            ..Default::default()
        };
        let (s, extra, _) = build_selector(&c, None);
        assert_eq!(
            s,
            "bv*[height<=1080][height>0]+ba/b[height<=1080][height>0]/bv*+ba/b"
        );
        assert!(extra.contains(&"-S".to_string()));
        // Tier round-trips through headers; exact format beats tier, no sort.
        let mut h = std::collections::BTreeMap::new();
        c.write_headers(&mut h);
        assert_eq!(YtChoice::from_headers(&h), c);
        assert_eq!(h.get("x-yt-quality").map(|s| s.as_str()), Some("1080"));
        let c2 = YtChoice {
            format: Some("137".into()),
            quality_height: Some(1080),
            ..Default::default()
        };
        let (s, extra, _) = build_selector(&c2, None);
        assert_eq!(s, "137+bestaudio/137/bv*+ba/b");
        assert!(!extra.contains(&"-S".to_string()));
        // Audio-only unchanged, no sort.
        let c3 = YtChoice {
            format: Some("audio:ba".into()),
            ..Default::default()
        };
        let (s, extra, audio) = build_selector(&c3, None);
        assert_eq!(s, "ba/b");
        assert!(audio && !extra.contains(&"-S".to_string()));
        // Tier + specific audio: id replaces ba in first arm only.
        let c = YtChoice {
            quality_height: Some(720),
            audio: Some("140".into()),
            ..Default::default()
        };
        let (s, extra, _) = build_selector(&c, None);
        assert_eq!(
            s,
            "bv*[height<=720][height>0]+140/b[height<=720][height>0]/bv*+ba/b"
        );
        assert!(extra.contains(&"-S".to_string()));
        // Tier + best/none audio: generic selector unchanged.
        for a in ["best", "none"] {
            let c = YtChoice {
                quality_height: Some(720),
                audio: Some(a.into()),
                ..Default::default()
            };
            let (s, _, _) = build_selector(&c, None);
            assert_eq!(
                s, "bv*[height<=720][height>0]+ba/b[height<=720][height>0]/bv*+ba/b",
                "audio={a}"
            );
        }
        // Reader filter forwards postprocess bracket lines, not noise.
        assert!(stdout_line_forwarded(
            "[Merger] Merging formats into \"x.mp4\""
        ));
        assert!(stdout_line_forwarded("[VideoRemuxer] Not remuxing"));
        assert!(stdout_line_forwarded("[ExtractAudio] Destination: x.mp3"));
        assert!(stdout_line_forwarded("[VideoConvertor] Converting video"));
        assert!(stdout_line_forwarded(
            "[EmbedSubtitle] Embedding subtitles in x.mkv"
        ));
        assert!(stdout_line_forwarded("[download] Destination: x.f137.mp4"));
        assert!(stdout_line_forwarded("download:1|2|3|4"));
        assert!(stdout_line_forwarded("postprocess:started"));
        assert!(!stdout_line_forwarded("[youtube] Extracting URL"));
        assert!(!stdout_line_forwarded("[download] 12.3% of 10MiB"));
        assert!(!stdout_line_forwarded("[info] Writing video metadata"));
        // Bug A real format: bare (old eaten prefix) + prefixed (new template).
        assert!(stdout_line_forwarded("1024|11295255|NA|510940.67"));
        assert!(stdout_line_forwarded("download:1024|11295255|NA|510940.67"));
        assert!(!stdout_line_forwarded("1024|11295255"));
        for line in [
            "1024|11295255|NA|510940.67",
            "download:1024|11295255|NA|510940.67",
        ] {
            let ctrl = Controls::new(0);
            let base = Arc::new(std::sync::atomic::AtomicU64::new(0));
            handle_stdout_line(line, &ctrl, &base);
            assert_eq!(ctrl.phase.load(Ordering::Relaxed), PHASE_DOWNLOADING);
            assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), 1024);
            assert_eq!(ctrl.total.load(Ordering::Relaxed), 11295255);
        }
    }

    #[test]
    fn selector_video_only_without_audio_choice_adds_bestaudio() {
        // No dump: format 18 unknown -> must not emit bare "18" (silent video risk).
        let c = YtChoice {
            format: Some("18".into()),
            ..Default::default()
        };
        let (s, _, _) = build_selector(&c, None);
        assert_eq!(s, "18+bestaudio/18/bv*+ba/b");
        // With dump proving muxed, id + generic fallback (id can rotate away).
        let d: YtDump = serde_json::from_str(
            r#"{"title":"t","formats":[{"format_id":"18","ext":"mp4","vcodec":"avc","acodec":"aac"}]}"#,
        )
        .unwrap();
        let (s, _, _) = build_selector(&c, Some(&d));
        assert_eq!(s, "18/bv*+ba/b");
        // Muxed + stale audio choice: audio ignored.
        let c2 = YtChoice {
            format: Some("18".into()),
            audio: Some("251".into()),
            ..Default::default()
        };
        let (s, _, _) = build_selector(&c2, Some(&d));
        assert_eq!(s, "18/bv*+ba/b");
        // Auto + specific audio.
        let c3 = YtChoice {
            audio: Some("251".into()),
            ..Default::default()
        };
        let (s, _, _) = build_selector(&c3, None);
        assert_eq!(s, "bv*+251/bv*+ba/b");
    }

    #[test]
    fn selector_from_choice() {
        let c = YtChoice {
            format: Some("137".into()),
            audio: Some("251".into()),
            ..Default::default()
        };
        let (s, extra, audio) = build_selector(&c, None);
        assert_eq!(s, "137+251/137+bestaudio/bv*+ba/b");
        assert!(extra.is_empty() && !audio);
        let c = YtChoice {
            format: Some("audio:251".into()),
            extract: Some("mp3".into()),
            ..Default::default()
        };
        let (s, extra, audio) = build_selector(&c, None);
        assert_eq!(s, "251/ba/b");
        assert!(audio && extra.contains(&"-x".to_string()));
        let c = YtChoice {
            subs: vec!["en".into()],
            auto_subs: vec!["vi".into()],
            sub_embed: true,
            ..Default::default()
        };
        let (_, extra, _) = build_selector(&c, None);
        assert!(extra.contains(&"--write-subs".to_string()));
        assert!(extra.contains(&"--write-auto-subs".to_string()));
        assert!(extra.contains(&"en,vi".to_string()));
        assert!(extra.contains(&"--embed-subs".to_string()));
        // Round-trip through headers; pseudo-headers stripped from base_args.
        let mut h = std::collections::BTreeMap::new();
        c.write_headers(&mut h);
        assert_eq!(YtChoice::from_headers(&h), c);
        let t = crate::model::DownloadTask {
            headers: h,
            ..Default::default()
        };
        let a = base_args("https://x/watch", &t, None);
        assert!(!a
            .iter()
            .any(|x| x.starts_with("x-yt") || x.starts_with("x-ytdlp")));
    }

    #[test]
    fn cookie_jar_netscape_format() {
        let t = crate::model::DownloadTask {
            cookies_json: r#"[{"name":"a","value":"1","domain":"example.com","path":"/","expirationDate":1893456000.0,"secure":true,"httpOnly":true,"hostOnly":false},{"name":"b","value":"2","domain":".example.com","path":"/x","secure":false,"httpOnly":false,"hostOnly":true}]"#.into(),
            ..Default::default()
        };
        let jar = write_cookie_jar(&t).expect("jar");
        let s = std::fs::read_to_string(&jar).unwrap();
        assert!(
            s.contains("#HttpOnly_.example.com\tTRUE\t/\tTRUE\t1893456000\ta\t1"),
            "{s}"
        );
        assert!(s.contains(".example.com\tFALSE\t/x\tFALSE\t0\tb\t2"), "{s}");
        // sweep: stale jar removed, fresh jar + non-jar files kept
        let dir = std::env::temp_dir().join(format!("wc-sweep-{}", crate::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new, other) = (
            dir.join("cookies-1-old.txt"),
            dir.join("cookies-1-new.txt"),
            dir.join("keep.txt"),
        );
        for p in [&old, &new, &other] {
            std::fs::write(p, "x").unwrap();
        }
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
        for p in [&old, &other] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(past))
                .unwrap();
        }
        sweep_cookie_jars(&dir);
        assert!(!old.exists() && new.exists() && other.exists());
        let _ = std::fs::remove_dir_all(&dir);
        // RAII: drop removes the jar file (no manual remove_file needed).
        assert!(
            std::path::Path::new(&*jar).exists(),
            "jar alive while guard alive"
        );
        let jar_path = jar.to_path_buf();
        drop(jar);
        assert!(!jar_path.exists(), "guard drop removes jar");
        // Jar present: --cookies used, Cookie header suppressed.
        let mut h = std::collections::BTreeMap::new();
        h.insert("Cookie".to_string(), "a=1".to_string());
        let t2 = crate::model::DownloadTask {
            cookies: "a=1".into(),
            headers: h,
            ..Default::default()
        };
        let a = base_args(
            "https://x/watch",
            &t2,
            Some(std::path::Path::new("jar.txt")),
        );
        assert!(a.iter().any(|x| x == "--cookies"));
        assert!(!a.iter().any(|x| x.starts_with("Cookie:")));
    }

    #[test]
    fn cookie_jar_all_filtered_returns_none() {
        // Empty domains filtered -> zero lines -> None (legacy header fallback).
        let t = crate::model::DownloadTask {
            cookies_json:
                r#"[{"name":"a","value":"1","domain":"","path":"/"},{"name":"b","value":"2"}]"#
                    .into(),
            ..Default::default()
        };
        assert!(write_cookie_jar(&t).is_none());
        let t = crate::model::DownloadTask {
            cookies_json: r#"[]"#.into(),
            ..Default::default()
        };
        assert!(write_cookie_jar(&t).is_none());
    }

    #[test]
    fn manifest_url_detection() {
        assert!(is_manifest_url("https://cdn.globo.com/v/pl.m3u8"));
        assert!(is_manifest_url(
            "https://cdn.globo.com/v/PL.M3U8?token=x#frag"
        ));
        assert!(is_manifest_url("https://x/v/pl.mpd?a=1"));
        assert!(!is_manifest_url("https://x/watch?f=.m3u8"));
        assert!(!is_manifest_url(
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ"
        ));
        assert!(!is_manifest_url("https://x/v/pl.m3u8x"));
    }

    #[test]
    fn base_args_manifest_forces_generic_and_strips_page_title() {
        let mut h = std::collections::BTreeMap::new();
        h.insert("X-Page-Title".to_string(), "Some Title".to_string());
        h.insert("Referer".to_string(), "https://x/".to_string());
        let t = crate::model::DownloadTask {
            headers: h,
            ..Default::default()
        };
        let a = base_args("https://cdn.globo.com/v/pl.m3u8?sig=1", &t, None);
        assert!(a.iter().any(|x| x == "--force-generic-extractor"), "{a:?}");
        assert!(
            !a.iter()
                .any(|x| x.to_ascii_lowercase().starts_with("x-page-title")),
            "{a:?}"
        );
        assert!(a.iter().any(|x| x.starts_with("Referer:")), "{a:?}");
        // Page URL: no generic flag.
        let a = base_args("https://globo.com/g1/play/123", &t, None);
        assert!(!a.iter().any(|x| x == "--force-generic-extractor"), "{a:?}");
        assert!(
            !a.iter()
                .any(|x| x.to_ascii_lowercase().starts_with("x-page-title")),
            "{a:?}"
        );
    }

    #[test]
    fn base_args_strips_denied_and_pseudo_headers() {
        let mut h = std::collections::BTreeMap::new();
        h.insert("Range".to_string(), "bytes=0-99".to_string());
        h.insert("Accept-Encoding".to_string(), "gzip".to_string());
        h.insert("Host".to_string(), "evil.example".to_string());
        h.insert("X-Rep-Id".to_string(), "1".to_string());
        h.insert("X-Variant-Index".to_string(), "2".to_string());
        h.insert("X-Nzb".to_string(), "3".to_string());
        h.insert("User-Agent".to_string(), "BrowserUA/1".to_string());
        h.insert("Authorization".to_string(), "Bearer tok".to_string());
        let t = crate::model::DownloadTask {
            headers: h,
            ..Default::default()
        };
        let a = base_args("https://example.com/v", &t, None);
        for denied in ["Range:", "Accept-Encoding:", "Host:", "X-Rep-Id:", "X-Variant-Index:", "X-Nzb:"] {
            assert!(
                !a.iter().any(|x| x.to_ascii_lowercase().starts_with(&denied.to_ascii_lowercase())),
                "{denied} leaked: {a:?}"
            );
        }
        assert!(a.iter().any(|x| x.starts_with("User-Agent:")), "{a:?}");
        assert!(a.iter().any(|x| x.starts_with("Authorization:")), "{a:?}");
    }

    #[test]
    fn flatten_entries_descends_to_first_formatted_entry() {
        // Playlist fixture: formats nested in entries[0] (page-URL probe shape).
        let j = r#"{"title":"Playlist","formats":[],"entries":[{"title":"","formats":[],"entries":[]},{"title":"Entry Vid","duration":42.0,"uploader":"Globo","formats":[{"format_id":"hls-4370","ext":"mp4","height":1080,"vcodec":"avc1","acodec":"mp4a"}],"subtitles":{"pt":[{"ext":"srt"}]},"automatic_captions":{},"entries":[]}],"subtitles":{},"automatic_captions":{}}"#;
        let d = serde_json::from_str::<YtDump>(j).unwrap().flatten_entries();
        assert_eq!(d.formats.len(), 1);
        assert_eq!(d.formats[0].format_id, "hls-4370");
        assert_eq!(d.title, "Entry Vid");
        assert_eq!(d.uploader(), "Globo");
        assert_eq!(d.sub_langs(), vec!["pt".to_string()]);
        // Entry title empty: parent title kept.
        let j2 = r#"{"title":"Parent Title","formats":[],"entries":[{"title":"","formats":[{"format_id":"f1","ext":"mp4"}]}]}"#;
        let d2 = serde_json::from_str::<YtDump>(j2)
            .unwrap()
            .flatten_entries();
        assert_eq!(d2.title, "Parent Title");
        assert_eq!(d2.formats[0].format_id, "f1");
        // Single video untouched; empty playlist stays empty.
        let j3 = r#"{"title":"Solo","formats":[{"format_id":"f9","ext":"mp4"}],"entries":[]}"#;
        let d3 = serde_json::from_str::<YtDump>(j3)
            .unwrap()
            .flatten_entries();
        assert_eq!(d3.title, "Solo");
        assert!(serde_json::from_str::<YtDump>(
            r#"{"title":"P","formats":[],"entries":[{"title":"E","formats":[]}]}"#
        )
        .unwrap()
        .flatten_entries()
        .formats
        .is_empty());
    }

    #[test]
    fn dump_parses_subs_and_meta() {
        let j = r#"{"title":"T","duration":61.5,"uploader":"U","formats":[{"format_id":"137","ext":"mp4","width":1920,"height":1080,"fps":30.0,"vcodec":"avc1","acodec":"none","tbr":2400.0}],"subtitles":{"en":[{"ext":"srt"}]},"automatic_captions":{"vi":[{"ext":"json3"}],"en":[{"ext":"json3"}]}}"#;
        let d: YtDump = serde_json::from_str(j).unwrap();
        assert_eq!(d.uploader(), "U");
        assert_eq!(d.video_formats().len(), 1);
        assert_eq!(d.sub_langs(), vec!["en".to_string()]);
        assert_eq!(d.auto_langs(), vec!["vi".to_string()]);
        assert!(d.video_formats()[0].is_video_only());
    }
}
