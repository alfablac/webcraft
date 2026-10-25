//! Persistent app-wide log: one JSONL file per UTC day under
//! `platform::config_dir()/logs` (`app-YYYY-MM-DD.jsonl`).
//! Caller wires module with `pub mod applog;` in `src/lib.rs`.
//! Secrets redacted before write; messages capped at 1000 chars.
//! `append` also pushes into a bounded in-memory ring (`snapshot`/`clear`,
//! last `MAX_MEM_ENTRIES` since process start) for the Options Activity Log.
//! Retention: log files older than 90 days deleted by `prune`; logs and
//! the sibling `dl-history` dir share a 50 MiB disk budget, enforced by
//! deleting oldest-day files first across both dirs.
//! Never panics: all I/O best-effort, errors ignored.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Shared disk budget for `logs` + sibling `dl-history` dirs.
pub const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
/// Log files older than this many days are pruned.
pub const RETENTION_DAYS: u64 = 90;
/// Message cap (chars, char-boundary safe) applied in `append`.
pub const MAX_MSG_CHARS: usize = 1000;
/// In-memory ring capacity (session entries kept for the UI log).
pub const MAX_MEM_ENTRIES: usize = 500;

pub const LV_INFO: &str = "INFO";
pub const LV_WARN: &str = "WARN";
pub const LV_ERROR: &str = "ERROR";
pub const LV_DEBUG: &str = "DEBUG";

const DAY_MS: u64 = 86_400_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    #[serde(default)]
    pub ts_ms: u64,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub cat: String,
    #[serde(default)]
    pub msg: String,
}

pub fn logs_dir() -> PathBuf {
    crate::platform::config_dir().join("logs")
}

/// Sibling of `logs_dir()` sharing the disk budget.
pub fn history_dir() -> PathBuf {
    crate::platform::config_dir().join("dl-history")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- dates

/// Civil date from days since epoch (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since epoch for a civil date (inverse of `civil_from_days`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// `YYYY-MM-DD` (UTC) for a millisecond epoch timestamp. Pure.
pub fn day_stamp(ts_ms: u64) -> String {
    let (y, m, d) = civil_from_days((ts_ms / DAY_MS) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn today_stamp() -> String {
    day_stamp(now_ms())
}

/// Parse a strict `YYYY-MM-DD` stamp to days since epoch; `None` when
/// malformed or not a real calendar date.
fn stamp_to_days(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    // Round-trip rejects impossible dates (e.g. Feb 30).
    if day_stamp(days as u64 * DAY_MS) == s {
        Some(days)
    } else {
        None
    }
}

/// First valid `YYYY-MM-DD` found anywhere in a file name, as epoch days.
fn day_in_name(name: &str) -> Option<i64> {
    let b = name.as_bytes();
    if b.len() < 10 {
        return None;
    }
    for i in 0..=b.len() - 10 {
        if let Ok(s) = std::str::from_utf8(&b[i..i + 10]) {
            if let Some(days) = stamp_to_days(s) {
                return Some(days);
            }
        }
    }
    None
}

/// Day key for budget ordering: date in file name, else mtime day,
/// else epoch (oldest, deleted first).
fn file_day(path: &Path) -> i64 {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if let Some(days) = day_in_name(name) {
            return days;
        }
    }
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_millis() as u64 / DAY_MS) as i64)
        .unwrap_or(0)
}

fn is_log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("app-") && n.ends_with(".jsonl"))
        .unwrap_or(false)
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect()
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

// ------------------------------------------------------------- mem ring

fn mem_ring() -> &'static std::sync::Mutex<std::collections::VecDeque<LogRecord>> {
    static RING: std::sync::OnceLock<
        std::sync::Mutex<std::collections::VecDeque<LogRecord>>,
    > = std::sync::OnceLock::new();
    RING.get_or_init(|| {
        std::sync::Mutex::new(std::collections::VecDeque::with_capacity(MAX_MEM_ENTRIES))
    })
}

/// Push into the in-memory ring, dropping oldest beyond `MAX_MEM_ENTRIES`.
/// Poison-tolerant, never panics.
fn mem_push(rec: LogRecord) {
    if let Ok(mut q) = mem_ring().lock() {
        if q.len() >= MAX_MEM_ENTRIES {
            q.pop_front();
        }
        q.push_back(rec);
    }
}

/// Snapshot of the in-memory ring, oldest first / newest last (at most
/// `MAX_MEM_ENTRIES`). Clones the bounded buffer only; safe per frame.
pub fn snapshot() -> Vec<LogRecord> {
    mem_ring()
        .lock()
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default()
}

/// Clear the in-memory ring (persistent JSONL files untouched).
pub fn clear() {
    if let Ok(mut q) = mem_ring().lock() {
        q.clear();
    }
}

