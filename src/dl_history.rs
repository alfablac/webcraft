//! Persistent download history: daily JSONL event logs.
//! Caller wires module with `pub mod dl_history;` in `src/lib.rs`.
//! Daily files `dl-YYYY-MM-DD.jsonl` under `config_dir()/dl-history`.
//! Retention goal: 90 days within shared 50 MiB budget — sibling
//! `applog::prune` (not in this crate yet) owns budget enforcement; this
//! module only enforces its own per-file sanity (see `prune_history`).
//! Never panics: all I/O best-effort, corrupt lines skipped on load.
//! Privacy: only `url_host` persisted (never full URL: tokens in query /
//! userinfo); `detail` redacted before write (and not trusted on load).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const KIND_ADDED: &str = "added";
pub const KIND_STARTED: &str = "started";
pub const KIND_PAUSED: &str = "paused";
pub const KIND_RESUMED: &str = "resumed";
pub const KIND_COMPLETED: &str = "completed";
pub const KIND_FAILED: &str = "failed";
pub const KIND_REMOVED: &str = "removed";
/// Speed sample. Two encodings share this kind (both written by
/// `append_speed`): global aggregate (`task_id` empty, `bytes` holds rate
/// truncated to integer bps — `bps` float not stored in JSONL, recovered
/// as `bytes as f64` on load) and per-task cumulative (`task_id` set,
/// `bytes` = bytes downloaded total at sample time; rate derived from
/// consecutive deltas by loaders).
pub const KIND_SPEED: &str = "speed";

/// Retention window in days (`prune_history` drops strictly older days).
pub const MAX_DAYS: u64 = 90;
/// Per-file sanity cap: a single day file larger than this is truncated
/// to its newest lines by `prune_history`. Shared 50 MiB budget across
/// all log dirs enforced by sibling `applog::prune`, not here.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Lines kept when a day file exceeds `MAX_FILE_BYTES`.
pub const MAX_FILE_LINES: usize = 50_000;
/// `detail` cap (chars, char-boundary safe) applied in `append_event`.
pub const MAX_DETAIL_CHARS: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DlEvent {
    pub ts_ms: u64,
    pub kind: String,
    pub task_id: String,
    #[serde(default)]
    pub filename: String,
    /// Host only (see `host_of`); never a full URL.
    #[serde(default)]
    pub url_host: String,
    /// Bytes downloaded (total at event time; for `KIND_SPEED` per-task
    /// samples: cumulative downloaded; global samples: integer bps).
    #[serde(default)]
    pub bytes: u64,
    /// Expected total bytes, 0 = unknown.
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub detail: String,
}

impl DlEvent {
    pub fn new(task_id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            ts_ms: now_ms(),
            kind: kind.into(),
            task_id: task_id.into(),
            ..Self::default()
        }
    }
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = filename.into();
        self
    }
    /// Store host of `url` only; full URL never persisted.
    pub fn with_url(mut self, url: &str) -> Self {
        self.url_host = host_of(url);
        self
    }
    pub fn with_url_host(mut self, url_host: impl Into<String>) -> Self {
        self.url_host = url_host.into();
        self
    }
    pub fn with_bytes(mut self, bytes: u64, total: u64) -> Self {
        self.bytes = bytes;
        self.total = total;
        self
    }
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// Global aggregate speed series point (bytes/sec).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeedPoint {
    pub ts_ms: u64,
    pub bps: f64,
}

/// Aggregates over `KIND_COMPLETED` events (see `stats`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DlStats {
    pub completions: u64,
    /// Sum of `bytes` over completions (fallback `total` when bytes=0).
    pub total_bytes: u64,
    pub total_duration_ms: u64,
    /// `total_bytes * 1000 / total_duration_ms`, 0 when no duration.
    pub avg_bps: f64,
}

pub fn history_dir() -> PathBuf {
    crate::platform::config_dir().join("dl-history")
}