// ---------------------------------------------------------------- append

/// Append one redacted JSON line to `dir/app-<today>.jsonl` and push the
/// same (redacted, capped) record into the in-memory ring.
/// Creates `dir` as needed. Never panics; I/O errors ignored.
pub fn append(dir: &Path, level: &str, cat: &str, msg: &str) {
    let rec = LogRecord {
        ts_ms: now_ms(),
        level: level.to_string(),
        cat: cat.to_string(),
        msg: redact(msg).chars().take(MAX_MSG_CHARS).collect(),
    };
    mem_push(rec.clone());
    let _ = std::fs::create_dir_all(dir);
    let Ok(line) = serde_json::to_string(&rec) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("app-{}.jsonl", today_stamp())))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Append to `logs_dir()`.
pub fn append_default(level: &str, cat: &str, msg: &str) {
    append(&logs_dir(), level, cat, msg);
}

// ---------------------------------------------------------------- prune

/// Delete retention-expired log files, then enforce the shared budget.
///
/// 1. `dir/app-*.jsonl` files whose day is more than `RETENTION_DAYS`
///    before `now_ms`'s day are deleted.
/// 2. While total size of all files in `dir` + `history_dir` exceeds
///    `MAX_TOTAL_BYTES`, the oldest-day file is deleted first across
///    both dirs (log files in `dir`, any files in `history_dir`; day
///    from file name, falling back to mtime). Never panics.
pub fn prune(dir: &Path, history_dir: &Path, now_ms: u64) {
    let today_days = (now_ms / DAY_MS) as i64;
    let cutoff = today_days - RETENTION_DAYS as i64;

    let mut log_files = list_files(dir);
    // Retention: expired log files only; history retention is its owner's.
    log_files.retain(|p| {
        if is_log_file(p) {
            if let Some(days) = p.file_name().and_then(|n| n.to_str()).and_then(day_in_name) {
                if days < cutoff {
                    let _ = std::fs::remove_file(p);
                    return false;
                }
            }
        }
        true
    });

    let same_dir = history_dir == dir;
    let mut all: Vec<PathBuf> = log_files;
    if !same_dir {
        for p in list_files(history_dir) {
            if !all.contains(&p) {
                all.push(p);
            }
        }
    }
    let mut total: u64 = all.iter().map(|p| file_size(p)).sum();
    if total <= MAX_TOTAL_BYTES {
        return;
    }
    // Budget candidates, oldest day first (path tiebreak: deterministic).
    let mut candidates: Vec<PathBuf> = all
        .iter()
        .filter(|p| is_log_file(p) || (!same_dir && p.starts_with(history_dir)))
        .cloned()
        .collect();
    candidates.sort_by(|a, b| (file_day(a), a).cmp(&(file_day(b), b)));
    for p in candidates {
        if total <= MAX_TOTAL_BYTES {
            break;
        }
        let size = file_size(&p);
        if std::fs::remove_file(&p).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// Prune `logs_dir()` + sibling `history_dir()`.
pub fn prune_default(now_ms: u64) {
    prune(&logs_dir(), &history_dir(), now_ms);
}

// ---------------------------------------------------------------- load

/// Records from `dir/app-*.jsonl` for the last `days` days (including
/// today), oldest first / newest last. Bad lines skipped, missing dir
/// = empty. `days == 0` = empty.
pub fn load(dir: &Path, days: u32) -> Vec<LogRecord> {
    if days == 0 {
        return Vec::new();
    }
    let today_days = (now_ms() / DAY_MS) as i64;
    let cutoff = today_days - (days as i64 - 1);
    let mut files: Vec<(i64, PathBuf)> = list_files(dir)
        .into_iter()
        .filter(|p| is_log_file(p))
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(day_in_name)
                .filter(|d| *d >= cutoff)
                .map(|d| (d, p))
        })
        .collect();
    files.sort();
    let mut out = Vec::new();
    for (_, path) in files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.extend(
                content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<LogRecord>(l).ok()),
            );
        }
    }
    out
}

/// Alias for `load`.
pub fn read_days(dir: &Path, days: u32) -> Vec<LogRecord> {
    load(dir, days)
}

// ---------------------------------------------------------------- redact

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