/// Alias for `history_dir`.
pub fn dl_history_dir() -> PathBuf {
    history_dir()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- civil date
// Self-contained duplicate of model::civil_from_days (tiny, no cross-module).

/// UTC day stamp `YYYY-MM-DD` for epoch milliseconds.
pub fn day_stamp(ts_ms: u64) -> String {
    let (y, m, d) = civil_from_days((ts_ms / 86_400_000) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// Parse `dl-YYYY-MM-DD.jsonl` file name into days-since-epoch.
fn day_of_filename(name: &str) -> Option<i64> {
    let stem = name.strip_prefix("dl-")?.strip_suffix(".jsonl")?;
    let mut it = stem.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

pub fn day_file(dir: &Path, ts_ms: u64) -> PathBuf {
    dir.join(format!("dl-{}.jsonl", day_stamp(ts_ms)))
}

// ------------------------------------------------------------------- host

/// Host of `url`: scheme, userinfo, port, path/query/fragment stripped.
/// Handles `scheme://[user[:pass]@]host[:port]/...`, scheme-less
/// `host/path`, IPv6 `[::1]:port`. Empty/garbage input yields `""`
/// (or the bare token before first `/`); never returns userinfo.
pub fn host_of(url: &str) -> String {
    let s = url.trim();
    if s.is_empty() {
        return String::new();
    }
    let after_scheme = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..end];
    // Last '@' separates userinfo (password may itself contain none in
    // valid URLs, but last-@ is the RFC authority split).
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 literal: keep brackets, drop ]:port.
        match rest.find(']') {
            Some(i) => &hostport[..i + 2],
            None => hostport,
        }
    } else {
        match hostport.find(':') {
            Some(i) => &hostport[..i],
            None => hostport,
        }
    };
    host.trim().to_ascii_lowercase()
}

// ----------------------------------------------------------------- redact
// Local tiny redact (self-contained; mirrors chat_history policy subset):
// mask token/password/api_key query + key=value / key: value, URL
// userinfo passwords, Bearer, sk- keys. `#fragment` kept (MEGA keys).

const ASSIGN_KEYS: &[&str] = &[
    "x-api-key",
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "access-token",
    "refresh_token",
    "refresh-token",
    "id_token",
    "auth_token",
    "session_token",
    "client_secret",
    "app_secret",
    "secret_key",
    "private_key",
    "authorization",
    "password",
    "passwd",
    "secret",
    "token",
    "pwd",
];

fn is_sensitive_key(k: &str) -> bool {
    let n = k
        .trim()
        .trim_matches(['"', '\''])
        .to_ascii_lowercase()
        .replace('-', "_");
    matches!(
        n.as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "session_token"
            | "api_key"
            | "apikey"
            | "x_api_key"
            | "password"
            | "passwd"
            | "pwd"
            | "secret"
            | "client_secret"
            | "app_secret"
            | "secret_key"
            | "private_key"
            | "authorization"
            | "auth"
            | "key"
            | "sig"
            | "signature"
    ) || n.ends_with("_token")
        || n.ends_with("_secret")
        || n.ends_with("_password")
        || n.ends_with("_api_key")
}

fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn is_value_delim(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t'
            | b'\r'
            | b'\n'
            | b'&'
            | b'#'
            | b'"'
            | b'\''
            | b','
            | b';'
            | b')'
            | b']'
            | b'}'
            | b'<'
            | b'>'
    )
}

fn mask_bearer(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = find_ci(b, b"bearer", i) {
        let after = pos + 6;
        let ok = (pos == 0 || !b[pos - 1].is_ascii_alphanumeric())
            && after < b.len()
            && (b[after] == b' ' || b[after] == b'\t');
        if !ok {
            out.push_str(&text[i..after.min(text.len())]);
            i = after;
            continue;
        }
        let mut j = after;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && !is_value_delim(b[k]) {
            k += 1;
        }
        if k == j {
            out.push_str(&text[i..after]);
            i = after;
            continue;
        }
        out.push_str(&text[i..j]);
        out.push_str("***");
        i = k;
    }
    out.push_str(&text[i..]);
    out
}

fn mask_sk(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = find_ci(b, b"sk-", i) {
        let mut k = pos + 3;
        while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'-' || b[k] == b'_') {
            k += 1;
        }
        if pos > 0 && b[pos - 1].is_ascii_alphanumeric() || k == pos + 3 {
            let end = (pos + 3).min(text.len());
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        out.push_str(&text[i..pos + 3]);
        out.push_str("***");
        i = k;
    }
    out.push_str(&text[i..]);
    out
}

fn mask_userinfo(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = find_ci(b, b"://", i) {
        let start = pos + 3;
        let mut at = None;
        let mut j = start;
        while j < b.len() {
            if b[j] == b'@' {
                at = Some(j);
                break;
            }
            if matches!(b[j], b'/' | b'?' | b'#' | b' ' | b'\t' | b'"' | b'\'') {
                break;
            }
            j += 1;
        }
        let (Some(at), Some(colon)) = (at, (start..at.unwrap_or(start)).find(|&x| b[x] == b':'))
        else {
            out.push_str(&text[i..start.min(text.len())]);
            i = start;
            continue;
        };
        out.push_str(&text[i..colon + 1]);
        out.push_str("***");
        i = at;
    }
    out.push_str(&text[i..]);
    out
}

fn mask_assignments(text: &str) -> String {
    let b = text.as_bytes();
    let mut keys: Vec<&str> = ASSIGN_KEYS.to_vec();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < b.len() {
        for key in &keys {
            let kb = key.as_bytes();
            if i + kb.len() > b.len() || !b[i..i + kb.len()].eq_ignore_ascii_case(kb) {
                continue;
            }
            if i > 0 && b[i - 1].is_ascii_alphanumeric() {
                continue;
            }
            let mut j = i + kb.len();
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                j += 1;
            }
            let mut p = j;
            while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                p += 1;
            }
            if p >= b.len() || (b[p] != b':' && b[p] != b'=') {
                continue;
            }
            p += 1;
            while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                p += 1;
            }
            let quote = if p < b.len() && (b[p] == b'"' || b[p] == b'\'') {
                let q = b[p];
                p += 1;
                Some(q)
            } else {
                None
            };
            let mut vstart = p;
            if find_ci(b, b"bearer", p) == Some(p) {
                let mut q = p + 6;
                if q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                    while q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                        q += 1;
                    }
                    vstart = q;
                }
            }
            let mut vend = vstart;
            if let Some(q) = quote {
                while vend < b.len() && b[vend] != q {
                    vend += 1;
                }
            } else {
                while vend < b.len() && !is_value_delim(b[vend]) {
                    vend += 1;
                }
            }
            if vend == vstart {
                continue;
            }
            out.push_str(&text[i..vstart]);
            out.push_str("***");
            i = vend;
            continue 'outer;
        }
        let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Mask sensitive query-param values only; `#fragment` kept (MEGA keys).
fn mask_query_params(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'?' {
            let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        let mut end = i + 1;
        while end < b.len()
            && !matches!(
                b[end],
                b'#' | b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'' | b'<' | b'>'
            )
        {
            end += 1;
        }
        out.push('?');
        for (n, part) in text[i + 1..end].split('&').enumerate() {
            if n > 0 {
                out.push('&');
            }
            match part.split_once('=') {
                Some((k, _)) if is_sensitive_key(k) => {
                    out.push_str(k);
                    out.push_str("=***");
                }
                _ => out.push_str(part),
            }
        }
        i = end;
    }
    out
}

/// Mask secrets in free text (detail strings). Pure.
pub fn redact(text: &str) -> String {
    let s = mask_bearer(text);
    let s = mask_sk(&s);
    let s = mask_userinfo(&s);
    let s = mask_assignments(&s);
    mask_query_params(&s)
}