fn norm_key(k: &str) -> String {
    k.trim()
        .trim_matches(['"', '\''])
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn is_sensitive_key(k: &str) -> bool {
    let n = norm_key(k);
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
        let before_ok = pos == 0 || !b[pos - 1].is_ascii_alphanumeric();
        let after = pos + 6;
        let sep_ok = after < b.len() && (b[after] == b' ' || b[after] == b'\t');
        if !before_ok || !sep_ok {
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
        let before_ok = pos == 0 || !b[pos - 1].is_ascii_alphanumeric();
        let mut k = pos + 3;
        while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'-' || b[k] == b'_') {
            k += 1;
        }
        if !before_ok || k == pos + 3 {
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
        // '@' must come before any path/query/fragment/space delimiter.
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
        let Some(at) = at else {
            out.push_str(&text[i..start.min(text.len())]);
            i = start;
            continue;
        };
        let Some(colon) = (start..at).find(|&x| b[x] == b':') else {
            out.push_str(&text[i..start]);
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

/// Mask `key: value` / `key=value` (incl. quoted JSON forms) for secret keys.
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
            let before_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            if !before_ok {
                continue;
            }
            let mut j = i + kb.len();
            // Optional closing quote of a JSON key: "api_key": ...
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
            // `Authorization: Bearer <tok>`: keep Bearer, mask token only.
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
        // Push one char (UTF-8 safe step).
        let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Mask sensitive query-param values only; fragment (`#...`, MEGA keys) kept.
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
                Some((k, _v)) if is_sensitive_key(k) => {
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

/// Mask secrets: `sk-…`, `Bearer …`, `token/password/api_key/secret`
/// assignments (query, `key=value`, `key: value`, JSON forms) and URL
/// userinfo passwords. Non-sensitive query params and `#fragment`
/// (MEGA decryption keys) preserved. Pure, self-contained.
pub fn redact(text: &str) -> String {
    let s = mask_bearer(text);
    let s = mask_sk(&s);
    let s = mask_userinfo(&s);
    let s = mask_assignments(&s);
    mask_query_params(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("webcraft-applog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn sparse(path: &Path, bytes: u64) {
        let f = std::fs::File::create(path).unwrap();
        f.set_len(bytes).unwrap();
    }

    #[test]
    fn day_stamp_known_dates() {
        assert_eq!(day_stamp(0), "1970-01-01");
        assert_eq!(day_stamp(86_399_999), "1970-01-01");
        assert_eq!(day_stamp(86_400_000), "1970-01-02");
        assert_eq!(day_stamp(1_700_000_000_000), "2023-11-14");
        assert_eq!(day_stamp(951_782_400_000), "2000-02-29"); // leap day
        assert_eq!(day_stamp(1_735_689_600_000), "2025-01-01");
        assert_eq!(today_stamp(), day_stamp(now_ms()));
        assert!(logs_dir().ends_with("logs"));
        assert!(history_dir().ends_with("dl-history"));
        assert_eq!(MAX_TOTAL_BYTES, 50 * 1024 * 1024);
        assert_eq!(RETENTION_DAYS, 90);
        assert_eq!(
            (LV_INFO, LV_WARN, LV_ERROR, LV_DEBUG),
            ("INFO", "WARN", "ERROR", "DEBUG")
        );
    }

    #[test]
    fn redact_masks_secrets_keeps_mega_fragment_and_plain_params() {
        let r = redact("key sk-abcdef1234567890 done Bearer tok123 password: hunter2 api_key=zzz");
        assert!(!r.contains("abcdef1234567890"), "{r}");
        assert!(!r.contains("tok123"), "{r}");
        assert!(!r.contains("hunter2"), "{r}");
        assert!(!r.contains("zzz"), "{r}");
        assert!(r.contains("sk-***"), "{r}");
        assert!(r.contains("Bearer ***"), "{r}");
        let mega = "add https://mega.nz/file/dXQT#ENgUuZqYxmxUWK5tB7ei-mhSurDLjtVzxAEvNdI1GzM";
        assert_eq!(redact(mega), mega);
        let u = redact("https://x/f.zip?token=secret&foo=bar&sig=abc#frag");
        assert!(!u.contains("secret") && !u.contains("abc"), "{u}");
        assert!(u.contains("foo=bar") && u.contains("#frag"), "{u}");
        let j = redact(r#"{"api_key":"supersecret","note":"ok"}"#);
        assert!(!j.contains("supersecret") && j.contains("ok"), "{j}");
        let j2 = redact(r#"{"password":"hunter2","client_secret": "s3"}"#);
        assert!(!j2.contains("hunter2") && !j2.contains("s3"), "{j2}");
        assert_eq!(
            redact("Authorization: Bearer abc123"),
            "Authorization: Bearer ***"
        );
        let ui = redact("https://user:passw0rd@h/f.zip");
        assert!(!ui.contains("passw0rd"), "{ui}");
        assert_eq!(redact("hello world"), "hello world");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn append_load_roundtrip_redacts_and_caps() {
        let dir = tmp("roundtrip");
        append(&dir, LV_INFO, "download", "started token=leakme");
        append(&dir, LV_ERROR, "chat", &"é".repeat(1500));
        let path = dir.join(format!("app-{}.jsonl", today_stamp()));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("leakme"), "{raw}");
        assert_eq!(raw.lines().count(), 2);
        let recs = load(&dir, 1);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].level, LV_INFO);
        assert_eq!(recs[0].cat, "download");
        assert!(recs[0].ts_ms > 0);
        // Newest last; long message capped char-boundary safe.
        assert_eq!(recs[1].cat, "chat");
        assert_eq!(recs[1].msg.chars().count(), MAX_MSG_CHARS);
        assert_eq!(load(&dir, 0), Vec::<LogRecord>::new());
        assert_eq!(load(&tmp("missing"), 7), Vec::<LogRecord>::new());
        // Bad lines skipped.
        std::fs::write(
            &path,
            "not json\n{\"ts_ms\":1,\"level\":\"INFO\",\"cat\":\"app\",\"msg\":\"ok\"}\n",
        )
        .unwrap();
        let recs = load(&dir, 1);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].msg, "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_retention_by_day() {
        let dir = tmp("retention");
        let hist = tmp("retention-hist");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&hist).unwrap();
        let now = 1_700_000_000_000u64; // 2023-11-14
        let old = dir.join(format!("app-{}.jsonl", day_stamp(now - 100 * DAY_MS)));
        let edge = dir.join(format!("app-{}.jsonl", day_stamp(now - 89 * DAY_MS)));
        let recent = dir.join(format!("app-{}.jsonl", day_stamp(now - DAY_MS)));
        let hist_old = hist.join(format!("dl-{}.jsonl", day_stamp(now - 100 * DAY_MS)));
        for p in [&old, &edge, &recent, &hist_old] {
            std::fs::write(p, "{}\n").unwrap();
        }
        prune(&dir, &hist, now);
        assert!(!old.exists(), "older than 90d pruned");
        assert!(edge.exists(), "within 90d kept");
        assert!(recent.exists());
        assert!(hist_old.exists(), "history age not pruned, budget only");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&hist);
    }

    #[test]
    fn prune_budget_oldest_day_first_across_dirs() {
        let dir = tmp("budget");
        let hist = tmp("budget-hist");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&hist).unwrap();
        let now = 1_700_000_000_000u64;
        let mib = 1024 * 1024;
        // 30 MiB history (6d ago) + 30 MiB log (5d ago) = 60 > 50 MiB:
        // only oldest (history) deleted, then 30 MiB fits.
        let hist_old = hist.join(format!("dl-{}.bin", day_stamp(now - 6 * DAY_MS)));
        let log_newer = dir.join(format!("app-{}.jsonl", day_stamp(now - 5 * DAY_MS)));
        let log_today = dir.join(format!("app-{}.jsonl", day_stamp(now)));
        sparse(&hist_old, 30 * mib);
        sparse(&log_newer, 30 * mib);
        std::fs::write(&log_today, "{}\n").unwrap();
        prune(&dir, &hist, now);
        assert!(!hist_old.exists(), "oldest day across dirs deleted first");
        assert!(log_newer.exists(), "budget met after one deletion");
        assert!(log_today.exists());
        // Missing dirs: no panic.
        prune(&tmp("nope-a"), &tmp("nope-b"), now);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&hist);
    }

    #[test]
    fn mem_ring_bounded_redacted_snapshot_and_clear() {
        // Global ring shared with parallel tests: assert on our own tail
        // entries (found by scan), not exact total length.
        clear();
        let dir = tmp("memring");
        append(&dir, LV_INFO, "app", "hello token=leakme");
        append(&dir, LV_ERROR, "download", "boom");
        let snap = snapshot();
        let hello = snap
            .iter()
            .find(|r| r.msg.starts_with("hello"))
            .expect("own entry present");
        assert_eq!(hello.msg, "hello token=***");
        assert!(!hello.msg.contains("leakme"));
        assert_eq!(hello.level, LV_INFO);
        assert!(hello.ts_ms > 0);
        let boom = snap
            .iter()
            .find(|r| r.msg == "boom")
            .expect("own error entry present");
        assert_eq!(boom.level, LV_ERROR);
        // Bounded: overflow drops oldest, newest entries retained.
        for i in 0..(MAX_MEM_ENTRIES + 5) {
            append(&dir, LV_DEBUG, "flood", &format!("m{i}"));
        }
        let snap = snapshot();
        assert!(snap.len() <= MAX_MEM_ENTRIES);
        let newest = format!("m{}", MAX_MEM_ENTRIES + 4);
        assert!(snap.iter().any(|r| r.msg == newest), "newest flood entry kept");
        assert!(!snap.iter().any(|r| r.msg == "m0"), "oldest flood entry dropped");
        clear();
        assert!(
            !snapshot().iter().any(|r| r.msg == newest),
            "clear removes our entries"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