/// Alias for `redact`.
pub fn redact_detail(text: &str) -> String {
    redact(text)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------- storage

fn file_for(dir: &Path, ts_ms: u64) -> PathBuf {
    day_file(dir, ts_ms)
}

/// Append one event to `dl-<day of event ts>.jsonl` in `dir`.
/// `detail` redacted + capped; `url_host` re-derived when it looks like a
/// full URL (defense: callers passing URL by mistake never persist it).
/// Creates `dir`. Never panics; errors ignored.
pub fn append_event(dir: &Path, event: &DlEvent) {
    let mut clean = event.clone();
    clean.detail = truncate_chars(&redact(&event.detail), MAX_DETAIL_CHARS);
    if clean.url_host.contains("://")
        || clean.url_host.contains('/')
        || clean.url_host.contains('?')
    {
        clean.url_host = host_of(&clean.url_host);
    }
    let _ = std::fs::create_dir_all(dir);
    let Ok(line) = serde_json::to_string(&clean) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_for(dir, clean.ts_ms))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Append to `history_dir()`.
pub fn append_event_default(event: &DlEvent) {
    append_event(&history_dir(), event);
}

/// Record one download lifecycle event stamped now (kind consts above).
#[allow(clippy::too_many_arguments)]
pub fn record(
    dir: &Path,
    kind: &str,
    task_id: &str,
    filename: &str,
    url: &str,
    bytes: u64,
    total: u64,
    duration_ms: u64,
    detail: &str,
) {
    append_event(
        dir,
        &DlEvent {
            ts_ms: now_ms(),
            kind: kind.to_string(),
            task_id: task_id.to_string(),
            filename: filename.to_string(),
            url_host: host_of(url),
            bytes,
            total,
            duration_ms,
            detail: detail.to_string(),
        },
    );
}

// ------------------------------------------------------------------ speed
// Global aggregate point: task_id empty, bytes = bps truncated (see
// KIND_SPEED). Per-task variant keeps cumulative downloaded in bytes.

/// Append global aggregate speed sample (`bps` bytes/sec at `ts_ms`).
pub fn append_speed(dir: &Path, ts_ms: u64, bps: f64) {
    let bps = if bps.is_finite() && bps > 0.0 {
        bps
    } else {
        0.0
    };
    append_event(
        dir,
        &DlEvent {
            ts_ms,
            kind: KIND_SPEED.to_string(),
            bytes: bps as u64,
            ..DlEvent::default()
        },
    );
}

/// Append global speed sample to `history_dir()`.
pub fn append_speed_default(ts_ms: u64, bps: f64) {
    append_speed(&history_dir(), ts_ms, bps);
}

/// Append per-task cumulative speed sample: `bytes` = bytes downloaded
/// total for `task_id` at `ts_ms` (`total` optional expected size).
pub fn append_task_speed(dir: &Path, ts_ms: u64, task_id: &str, bytes: u64, total: u64) {
    append_event(
        dir,
        &DlEvent {
            ts_ms,
            kind: KIND_SPEED.to_string(),
            task_id: task_id.to_string(),
            bytes,
            total,
            ..DlEvent::default()
        },
    );
}

// ------------------------------------------------------------------- load

fn day_files_between(dir: &Path, from_ms: u64, to_ms: u64) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let from_day = (from_ms / 86_400_000) as i64;
    let to_day = (to_ms.max(from_ms) / 86_400_000) as i64;
    let mut days: Vec<(i64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            day_of_filename(&name)
                .filter(|d| (from_day..=to_day).contains(d))
                .map(|d| (d, e.path()))
        })
        .collect();
    days.sort_by_key(|(d, _)| *d);
    days.into_iter().map(|(_, p)| p).collect()
}

fn load_file(path: &Path, out: &mut Vec<DlEvent>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    out.extend(
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<DlEvent>(l).ok()),
    );
}

/// All events with `from_ms <= ts_ms <= to_ms` (inclusive) across daily
/// files covering the range, sorted by `ts_ms`. Missing dir/files,
/// corrupt lines: skipped. Sort stable: same-ms events keep file order.
pub fn load_range(dir: &Path, from_ms: u64, to_ms: u64) -> Vec<DlEvent> {
    if to_ms < from_ms {
        return Vec::new();
    }
    let mut out = Vec::new();
    for p in day_files_between(dir, from_ms, to_ms) {
        load_file(&p, &mut out);
    }
    out.retain(|e| (from_ms..=to_ms).contains(&e.ts_ms));
    out.sort_by_key(|e| e.ts_ms);
    out
}

/// All events in daily file for `ts_ms`'s day (empty when missing).
pub fn load_day(dir: &Path, ts_ms: u64) -> Vec<DlEvent> {
    let mut out = Vec::new();
    load_file(&file_for(dir, ts_ms), &mut out);
    out.sort_by_key(|e| e.ts_ms);
    out
}

/// Newest `limit` events in range (range order preserved); 0 = empty.
pub fn load_recent(dir: &Path, from_ms: u64, to_ms: u64, limit: usize) -> Vec<DlEvent> {
    if limit == 0 {
        return Vec::new();
    }
    let all = load_range(dir, from_ms, to_ms);
    let from = all.len().saturating_sub(limit);
    all[from..].to_vec()
}

/// Events of one `kind` in range (exact match).
pub fn load_kind(dir: &Path, from_ms: u64, to_ms: u64, kind: &str) -> Vec<DlEvent> {
    load_range(dir, from_ms, to_ms)
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

/// Events for one task in range (exact match, range order).
pub fn load_task(dir: &Path, from_ms: u64, to_ms: u64, task_id: &str) -> Vec<DlEvent> {
    load_range(dir, from_ms, to_ms)
        .into_iter()
        .filter(|e| e.task_id == task_id)
        .collect()
}

// ----------------------------------------------------------------- series

fn bucket_avg(samples: &[(u64, f64)], bucket_ms: u64) -> Vec<SpeedPoint> {
    if samples.is_empty() || bucket_ms == 0 {
        return Vec::new();
    }
    let mut out: Vec<SpeedPoint> = Vec::new();
    let mut bucket = u64::MAX;
    let mut sum = 0.0f64;
    let mut n = 0u64;
    for &(ts, v) in samples {
        let b = ts / bucket_ms;
        if b != bucket {
            if n > 0 {
                out.push(SpeedPoint {
                    ts_ms: bucket * bucket_ms,
                    bps: sum / n as f64,
                });
            }
            bucket = b;
            sum = 0.0;
            n = 0;
        }
        sum += v;
        n += 1;
    }
    if n > 0 {
        out.push(SpeedPoint {
            ts_ms: bucket * bucket_ms,
            bps: sum / n as f64,
        });
    }
    out
}

/// Rates between consecutive cumulative samples per task (mirrors
/// `app::task_speed_series` semantics on epoch-ms samples): decreasing
/// counters (restart/reset) clamp to 0 via saturating subtraction.
fn rates_from_events(events: &[DlEvent]) -> Vec<(u64, f64)> {
    let mut by_task: std::collections::HashMap<&str, Vec<&DlEvent>> =
        std::collections::HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for e in events {
        if e.kind != KIND_SPEED || e.task_id.is_empty() {
            continue;
        }
        let v = by_task.entry(e.task_id.as_str()).or_default();
        if v.is_empty() {
            order.push(e.task_id.as_str());
        }
        v.push(e);
    }
    let mut out = Vec::new();
    for id in order {
        let mut s = by_task.remove(id).unwrap_or_default();
        s.sort_by_key(|e| e.ts_ms);
        for w in s.windows(2) {
            let dt_ms = w[1].ts_ms.saturating_sub(w[0].ts_ms);
            if dt_ms > 0 {
                let db = w[1].bytes.saturating_sub(w[0].bytes);
                out.push((w[1].ts_ms, db as f64 * 1000.0 / dt_ms as f64));
            }
        }
    }
    out.sort_by_key(|(ts, _)| *ts);
    out
}

/// Pure: bucket-average global speed samples from already-loaded events.
/// Global samples (`task_id` empty) are authoritative when present and
/// contribute stored bps directly. Otherwise per-task cumulative samples
/// contribute derived rates summed per timestamp across tasks (aggregate
/// at that instant; mixing both would double-count). Bucket start =
/// `ts_ms / bucket_ms * bucket_ms`. `bucket_ms == 0` or none: empty.
pub fn speed_series(events: &[DlEvent], bucket_ms: u64) -> Vec<SpeedPoint> {
    // (ts, aggregate-bps-at-ts): sum simultaneous task rates so bucket
    // average weights instants, not tasks.
    let mut instants: Vec<(u64, f64)> = events
        .iter()
        .filter(|e| e.kind == KIND_SPEED && e.task_id.is_empty())
        .map(|e| (e.ts_ms, e.bytes as f64))
        .collect();
    if instants.is_empty() {
        let mut agg: std::collections::HashMap<u64, f64> = std::collections::HashMap::new();
        for (ts, r) in rates_from_events(events) {
            *agg.entry(ts).or_default() += r;
        }
        instants = agg.into_iter().collect();
        instants.sort_by_key(|(ts, _)| *ts);
    }
    bucket_avg(&instants, bucket_ms)
}

/// Pure: per-task bucket-averaged series from cumulative samples.
pub fn task_speed_series(events: &[DlEvent], task_id: &str, bucket_ms: u64) -> Vec<SpeedPoint> {
    let mut ev: Vec<DlEvent> = events
        .iter()
        .filter(|e| e.kind == KIND_SPEED && e.task_id == task_id)
        .cloned()
        .collect();
    ev.sort_by_key(|e| e.ts_ms);
    let rates: Vec<(u64, f64)> = ev
        .windows(2)
        .filter_map(|w| {
            let dt = w[1].ts_ms.saturating_sub(w[0].ts_ms);
            (dt > 0).then(|| {
                (
                    w[1].ts_ms,
                    w[1].bytes.saturating_sub(w[0].bytes) as f64 * 1000.0 / dt as f64,
                )
            })
        })
        .collect();
    bucket_avg(&rates, bucket_ms)
}

/// Global speed series loaded from daily files in range, bucket-averaged.
pub fn load_speed_series(dir: &Path, from_ms: u64, to_ms: u64, bucket_ms: u64) -> Vec<SpeedPoint> {
    speed_series(&load_range(dir, from_ms, to_ms), bucket_ms)
}

/// Per-task speed series loaded from daily files in range.
pub fn load_task_speed_series(
    dir: &Path,
    from_ms: u64,
    to_ms: u64,
    task_id: &str,
    bucket_ms: u64,
) -> Vec<SpeedPoint> {
    task_speed_series(&load_range(dir, from_ms, to_ms), task_id, bucket_ms)
}

// ------------------------------------------------------------------ stats

/// Aggregates over `KIND_COMPLETED` events in slice: count, bytes
/// (falls back to `total` when `bytes == 0`), duration, mean rate
/// `bytes*1000/duration_ms`.
pub fn stats(events: &[DlEvent]) -> DlStats {
    let mut s = DlStats::default();
    for e in events.iter().filter(|e| e.kind == KIND_COMPLETED) {
        s.completions += 1;
        s.total_bytes += if e.bytes > 0 { e.bytes } else { e.total };
        s.total_duration_ms += e.duration_ms;
    }
    if s.total_duration_ms > 0 {
        s.avg_bps = s.total_bytes as f64 * 1000.0 / s.total_duration_ms as f64;
    }
    s
}

/// Alias view: (completions, total_bytes, total_duration_ms).
pub fn totals(events: &[DlEvent]) -> (u64, u64, u64) {
    let s = stats(events);
    (s.completions, s.total_bytes, s.total_duration_ms)
}

/// Stats over events loaded from daily files in range.
pub fn load_stats(dir: &Path, from_ms: u64, to_ms: u64) -> DlStats {
    stats(&load_range(dir, from_ms, to_ms))
}

/// `(completions, total_bytes, total_duration_ms)` in range.
pub fn load_totals(dir: &Path, from_ms: u64, to_ms: u64) -> (u64, u64, u64) {
    totals(&load_range(dir, from_ms, to_ms))
}

/// Completed events only, in range.
pub fn load_completions(dir: &Path, from_ms: u64, to_ms: u64) -> Vec<DlEvent> {
    load_kind(dir, from_ms, to_ms, KIND_COMPLETED)
}

// ------------------------------------------------------------------ prune

/// Delete day files strictly older than `now_ms - max_days` days and
/// truncate oversized day files to newest `MAX_FILE_LINES` lines
/// (per-file sanity only; shared budget owned by `applog::prune`).
/// Returns number of files deleted. Never panics.
pub fn prune_history_with(dir: &Path, now_ms_val: u64, max_days: u64) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff_day = (now_ms_val / 86_400_000) as i64 - max_days as i64;
    let mut deleted = 0usize;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(day) = day_of_filename(&name) else {
            continue;
        };
        let path = e.path();
        if day < cutoff_day {
            if std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
            continue;
        }
        // Per-file sanity: oversized file keeps newest lines.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = content.lines().collect();
                let from = lines.len().saturating_sub(MAX_FILE_LINES);
                let kept = &lines[from..];
                let mut out = kept.join("\n");
                if !out.is_empty() {
                    out.push('\n');
                }
                // Still oversized with few huge lines: keep newest line.
                let mut kept: Vec<&str> = kept.to_vec();
                while out.len() as u64 > MAX_FILE_BYTES && kept.len() > 1 {
                    kept.remove(0);
                    out = kept.join("\n");
                    out.push('\n');
                }
                let _ = std::fs::write(&path, out);
            }
        }
    }
    deleted
}

/// Prune with 90-day retention relative to now.
pub fn prune_history(dir: &Path) -> usize {
    prune_history_with(dir, now_ms(), MAX_DAYS)
}

/// Alias for `prune_history`.
pub fn prune(dir: &Path) -> usize {
    prune_history(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("webcraft-dl-history-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn ev(ts_ms: u64, kind: &str, task_id: &str) -> DlEvent {
        DlEvent {
            ts_ms,
            kind: kind.to_string(),
            task_id: task_id.to_string(),
            ..DlEvent::default()
        }
    }

    const DAY: u64 = 86_400_000;
    // 2024-01-01T00:00:00Z
    const T0: u64 = 1_704_067_200_000;

    #[test]
    fn day_stamp_civil() {
        assert_eq!(day_stamp(0), "1970-01-01");
        assert_eq!(day_stamp(T0), "2024-01-01");
        assert_eq!(day_stamp(T0 + DAY - 1), "2024-01-01");
        assert_eq!(day_stamp(T0 + DAY), "2024-01-02");
        // Leap day + known far date (model.rs: day 20743 = 2026-10-17).
        assert_eq!(day_stamp(1_709_164_800_000), "2024-02-29");
        assert_eq!(day_stamp(20_743 * DAY), "2026-10-17");
        assert_eq!(day_of_filename("dl-2024-01-01.jsonl"), Some(19723));
        assert_eq!(day_of_filename("dl-2024-13-01.jsonl"), None);
        assert_eq!(day_of_filename("other.jsonl"), None);
        assert!(history_dir().ends_with("dl-history"));
    }

    #[test]
    fn host_of_strips_everything_but_host() {
        assert_eq!(
            host_of("https://user:pass@Example.COM:8443/a/b.zip?token=x#frag"),
            "example.com"
        );
        assert_eq!(host_of("https://example.com/f.zip"), "example.com");
        assert_eq!(host_of("example.com:8080/f.zip"), "example.com");
        assert_eq!(host_of("magnet:?xt=urn:btih:abc"), "magnet");
        assert_eq!(host_of("http://[::1]:9000/f"), "[::1]");
        assert_eq!(host_of(""), "");
        assert_eq!(host_of("  https://Sub.Host.IO/x  "), "sub.host.io");
    }

    #[test]
    fn redact_masks_secrets_keeps_fragment() {
        let r = redact("fail https://u:p@h/f?token=secret&ok=1#MEGAKEY password: hunter2 api_key=zzz Bearer tok9");
        assert!(!r.contains("secret"), "{r}");
        assert!(!r.contains("hunter2"), "{r}");
        assert!(!r.contains("zzz"), "{r}");
        assert!(!r.contains("tok9"), "{r}");
        assert!(!r.contains(":p@"), "{r}");
        assert!(r.contains("ok=1") && r.contains("#MEGAKEY"), "{r}");
        assert_eq!(redact("hello world"), "hello world");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn append_load_roundtrip_across_days() {
        let dir = tmp("roundtrip");
        // Out-of-file-order appends across two days + corrupt line injected.
        append_event(&dir, &ev(T0 + DAY + 5, KIND_COMPLETED, "b"));
        append_event(&dir, &ev(T0 + 100, KIND_ADDED, "a"));
        append_event(
            &dir,
            &DlEvent {
                detail: "boom token=leakme".into(),
                url_host: "https://user:pw@Files.Example.com:9/x?token=q".into(),
                ..ev(T0 + 200, KIND_FAILED, "a")
            },
        );
        append_speed(&dir, T0 + 300, 2048.9);
        append_task_speed(&dir, T0 + 400, "a", 1_000, 0);
        append_task_speed(&dir, T0 + 1_400, "a", 3_000, 0);
        std::fs::write(
            dir.join(format!("dl-{}.jsonl", day_stamp(T0))),
            format!(
                "{}\nnot json {{\n",
                std::fs::read_to_string(dir.join(format!("dl-{}.jsonl", day_stamp(T0)))).unwrap()
            ),
        )
        .unwrap();

        assert!(dir.join("dl-2024-01-01.jsonl").exists());
        assert!(dir.join("dl-2024-01-02.jsonl").exists());
        let all = load_range(&dir, T0, T0 + 2 * DAY);
        assert_eq!(all.len(), 6);
        assert_eq!(all[0].kind, KIND_ADDED);
        assert_eq!(all.last().unwrap().kind, KIND_COMPLETED);
        let failed = &all[1];
        assert!(!failed.detail.contains("leakme"), "{}", failed.detail);
        assert_eq!(failed.url_host, "files.example.com");
        // Range slicing + kind/task filters.
        assert_eq!(load_range(&dir, T0 + 150, T0 + 350).len(), 2);
        assert_eq!(load_range(&dir, T0 + DAY, T0).len(), 0);
        assert_eq!(load_kind(&dir, T0, T0 + 2 * DAY, KIND_COMPLETED).len(), 1);
        assert_eq!(load_task(&dir, T0, T0 + 2 * DAY, "a").len(), 4);
        assert_eq!(load_recent(&dir, T0, T0 + 2 * DAY, 2).len(), 2);
        assert!(load_recent(&dir, T0, T0 + 2 * DAY, 0).is_empty());
        assert_eq!(load_day(&dir, T0 + DAY).len(), 1);
        assert_eq!(load_range(&tmp("missing"), 0, u64::MAX).len(), 0);
        // Serde forward-compat: missing optional fields default.
        let e: DlEvent =
            serde_json::from_str(r#"{"ts_ms":1,"kind":"added","task_id":"t"}"#).unwrap();
        assert_eq!(e.bytes, 0);
        assert_eq!(e.url_host, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn speed_series_bucket_average() {
        // Global samples: buckets of 10s.
        let evs = vec![
            DlEvent {
                bytes: 1000,
                ..ev(0, KIND_SPEED, "")
            },
            DlEvent {
                bytes: 3000,
                ..ev(5_000, KIND_SPEED, "")
            },
            DlEvent {
                bytes: 5000,
                ..ev(12_000, KIND_SPEED, "")
            },
            ev(1_000, KIND_COMPLETED, "x"), // ignored
        ];
        let s = speed_series(&evs, 10_000);
        assert_eq!(
            s,
            vec![
                SpeedPoint {
                    ts_ms: 0,
                    bps: 2000.0
                },
                SpeedPoint {
                    ts_ms: 10_000,
                    bps: 5000.0
                },
            ]
        );
        assert!(speed_series(&evs, 0).is_empty());
        assert!(speed_series(&[], 10_000).is_empty());

        // Per-task cumulative: 0->2000 B in 1s, then +3000 B in 1s.
        let evs = vec![
            DlEvent {
                bytes: 0,
                ..ev(0, KIND_SPEED, "a")
            },
            DlEvent {
                bytes: 2000,
                ..ev(1_000, KIND_SPEED, "a")
            },
            DlEvent {
                bytes: 5000,
                ..ev(2_000, KIND_SPEED, "a")
            },
            DlEvent {
                bytes: 100,
                ..ev(0, KIND_SPEED, "b")
            },
            DlEvent {
                bytes: 1100,
                ..ev(1_000, KIND_SPEED, "b")
            },
        ];
        let t = task_speed_series(&evs, "a", 60_000);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].ts_ms, 0);
        assert!((t[0].bps - 2500.0).abs() < 1e-9, "{t:?}");
        // Global view sums simultaneous task rates (a 2000 + b 1000 @1s).
        let g = speed_series(&evs, 60_000);
        assert_eq!(g.len(), 1);
        assert!((g[0].bps - 3000.0).abs() < 1e-9, "{g:?}");
    }

    #[test]
    fn speed_file_roundtrip_series() {
        let dir = tmp("speed");
        append_speed(&dir, T0, 1000.0);
        append_speed(&dir, T0 + 1_000, 3000.0);
        append_task_speed(&dir, T0, "a", 0, 0);
        append_task_speed(&dir, T0 + 1_000, "a", 4000, 0);
        let g = load_speed_series(&dir, T0, T0 + DAY, 10_000);
        assert_eq!(g.len(), 1);
        assert!((g[0].bps - 2000.0).abs() < 1e-9, "{g:?}");
        let t = load_task_speed_series(&dir, T0, T0 + DAY, "a", 10_000);
        assert_eq!(t.len(), 1);
        assert!((t[0].bps - 4000.0).abs() < 1e-9, "{t:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_from_completions() {
        let evs = vec![
            DlEvent {
                bytes: 2000,
                duration_ms: 1000,
                ..ev(0, KIND_COMPLETED, "a")
            },
            DlEvent {
                bytes: 0,
                total: 3000,
                duration_ms: 1000,
                ..ev(1, KIND_COMPLETED, "b")
            },
            DlEvent {
                bytes: 9999,
                duration_ms: 1,
                ..ev(2, KIND_FAILED, "c")
            },
            ev(3, KIND_SPEED, ""),
        ];
        let s = stats(&evs);
        assert_eq!(s.completions, 2);
        assert_eq!(s.total_bytes, 5000);
        assert_eq!(s.total_duration_ms, 2000);
        assert!((s.avg_bps - 2500.0).abs() < 1e-9, "{s:?}");
        assert_eq!(totals(&evs), (2, 5000, 2000));
        assert_eq!(stats(&[]), DlStats::default());
    }

    #[test]
    fn prune_drops_old_days_keeps_recent() {
        let dir = tmp("prune");
        let old = T0;
        let recent = T0 + 100 * DAY;
        append_event(&dir, &ev(old, KIND_ADDED, "old"));
        append_event(&dir, &ev(recent, KIND_ADDED, "new"));
        // Foreign file untouched.
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let deleted = prune_history_with(&dir, recent, MAX_DAYS);
        assert_eq!(deleted, 1);
        assert!(!dir.join("dl-2024-01-01.jsonl").exists());
        assert!(dir.join(format!("dl-{}.jsonl", day_stamp(recent))).exists());
        assert!(dir.join("notes.txt").exists());
        assert_eq!(prune_history_with(&tmp("missing"), recent, MAX_DAYS), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
