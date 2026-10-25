use crate::model::*;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub fn build_client(proxy: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("webcraft/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .pool_idle_timeout(std::time::Duration::from_secs(20))
        .pool_max_idle_per_host(8);
    if !proxy.trim().is_empty() {
        if let Ok(p) = reqwest::Proxy::all(proxy.trim()) {
            b = b.proxy(p);
        }
    }
    b.build().unwrap_or_default()
}

/// Minimal RFC 4648 base64 (no dep for one header).
fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Wire headers for HTTP(S) requests, in precedence order:
/// (1) `task.headers` rows EXCLUDING Cookie/Referer (invalid name/value
/// dropped, pseudo-headers stripped), (2) Referer from `task.referrer`,
/// (3) Cookie from `task.cookies`. The referrer/cookies fields are the
/// canonical browser/manual values, so they WIN over Cookie/Referer rows
/// in `task.headers`. Then Basic auth unless Authorization already present.
/// Single source for engine + stream fetches (FTP/SFTP ignore headers).
pub(crate) fn headers_for(task: &DownloadTask) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    for (k, v) in &task.headers {
        // Pseudo-headers are task config, never sent on wire.
        if crate::ytdlp::YT_HEADER_KEYS
            .iter()
            .any(|x| k.eq_ignore_ascii_case(x))
            || k.eq_ignore_ascii_case("x-page-url")
            || k.eq_ignore_ascii_case("x-page-title")
            || k.eq_ignore_ascii_case("x-page-stream")
            || k.eq_ignore_ascii_case("x-page-intent")
            || k.eq_ignore_ascii_case("x-rep-id")
            || k.eq_ignore_ascii_case("x-variant-index")
            || k.eq_ignore_ascii_case("x-nzb")
        {
            continue;
        }
        // Cookie/Referer rows lose to the dedicated fields applied below.
        if k.eq_ignore_ascii_case("cookie") || k.eq_ignore_ascii_case("referer") {
            continue;
        }
        if let (Ok(n), Ok(vv)) = (k.parse::<reqwest::header::HeaderName>(), v.parse()) {
            h.insert(n, vv);
        }
    }
    if !task.referrer.is_empty() {
        if let Ok(v) = task.referrer.parse() {
            h.insert(reqwest::header::REFERER, v);
        }
    }
    if !task.cookies.is_empty() {
        if let Ok(v) = task.cookies.parse() {
            h.insert(reqwest::header::COOKIE, v);
        }
    }
    if !task.username.is_empty() && !h.contains_key(reqwest::header::AUTHORIZATION) {
        let raw = format!("{}:{}", task.username, task.password);
        if let Ok(v) = format!("Basic {}", b64(raw.as_bytes())).parse() {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    h
}

/// Download progress phases (Controls::phase).
pub const PHASE_DOWNLOADING: u8 = 0;
pub const PHASE_EXTRACTING: u8 = 1;
pub const PHASE_PROCESSING: u8 = 2;
pub const PHASE_FINALIZING: u8 = 3;
pub const PHASE_ASSEMBLING: u8 = 4;

pub fn phase_name(phase: u8) -> &'static str {
    match phase {
        PHASE_EXTRACTING => "Extracting video",
        PHASE_PROCESSING => "Merging & processing",
        PHASE_FINALIZING => "Finalizing",
        PHASE_ASSEMBLING => "Reconstructing file",
        _ => "Downloading",
    }
}

pub struct Controls {
    pub paused: AtomicBool,
    pub cancelled: AtomicBool,
    pub downloaded: AtomicU64,
    pub phase: AtomicU8,
    pub limit_kbps: AtomicU64,
    pub rate: std::sync::Mutex<(
        std::time::Instant,
        u64,
        Option<std::time::Instant>,
        u64,
        f64,
    )>,
    /// Total bytes when known (0 = unknown). GUI needs this live; task.total only lands at finish.
    pub total: AtomicU64,
    /// Per-part (done, expected). Segmented HTTP/FTP/single-file use bytes;
    /// manifest downloads use segment counts and set `parts_are_segments`.
    /// Multi-file NZB: one entry per file (bytes, flag false); single-file
    /// NZB keeps segment-count convention (flag true).
    pub parts: std::sync::Mutex<Vec<(u64, u64)>>,
    /// Torrent files aligned 1:1 with `parts`: (librqbit index, relative path).
    /// All torrent files published, unselected included (progress checkboxes);
    /// empty for non-torrent downloads (generic Part/File label fallback).
    pub part_files: std::sync::Mutex<Vec<(usize, String)>>,
    /// Pending live reselection: GUI writes Some(normalized selection, empty=all),
    /// torrent poll loop consumes + applies. None = no request.
    pub desired_files: std::sync::Mutex<Option<Vec<usize>>>,
    pub parts_are_segments: AtomicBool,
    /// Torrent swarm snapshot (only torrent worker writes). Live-only:
    /// never persisted, GUI renders last known for Complete tasks.
    pub torrent: std::sync::Mutex<Option<TorrentSnap>>,
    /// Debrid live info (worker writes at resolution time). Live-only:
    /// never persisted, never logged. Resolved URL carries expiring token.
    pub live: std::sync::Mutex<Option<LiveInfo>>,
}

/// Debrid live channel: original is task.url (persisted); resolved/final
/// only in memory for progress modal + copy. Never persist, never log.
#[derive(Clone, Default)]
pub struct LiveInfo {
    pub debrid_info: String,
    pub resolved_url: String,
    pub final_url: String,
}

/// Redact bearer URL for display: scheme://host/.../last-segment.
/// Tokens sit in path/query on debrid CDNs, so middle+query dropped.
pub fn redact_url(u: &str) -> String {
    let Ok(parsed) = url::Url::parse(u) else {
        return "...".to_string();
    };
    let host = parsed.host_str().unwrap_or("");
    let last = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let last_dec = url::form_urlencoded::byte_serialize(last.as_bytes()).collect::<String>();
    if last.is_empty() {
        format!("{}://{}", parsed.scheme(), host)
    } else {
        format!("{}://{}/.../{}", parsed.scheme(), host, last_dec)
    }
}

/// Plain-data torrent swarm snapshot for Info dialog. No seeds/leecher split
/// exists in librqbit: peers_live = connected peers (seeds+leechers mixed).
#[derive(Clone, Default)]
pub struct TorrentSnap {
    pub trackers: Vec<String>,
    pub peers_live: u32,
    pub peers_connecting: u32,
    pub peers_seen: u32,
    pub uploaded: u64,
    pub pieces_done: u64,
    pub pieces_total: u32,
    pub info_hash: String,
    pub live: bool,
}
/// Adaptive per-download concurrency, learned from real segment GETs only
/// (never probes). Before first 429: optimistic at user max. First 429 batch:
/// limit locks to currently-admitted (206 in-flight) count — server's
/// learned max (OVH speedtest: 2). Every GET (first tries AND retries)
/// must acquire a permit; parked/429'd workers re-acquire. Regain: slow,
/// only after a full clean streak, never above user max.
/// ponytail: learned limit not persisted; restart re-learns in one burst.
pub struct ConcurrencyGate {
    inner: std::sync::Mutex<GateState>,
    max: usize,
}
struct GateState {
    limit: usize,
    in_flight: usize,
    clean_streak: u32,
    learned: bool,
    /// Permits that got 206 and are streaming. Learned max = this count.
    streaming: usize,
    throttle_gen: u64,
}
/// Clean completions needed to regain one connection slot.
pub const GATE_REGAIN_STREAK: u32 = 4;
/// Permit for one in-flight GET. Drop releases.
pub struct GatePermit {
    gate: Arc<ConcurrencyGate>,
}
impl Drop for GatePermit {
    fn drop(&mut self) {
        let mut s = self.gate.lock();
        s.in_flight = s.in_flight.saturating_sub(1);
    }
}
impl ConcurrencyGate {
    pub fn new(max: usize) -> Self {
        let max = max.max(1);
        Self {
            inner: std::sync::Mutex::new(GateState {
                limit: max,
                in_flight: 0,
                clean_streak: 0,
                learned: false,
                streaming: 0,
                throttle_gen: 0,
            }),
            max,
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn limit(&self) -> usize {
        self.lock().limit
    }
    /// Try take a GET permit. None = at limit, caller waits and retries.
    pub fn try_acquire(self: &Arc<Self>) -> Option<GatePermit> {
        let mut s = self.lock();
        if s.in_flight < s.limit {
            s.in_flight += 1;
            Some(GatePermit { gate: self.clone() })
        } else {
            None
        }
    }
    /// Wait for permit, honoring pause/cancel. 150ms poll.
    pub async fn acquire(self: &Arc<Self>, ctrl: &Controls) -> Result<GatePermit, String> {
        loop {
            if let Some(p) = self.try_acquire() {
                return Ok(p);
            }
            if ctrl.cancelled.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            wait_not_paused(ctrl).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    /// Mark permit's GET as admitted (206). Release on permit drop.
    pub fn mark_streaming(&self) {
        self.lock().streaming += 1;
    }
    pub fn unmark_streaming(&self) {
        let mut s = self.lock();
        s.streaming = s.streaming.saturating_sub(1);
    }
    /// 429: learned max = permits actually streaming (206), not holders.
    /// First call flips learned; later calls only lower, never raise.
    pub fn on_throttled(&self) -> usize {
        let mut s = self.lock();
        s.limit = s.streaming.max(1).min(s.limit);
        s.learned = true;
        s.clean_streak = 0;
        s.throttle_gen += 1;
        s.limit
    }
    pub fn throttle_gen(&self) -> u64 {
        self.lock().throttle_gen
    }
    pub fn learned(&self) -> bool {
        self.lock().learned
    }
    /// Clean segment completion: GATE_REGAIN_STREAK-th one regains a slot,
    /// only when learned (pre-429 optimism already at max).
    pub fn on_clean(&self) {
        let mut s = self.lock();
        if !s.learned {
            return;
        }
        s.clean_streak += 1;
        if s.clean_streak >= GATE_REGAIN_STREAK {
            s.clean_streak = 0;
            s.limit = (s.limit + 1).min(self.max);
        }
    }
}

impl Controls {
    pub fn new(limit: u64) -> Self {
        Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            downloaded: AtomicU64::new(0),
            phase: AtomicU8::new(0),
            limit_kbps: AtomicU64::new(limit),
            rate: std::sync::Mutex::new((std::time::Instant::now(), 0, None, 0, 0.0)),
            total: AtomicU64::new(0),
            parts: std::sync::Mutex::new(Vec::new()),
            part_files: std::sync::Mutex::new(Vec::new()),
            desired_files: std::sync::Mutex::new(None),
            parts_are_segments: AtomicBool::new(false),
            torrent: std::sync::Mutex::new(None),
            live: std::sync::Mutex::new(None),
        }
    }
}

fn compute_rate(prev_bytes: u64, cur_bytes: u64, dt_secs: f64) -> f64 {
    if dt_secs <= 0.0 || cur_bytes <= prev_bytes {
        0.0
    } else {
        (cur_bytes - prev_bytes) as f64 / dt_secs
    }
}

/// Per-task smoothed rate (bytes/s) from `ctrl.downloaded`. First call only
/// sets baseline and returns 0. Tuple fields 0..=1 stay owned by `throttle`.
pub fn rate_bps(ctrl: &Controls) -> f64 {
    let cur = ctrl.downloaded.load(Ordering::Relaxed);
    let now = std::time::Instant::now();
    let mut g = ctrl.rate.lock().unwrap_or_else(|e| e.into_inner());
    let Some(prev_t) = g.2 else {
        g.2 = Some(now);
        g.3 = cur;
        return 0.0;
    };
    let dt = now.duration_since(prev_t).as_secs_f64();
    if dt <= 0.0 {
        return g.4;
    }
    let inst = compute_rate(g.3, cur, dt);
    g.4 = if g.4 == 0.0 {
        inst
    } else {
        0.25 * inst + 0.75 * g.4
    };
    g.2 = Some(now);
    g.3 = cur;
    g.4
}

async fn throttle(ctrl: &Controls, bytes: u64) {
    let limit = ctrl.limit_kbps.load(Ordering::Relaxed);
    if limit == 0 {
        return;
    }
    // Global 1s window gate shared across workers.
    loop {
        let wait = {
            let mut g = ctrl.rate.lock().unwrap_or_else(|e| e.into_inner());
            if g.0.elapsed() >= std::time::Duration::from_secs(1) {
                g.0 = std::time::Instant::now();
                g.1 = 0;
            }
            let budget = limit.saturating_mul(1024);
            if bytes > budget {
                if g.1 == 0 {
                    g.1 = bytes;
                    return;
                }
                std::time::Duration::from_secs(1).saturating_sub(g.0.elapsed())
            } else if g.1 + bytes <= budget {
                g.1 += bytes;
                return;
            } else {
                std::time::Duration::from_secs(1).saturating_sub(g.0.elapsed())
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// Next stream chunk, but wake every 200ms to honour cancel/pause while socket idle.
/// Without this, cancel only lands when next chunk arrives (stalled server = never).
async fn next_chunk<S>(stream: &mut S, ctrl: &Controls) -> Result<Option<S::Item>, String>
where
    S: futures_util::Stream + Unpin,
{
    loop {
        wait_not_paused(ctrl).await?;
        match tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await {
            Ok(v) => return Ok(v),
            Err(_) => {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
            }
        }
    }
}

pub async fn wait_not_paused(ctrl: &Controls) -> Result<(), String> {
    while ctrl.paused.load(Ordering::Relaxed) {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if ctrl.cancelled.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    Ok(())
}

/// Split total into segment ranges.
pub fn plan_segments(total: u64, wanted: u32) -> Vec<(u64, u64)> {
    if total == 0 {
        return vec![];
    }
    let n = (wanted.max(1) as u64).min(total.max(1));
    let chunk = total / n;
    let mut v = Vec::new();
    let mut s = 0;
    for i in 0..n {
        let e = if i == n - 1 { total - 1 } else { s + chunk - 1 };
        v.push((s, e));
        s = e + 1;
    }
    v
}

/// Split floor: both halves of a split must stay >= this. 256KB floor let
/// repeated splits shred a download into 30+ crawling parts; 4MB keeps any
/// stolen tail worth a fresh connection.
pub const MIN_SPLIT: u64 = 4 * 1024 * 1024;
/// New end for victim when stealing its tail: split remaining (from committed
/// `done`) in half. None when either half would be < MIN_SPLIT.
/// ponytail: keyed on committed done, not in-flight progress; in-flight bytes
/// past split point are discarded by victim (see run_segment) and re-downloaded
/// by thief. Upgrade: track per-segment in-flight offset if waste matters.
#[cfg(test)]
fn split_point(start: u64, end: u64, done: u64) -> Option<u64> {
    split_point_min(start, end, done, MIN_SPLIT)
}
fn split_point_min(start: u64, end: u64, done: u64, min_half: u64) -> Option<u64> {
    let cur = start.checked_add(done)?;
    if cur > end {
        return None;
    }
    let remaining = end - cur + 1;
    if remaining < 2 * min_half.max(1) {
        None
    } else {
        Some(cur + remaining / 2 - 1)
    }
}

/// Pool part sizing: parts are a percentage of file, never infinitesimal.
/// target = total/(pool * PARTS_PER_CONN), clamped [MIN_SPLIT, cap].
/// Explicit override (Settings Part size) replaces target outright, see
/// `target_part_size_with` — a ceiling here made 512 MB setting invisible
/// (6 GiB/32 conns-waves = 192 MiB < cap, and work-steal slack re-split).
pub const PARTS_PER_CONN: u64 = 4;
pub const MAX_PART: u64 = 64 * 1024 * 1024;
/// Auto cap ceiling for huge files (Settings override 0 = auto).
pub const MAX_PART_AUTO: u64 = 256 * 1024 * 1024;
/// Configured override ceiling (Settings slider max 1024 MiB).
pub const MAX_PART_LIMIT: u64 = 1024 * 1024 * 1024;

/// Auto part cap scales with file size: small/medium files keep 64 MiB
/// (parallelism + work-stealing), huge files grow to fewer, larger parts
/// so simultaneous finishes, checkpoints and assembly shrink.
/// ponytail: static size tiers; speed/RTT-adaptive sizing needs measured
/// throughput model — add when tiers prove wrong.
pub fn auto_max_part(total: u64) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    match total {
        0..=4294967296 => MAX_PART,                    // <= 4 GiB
        4294967297..=17179869184 => 128 * 1024 * 1024, // <= 16 GiB
        _ if total <= 64 * GIB => 192 * 1024 * 1024,   // <= 64 GiB
        _ => MAX_PART_AUTO,
    }
}

/// Effective cap: `override_bytes` > 0 = Settings manual cap, else auto.
pub fn effective_max_part(total: u64, override_bytes: u64) -> u64 {
    if override_bytes > 0 {
        override_bytes.clamp(MIN_SPLIT, MAX_PART_LIMIT)
    } else {
        auto_max_part(total)
    }
}

pub fn target_part_size(total: u64, pool: u32) -> u64 {
    target_part_size_with(total, pool, 0)
}

pub fn target_part_size_with(total: u64, pool: u32, override_bytes: u64) -> u64 {
    // Small files: floor relaxes to per-connection share so pool still
    // parallelizes (parts never fewer than pool on fresh plan).
    let floor = MIN_SPLIT.min(total / (pool.max(1) as u64).max(1));
    if override_bytes > 0 {
        // Explicit user part size = target, not ceiling: 512 MiB on a 6 GiB
        // file means ~512 MiB parts (~12), NOT total/(pool*4) under a cap.
        // At least `pool` parts so every connection still gets work.
        let cap = effective_max_part(total, override_bytes);
        let per_conn = (total / (pool.max(1) as u64).max(1)).max(floor);
        return cap.min(per_conn);
    }
    let waves = (pool.max(1) as u64) * PARTS_PER_CONN;
    (total / waves.max(1)).clamp(floor, effective_max_part(total, override_bytes))
}
/// Plan with pool-relative part size (fresh downloads only; resume keeps
/// saved segmentation so part files stay valid).
pub fn plan_parts(total: u64, pool: u32) -> Vec<(u64, u64)> {
    plan_parts_with(total, pool, 0)
}

pub fn plan_parts_with(total: u64, pool: u32, override_bytes: u64) -> Vec<(u64, u64)> {
    if total == 0 {
        return vec![];
    }
    let target = target_part_size_with(total, pool, override_bytes);
    let n = total.div_ceil(target).max(1);
    plan_segments(total, n as u32)
}
/// Hard cap on non-merged segment count: pool*K waves plus slack so part
/// size (not count) is binding on huge files.
pub fn parts_cap(total: u64, pool: u32) -> usize {
    parts_cap_with(total, pool, 0)
}

pub fn parts_cap_with(total: u64, pool: u32, override_bytes: u64) -> usize {
    let cap = effective_max_part(total, override_bytes);
    let by_size = total.div_ceil(cap).max(1) as usize;
    if override_bytes > 0 {
        // Explicit size: no wave slack, else work-stealing splits the
        // user's parts back down (512 MiB plan -> 32 live segments).
        return by_size;
    }
    let by_waves = (pool.max(1) as usize) * (PARTS_PER_CONN as usize);
    by_size.max(by_waves)
}

/// Bytes of `written` (streamed for range starting at `range_start`) that
/// survive a shrink of the segment end to `end`. Rest was live-counted in
/// `ctrl.downloaded` but is discarded and re-downloaded by the thief.
fn seg_keep(end: u64, range_start: u64, written: u64) -> u64 {
    (end + 1).saturating_sub(range_start).min(written)
}

/// Merge consecutive pending segments (done==0, not active, contiguous)
/// into single ranges. Returns index remap old->surviving index (None for
/// removed slots). Callers must re-pin active indexes and rename part files.
/// Active/started segments untouched: their part files hold bytes.
/// ponytail: done==0 only; done>0 pending kept separate (part bytes exist).
fn coalesce_pending(segs: &mut [SegmentState], active: &std::collections::HashSet<usize>) -> bool {
    let mut changed = false;
    let mut survivor: Option<usize> = None;
    for i in 0..segs.len() {
        if segs[i].merged {
            continue;
        }
        let pending = segs[i].done == 0 && !active.contains(&i);
        if pending {
            if let Some(sv) = survivor {
                if segs[sv].end + 1 == segs[i].start {
                    segs[sv].end = segs[i].end;
                    segs[i].merged = true;
                    changed = true;
                    continue;
                }
            }
            survivor = Some(i);
        } else {
            survivor = None;
        }
    }
    changed
}

/// Deterministic spread of Retry-After wake times: workers must not wake
/// in lockstep and re-burst the rate limiter.
fn throttle_jitter(gen: u64, idx: usize) -> u64 {
    (gen.wrapping_mul(31).wrapping_add(idx as u64 * 7) % 5) * 250
}

/// Undo live-count of discarded bytes: counter was charged all `written`,
/// only `keep` gets committed to segment `done`.
fn apply_shrink_correction(ctrl: &Controls, written: u64, keep: u64) {
    let over = written.saturating_sub(keep);
    if over > 0 {
        ctrl.downloaded.fetch_sub(over, Ordering::Relaxed);
    }
}

/// Committed bytes across segments. Invariant: equals bytes counted once in
/// `ctrl.downloaded` whenever no segment is mid-stream.
#[cfg(test)]
fn committed_total(segs: &[SegmentState]) -> u64 {
    segs.iter().map(|s| s.done).sum()
}

enum Claim {
    Pending(usize),
    Split(usize),
    None,
}

/// Pure claim policy (testable core of `claim_or_split`): prefer unclaimed
/// unfinished segment; else split largest remaining active segment, but only
/// while `segs.len() < cap` (worker-pool size fixed at download start).
/// At/over cap an idle worker gets None and exits — remaining workers finish
/// their segments. Legacy over-cap state still claims pending work, it just
#[cfg(test)]
/// never splits further.
fn try_claim_or_split(
    segs: &mut Vec<SegmentState>,
    active: &mut std::collections::HashSet<usize>,
    cap: usize,
) -> Claim {
    try_claim_or_split_t(segs, active, cap, MIN_SPLIT)
}
fn try_claim_or_split_t(
    segs: &mut Vec<SegmentState>,
    active: &mut std::collections::HashSet<usize>,
    cap: usize,
    target: u64,
) -> Claim {
    let remaining = |sg: &SegmentState| (sg.end + 1).saturating_sub(sg.start + sg.done);
    for (i, sg) in segs.iter().enumerate() {
        if !sg.merged && remaining(sg) > 0 && !active.contains(&i) {
            active.insert(i);
            return Claim::Pending(i);
        }
    }
    let live = segs.iter().filter(|s| !s.merged).count();
    if live >= cap {
        return Claim::None;
    }
    let mut victim: Option<(usize, u64)> = None;
    for (i, sg) in segs.iter().enumerate() {
        if active.contains(&i) {
            if let Some(np) = split_point_min(sg.start, sg.end, sg.done, target) {
                let tail = sg.end - np;
                if victim.is_none_or(|(_, b)| tail > b) {
                    victim = Some((i, tail));
                }
            }
        }
    }
    let Some((vi, _)) = victim else {
        return Claim::None;
    };
    let old_end = segs[vi].end;
    let Some(np) = split_point_min(segs[vi].start, old_end, segs[vi].done, target) else {
        return Claim::None;
    };
    segs[vi].end = np;
    let new_idx = segs.len();
    segs.push(SegmentState {
        start: np + 1,
        end: old_end,
        done: 0,
        merged: false,
    });
    active.insert(new_idx);
    Claim::Split(new_idx)
}

pub struct Probe {
    pub total: Option<u64>,
    pub accept_ranges: bool,
    /// `Some` only when Range probe itself got `200` (server ignored
    /// Range): full response body, already paid for. `download_http`
    /// streams it as plain download — never re-GET, never drain first.
    /// Never log/persist: `final_url` may carry signed tokens.
    pub body: Option<reqwest::Response>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub final_url: String,
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
}

/// Percent-decode to bytes (no '+' handling: CD filenames are not form-encoded).
/// Body-free probe result cached between Add/capture confirm and download
/// start. Holds headers only — plain downloads re-GET, segmented downloads
/// reuse only segment plan (segments validate via ETag in state).
/// Never persisted, never logged (may carry signed `final_url`).
#[derive(Clone)]
pub struct CachedProbe {
    pub url: String,
    pub final_url: String,
    pub total: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
    pub fetched_at: std::time::Instant,
}
impl CachedProbe {
    pub fn into_probe(self) -> Probe {
        Probe {
            total: self.total,
            accept_ranges: self.accept_ranges,
            body: None,
            etag: self.etag,
            last_modified: self.last_modified,
            final_url: self.final_url,
            content_disposition: self.content_disposition,
            content_type: self.content_type,
        }
    }
}
pub const PREWARM_TTL: std::time::Duration = std::time::Duration::from_secs(120);
pub fn prewarm_fresh(fetched_at: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(fetched_at) < PREWARM_TTL
}
// ponytail: process-global single-use registry keyed by original URL;
// runtime-only (never state.json). Upgrade: per-task-id keys if
// same-URL-different-headers tasks collide in practice.
static PREWARM: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, CachedProbe>>,
> = std::sync::OnceLock::new();
fn prewarm_registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, CachedProbe>> {
    PREWARM.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
pub fn prewarm_store(cache: CachedProbe) {
    let mut g = prewarm_registry().lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    g.retain(|_, v| prewarm_fresh(v.fetched_at, now));
    g.insert(cache.url.clone(), cache);
}
pub fn prewarm_drop(url: &str) {
    prewarm_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(url.trim());
}
pub fn prewarm_take(url: &str) -> Option<CachedProbe> {
    prewarm_take_at(url, std::time::Instant::now())
}
fn prewarm_take_at(url: &str, now: std::time::Instant) -> Option<CachedProbe> {
    let mut g = prewarm_registry().lock().unwrap_or_else(|e| e.into_inner());
    let cache = g.remove(url.trim())?;
    if cache.url == url.trim() && prewarm_fresh(cache.fetched_at, now) {
        Some(cache)
    } else {
        None
    }
}

fn pct_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Sanitize a server-supplied filename: strip path separators + control
/// chars, reject empty/dot-only results. Reuses model::sanitize_filename
/// (which already strips dirs and maps <>:"/\|?* and controls).
pub fn sanitize_cd_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base.chars().all(|c| c == '.') {
        return None;
    }
    let cleaned: String = base.chars().filter(|c| (*c as u32) >= 32).collect();
    if cleaned.trim().is_empty() {
        return None;
    }
    let s = crate::model::sanitize_filename(&cleaned);
    if s == "download.bin" && !cleaned.eq_ignore_ascii_case("download.bin") {
        None // sanitizer fallback means nothing usable survived
    } else {
        Some(s)
    }
}

/// Parse Content-Disposition filename. RFC 5987 `filename*` (charset'lang'%XX)
/// wins over plain `filename=`. Returns sanitized name.
pub fn parse_content_disposition(header: &str) -> Option<String> {
    // filename* first
    for part in header.split(';') {
        let part = part.trim();
        let Some(eq) = part.find('=') else { continue };
        if !part[..eq].trim().eq_ignore_ascii_case("filename*") {
            continue;
        }
        let v = part[eq + 1..].trim().trim_matches('"');
        // charset'lang'value
        let mut it = v.splitn(3, '\'');
        let charset = it.next().unwrap_or("utf-8").to_ascii_lowercase();
        let _lang = it.next();
        let Some(encoded) = it.next() else { continue };
        let bytes = pct_decode_bytes(encoded);
        let text = if charset.contains("utf-8") || charset.contains("utf8") {
            String::from_utf8_lossy(&bytes).into_owned()
        } else if charset.contains("iso-8859-1") || charset.contains("latin1") {
            bytes.iter().map(|&b| b as char).collect()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        if let Some(s) = sanitize_cd_filename(&text) {
            return Some(s);
        }
    }
    for part in header.split(';') {
        let part = part.trim();
        let Some(eq) = part.find('=') else { continue };
        if !part[..eq].trim().eq_ignore_ascii_case("filename") {
            continue;
        }
        let raw = part[eq + 1..].trim();
        let unq = if raw.len() >= 2 && raw.starts_with('"') {
            // Quoted-string: unescape \" and \\, stop at closing quote.
            let mut out = String::new();
            let mut chars = raw[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' => break,
                    _ => out.push(c),
                }
            }
            out
        } else {
            raw.trim_matches('"').to_string()
        };
        // Plain filename= may still be percent-encoded by broken servers.
        let decoded = String::from_utf8_lossy(&pct_decode_bytes(&unq)).into_owned();
        if let Some(s) = sanitize_cd_filename(&decoded) {
            return Some(s);
        }
    }
    None
}

/// Generic placeholder names eligible for server-name adoption. Protects
/// user-renamed tasks: capture dialog sets filename via sanitize_filename,
/// so any specific name (with extension, not in this set) is left alone.
pub fn is_generic_filename(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() {
        return true;
    }
    let stem = n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n);
    stem.eq_ignore_ascii_case("download")
        || stem.eq_ignore_ascii_case("index")
        || Path::new(n).extension().is_none()
}

fn mime_ext(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match ct.as_str() {
        "application/zip" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/pdf" => "pdf",
        "application/octet-stream" => "bin",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "text/plain" => "txt",
        "application/json" => "json",
        "application/x-iso9660-image" => "iso",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" => "msi",
        "application/vnd.android.package-archive" => "apk",
        _ => return None,
    })
}

/// Adopt server filename when current name is generic. CD name wins; else
/// append MIME extension when current name lacks one.
pub fn adopt_server_filename(
    current: &str,
    content_disposition: Option<&str>,
    content_type: Option<&str>,
) -> Option<String> {
    if !is_generic_filename(current) {
        return None;
    }
    if let Some(cd) = content_disposition.and_then(parse_content_disposition) {
        if !cd.eq_ignore_ascii_case(current) {
            return Some(cd);
        }
    }
    if Path::new(current).extension().is_none() {
        if let Some(ext) = content_type.and_then(mime_ext) {
            let stem = if current.trim().is_empty() {
                "download"
            } else {
                current.trim()
            };
            return Some(format!("{stem}.{ext}"));
        }
    }
    None
}

/// Same-destination guard for final rename: dest exists with different
/// size than incoming file -> pick `name (1).ext`, `name (2).ext`, ...
/// Same size (likely own redownload) or absent dest: keep dest (rename replaces).
pub fn dedup_dest(dest: &Path, incoming_size: Option<u64>) -> PathBuf {
    let occupied_diff = |p: &Path| -> bool {
        match (std::fs::metadata(p).map(|m| m.len()).ok(), incoming_size) {
            (Some(existing), Some(inc)) => existing != inc,
            (Some(_), None) => true,
            _ => false,
        }
    };
    if !occupied_diff(dest) {
        return dest.to_path_buf();
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    let ext = dest.extension().map(|s| s.to_string_lossy().into_owned());
    let parent = dest.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for n in 1..1000 {
        let cand = match &ext {
            Some(e) => parent.join(format!("{stem} ({n}).{e}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        if !occupied_diff(&cand) && !cand.exists() {
            return cand;
        }
    }
    dest.to_path_buf()
}

/// Prewarm: probe now (dialog open) so confirm -> first byte skips
/// probe round-trip. Result has no body: response dropped after headers.
pub async fn prewarm(client: &reqwest::Client, task: &DownloadTask) -> Result<CachedProbe, String> {
    let mut p = probe(client, task).await?;
    let _ = p.body.take();
    Ok(CachedProbe {
        url: task.url.clone(),
        final_url: p.final_url,
        total: p.total,
        accept_ranges: p.accept_ranges,
        etag: p.etag,
        last_modified: p.last_modified,
        content_disposition: p.content_disposition,
        content_type: p.content_type,
        fetched_at: std::time::Instant::now(),
    })
}

pub async fn probe(client: &reqwest::Client, task: &DownloadTask) -> Result<Probe, String> {
    let resp = client
        .get(&task.url)
        .headers(headers_for(task))
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let lm = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let final_url = resp.url().to_string();
    let content_disposition = resp
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if status.as_u16() == 206 {
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let total = cr.rsplit('/').next().and_then(|v| v.parse().ok());
        // 1-byte probe body: drop without draining (connection discarded, waste = 1 byte).
        drop(resp);
        Ok(Probe {
            total,
            accept_ranges: true,
            body: None,
            etag,
            last_modified: lm,
            final_url,
            content_disposition: content_disposition.clone(),
            content_type: content_type.clone(),
        })
    } else if status.is_success() {
        // Server ignored Range: 200 body IS whole file. Hand live response to
        // plain path in download_http — never drain here, never re-GET.
        let total = resp.content_length();
        Ok(Probe {
            total,
            accept_ranges: false,
            body: Some(resp),
            etag,
            last_modified: lm,
            final_url,
            content_disposition,
            content_type,
        })
    } else {
        Err(format!("HTTP {status}"))
    }
}

struct SegmentState {
    start: u64,
    end: u64,
    done: u64,
    /// Coalesced into previous pending segment after throttle-learn:
    /// slot kept (stable indexes/part names), skipped everywhere.
    merged: bool,
}

fn seg_clone(x: &SegmentState) -> SegmentState {
    SegmentState {
        start: x.start,
        end: x.end,
        done: x.done,
        merged: x.merged,
    }
}
fn state_path_for(part_dir: &Path) -> PathBuf {
    part_dir.join("state.json")
}
/// Serializes concurrent save_state writers (8 workers checkpointing same
/// file raced non-atomically -> torn JSON -> whole resume discarded).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn save_state(
    part_dir: &Path,
    url: &str,
    etag: &Option<String>,
    total: u64,
    segs: &[SegmentState],
) {
    let v: Vec<_> = segs
        .iter()
        .filter(|s| !s.merged)
        .map(|s| serde_json::json!({"start":s.start,"end":s.end,"done":s.done}))
        .collect();
    let j = serde_json::json!({"url":url,"etag":etag,"total":total,"segments":v});
    // Atomic: tmp + rename, serialized. Torn state.json = resume lost.
    let _g = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dst = state_path_for(part_dir);
    let tmp = dst.with_extension("json.tmp");
    if std::fs::write(&tmp, j.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &dst);
    }
}

/// Segmented HTTP download with resume. Segment files: part{i} in dest_dir/.downloadcraft-{id}/
pub async fn download_http(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<Controls>,
) -> Result<DownloadTask, String> {
    download_http_with_prewarm(client, task, ctrl, None, 0).await
}

// `max_part`: max part bytes override; 0 = auto (see `effective_max_part`).
pub async fn download_http_with_prewarm(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
    prewarm: Option<CachedProbe>,
    max_part: u64,
) -> Result<DownloadTask, String> {
    // Mirror failover (IDM Url0..UrlN): first URL whose probe succeeds wins; task.url rewritten to it.
    let mut urls = vec![task.url.clone()];
    urls.extend(task.mirrors.iter().cloned());
    let mut found: Option<(String, Probe)> = None;
    let mut last_err = String::new();
    let mut prewarm = prewarm
        .filter(|c| c.url == task.url && prewarm_fresh(c.fetched_at, std::time::Instant::now()));
    for u in urls {
        let cand = DownloadTask {
            url: u.clone(),
            ..task.clone()
        };
        let hit = if u == task.url {
            prewarm.take().map(|c| c.into_probe())
        } else {
            None
        };
        match match hit {
            Some(p) => Ok(p),
            None => probe(&client, &cand).await,
        } {
            Ok(p) => {
                found = Some((u, p));
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some((used_url, mut probe)) = found else {
        return Err(format!("all mirrors failed (last: {last_err})"));
    };
    // Probe resolved redirects: plain/segment requests go to final_url,
    // not through redirect chain again. Internal only — never logged,
    // task.url returned to callers stays used mirror URL.
    let fetch_url = if probe.final_url.is_empty() {
        used_url.clone()
    } else {
        probe.final_url.clone()
    };
    task.url = used_url;
    task.total = probe.total;
    task.accept_ranges = Some(probe.accept_ranges);
    if let Some(ct) = probe.content_type.as_deref().filter(|s| !s.is_empty()) {
        task.mime = ct.to_string();
    }
    // Filename repair: adopt CD/MIME name only for generic placeholders,
    // never overriding user-renamed (specific) filenames.
    if let Some(name) = adopt_server_filename(
        &task.filename,
        probe.content_disposition.as_deref(),
        probe.content_type.as_deref(),
    ) {
        task.filename = name;
    }
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part_dir = task.dest_dir.join(format!(".downloadcraft-{}", task.id));
    {
        if let Ok(mut l) = ctrl.live.lock() {
            let e = l.get_or_insert_with(Default::default);
            if e.final_url.is_empty() {
                e.final_url = fetch_url.clone();
            }
        }
    }
    // Server ignored Range: plain streaming download with throttle.
    if !probe.accept_ranges || probe.total.is_none() {
        // 429 on probe/download: bounded Retry-After waits, then resume logic
        // above re-probes (not an error attempt storm).
        let mut resp = match probe.body.take() {
            Some(r) => r,
            None => {
                let mut waits = 0u32;
                loop {
                    let r = client
                        .get(&fetch_url)
                        .headers(headers_for(&task))
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    if r.status().as_u16() == 429 && waits < 12 {
                        waits += 1;
                        wait_backoff(&ctrl, retry_after_secs(r.headers())).await?;
                        continue;
                    }
                    break r;
                }
            }
        };
        if resp.status().as_u16() == 429 {
            // Probe body itself throttled: wait once, refetch plain.
            let ra = retry_after_secs(resp.headers());
            wait_backoff(&ctrl, ra).await?;
            resp = client
                .get(&fetch_url)
                .headers(headers_for(&task))
                .send()
                .await
                .map_err(|e| e.to_string())?;
        }
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        task.total = resp.content_length().or(task.total);
        if let Some(t) = task.total {
            ctrl.total.store(t, Ordering::Relaxed);
            ctrl.parts_are_segments.store(false, Ordering::Relaxed);
            *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(0, t)];
        }
        // Plain path: response headers carry CD/MIME too.
        if let Some(name) = adopt_server_filename(
            &task.filename,
            resp.headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
        ) {
            task.filename = name;
        }
        let dest = task.dest_dir.join(&task.filename);
        let tmp = task.dest_dir.join(format!(".partial-{}", task.filename));
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        let mut n: u64 = 0;
        loop {
            let Some(chunk) = next_chunk(&mut stream, &ctrl).await? else {
                break;
            };
            let chunk = chunk.map_err(|e| e.to_string())?;
            throttle(&ctrl, chunk.len() as u64).await;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            n += chunk.len() as u64;
            ctrl.downloaded.store(n, Ordering::Relaxed);
            task.downloaded = n;
            if let Some(t) = task.total {
                if let Some(part) = ctrl
                    .parts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .first_mut()
                {
                    part.0 = n.min(t);
                }
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        // Validate temp BEFORE dedup/rename: on failure delete temp only —
        // final dest (pre-existing/other-task file) stays untouched.
        let vdest = match validate_output(&tmp, &task.filename) {
            Ok(_) => {
                let dest = dedup_dest(&dest, Some(n));
                if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    if name != task.filename {
                        task.filename = name;
                    }
                }
                tokio::fs::rename(&tmp, &dest)
                    .await
                    .map_err(|e| e.to_string())?;
                dest
            }
            Err(ValidateFail::TsInMp4) => {
                // Remux temp in place, then dedup/rename to final dest.
                let fixed = ensure_mp4_or_ts_validated(&tmp).await?;
                let dest = dedup_dest(&dest, Some(n));
                let dest = if fixed.extension().and_then(|e| e.to_str()) == Some("ts") {
                    dest.with_extension("ts")
                } else {
                    dest
                };
                if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    if name != task.filename {
                        task.filename = name;
                    }
                }
                tokio::fs::rename(&fixed, &dest)
                    .await
                    .map_err(|e| e.to_string())?;
                dest
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(format!("invalid output: {e}"));
            }
        };
        // vdest already validated pre-rename (TsInMp4 remuxed there); this
        // gate only re-checks the renamed file, no quarantine (dest is ours).
        match validate_output(&vdest, &task.filename) {
            Ok(_) => {}
            Err(ValidateFail::TsInMp4) => {
                let final_path = ensure_mp4_or_ts(&vdest).await?;
                if let Some(name) = final_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                {
                    task.filename = name;
                }
                finish_check(&final_path, &task).await?;
                task.status = TaskStatus::Complete;
                task.downloaded = n;
                return Ok(task);
            }
            Err(e) => return Err(quarantine_bad(&dest, &e)),
        }
        finish_check(&dest, &task).await?;
        task.status = TaskStatus::Complete;
        task.downloaded = n;
        return Ok(task);
    }
    let total = probe.total.unwrap();
    task.total = Some(total);
    std::fs::create_dir_all(&part_dir).map_err(|e| e.to_string())?;
    // Resume state validation
    let mut segs: Vec<SegmentState> = Vec::new();
    let mut resumed: u64 = 0;
    let state_file = state_path_for(&part_dir);
    if state_file.exists() {
        if let Ok(txt) = std::fs::read_to_string(&state_file) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let etag_ok = v
                    .get("etag")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
                    == probe.etag
                    || probe.etag.is_none();
                if (v.get("url").and_then(|u| u.as_str()) == Some(fetch_url.as_str())
                    || v.get("url").and_then(|u| u.as_str()) == Some(task.url.as_str()))
                    && v.get("total").and_then(|t| t.as_u64()) == Some(total)
                    && etag_ok
                {
                    if let Some(arr) = v.get("segments").and_then(|s| s.as_array()) {
                        let mut loaded: Vec<SegmentState> = Vec::new();
                        for s in arr {
                            let (start, end, done) = (
                                s["start"].as_u64().unwrap_or(0),
                                s["end"].as_u64().unwrap_or(0),
                                s["done"].as_u64().unwrap_or(0),
                            );
                            // verify part file size matches done (index = position in saved array)
                            let pf = part_dir.join(format!("part{}", loaded.len()));
                            let actual = std::fs::metadata(&pf).map(|m| m.len()).unwrap_or(0);
                            loaded.push(SegmentState {
                                start,
                                end,
                                done: done.min(actual),
                                merged: false,
                            });
                        }
                        // Split tails appended out of order: intervals must tile [0,total) exactly, else discard corrupt state.
                        let mut sorted: Vec<&SegmentState> = loaded.iter().collect();
                        sorted.sort_by_key(|s| s.start);
                        let mut expect = 0u64;
                        let mut ok = total > 0;
                        for s in &sorted {
                            if s.start != expect || s.end < s.start || s.done > s.end - s.start + 1
                            {
                                ok = false;
                                break;
                            }
                            expect = s.end + 1;
                        }
                        if ok && expect == total {
                            resumed = loaded.iter().map(|s| s.done).sum();
                            segs = loaded;
                        }
                    }
                }
            }
        }
    }
    let pool: u32 = if task.segments == 0 {
        crate::model::Settings::default().max_connections
    } else {
        task.segments
    };
    if segs.is_empty() {
        segs = plan_parts_with(total, pool, max_part)
            .into_iter()
            .map(|(start, end)| SegmentState {
                start,
                end,
                done: 0,
                merged: false,
            })
            .collect();
    }
    let target = target_part_size_with(total, pool, max_part);
    let pcap = parts_cap_with(total, pool, max_part);
    {
        if let Ok(mut l) = ctrl.live.lock() {
            let e = l.get_or_insert_with(Default::default);
            e.final_url = fetch_url.clone();
        }
    }
    task.final_url = fetch_url.clone();
    ctrl.downloaded.store(resumed, Ordering::Relaxed);
    task.downloaded = resumed;
    ctrl.total.store(total, Ordering::Relaxed);
    ctrl.parts_are_segments.store(false, Ordering::Relaxed);
    {
        let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
        *pg = segs
            .iter()
            .map(|x| {
                if x.merged {
                    (
                        x.end.saturating_add(1).saturating_sub(x.start),
                        x.end.saturating_add(1).saturating_sub(x.start),
                    )
                } else {
                    (x.done, x.end.saturating_add(1).saturating_sub(x.start))
                }
            })
            .collect();
    }
    // Work-stealing pool: fixed worker count (= initial segment count) is
    // also the hard cap on total segments — idle workers may split only
    // below it, so parts never multiply past the connection count.
    let etag: Arc<str> = probe.etag.clone().unwrap_or_default().into();
    let segs = Arc::new(tokio::sync::Mutex::new(segs));
    let active = Arc::new(tokio::sync::Mutex::new(
        std::collections::HashSet::<usize>::new(),
    ));
    // Segment requests fetch final_url; task.url returned stays mirror URL.
    let mut fetch_task = task.clone();
    fetch_task.url = fetch_url.clone();
    // Adaptive gate: starts at user max, drops a slot per 429 observed,
    // regains slowly. Workers gate on live limit, not spawn count.
    let gate = Arc::new(ConcurrencyGate::new(pool.max(1) as usize));
    let mut handles = Vec::new();
    // Pool workers == user pool size; wire concurrency = gate permits.
    for _ in 0..(pool.max(1) as usize) {
        let (client, task_c, ctrl, segs, active, part_dir, etag) = (
            client.clone(),
            fetch_task.clone(),
            ctrl.clone(),
            segs.clone(),
            active.clone(),
            part_dir.clone(),
            etag.clone(),
        );
        let gate = gate.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let Some(i) = claim_or_split(
                    &segs,
                    &active,
                    if gate.learned() { 0 } else { pcap },
                    &part_dir,
                    &task_c.url,
                    &etag,
                    total,
                    &ctrl,
                    target,
                )
                .await
                else {
                    return Ok::<(), String>(());
                };
                // Concurrency enforced per-GET inside run_segment via
                // gate permits; claim cap only bounds split growth.
                let r = run_segment(
                    i, &segs, &client, &task_c, &part_dir, &etag, total, &ctrl, &gate, &active,
                )
                .await;
                active.lock().await.remove(&i);
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err("cancelled".to_string());
                }
                r?;
            }
        }));
    }
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if ctrl.cancelled.load(Ordering::Relaxed) || e == "cancelled" => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(e.to_string()),
        }
    }
    if ctrl.cancelled.load(Ordering::Relaxed) {
        task.status = TaskStatus::Paused;
        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
        return Ok(task);
    }
    ctrl.phase.store(PHASE_ASSEMBLING, Ordering::Relaxed);
    // Assemble in byte order: split tails appended (index != byte order).
    let order: Vec<usize> = {
        let s = segs.lock().await;
        let mut v: Vec<usize> = (0..s.len()).filter(|&i| !s[i].merged).collect();
        v.sort_by_key(|&i| s[i].start);
        v
    };
    let tmp = task.dest_dir.join(format!(".assemble-{}", task.filename));
    {
        let mut out = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        for i in order {
            let meta_len = tokio::fs::metadata(part_dir.join(format!("part{i}")))
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            // Part may hold discarded in-flight bytes past a split point; copy only full segment length clamped to file size.
            let want = {
                let s = segs.lock().await;
                let sg = &s[i];
                (sg.end - sg.start + 1).min(meta_len)
            };
            let f = tokio::fs::File::open(part_dir.join(format!("part{i}")))
                .await
                .map_err(|e| e.to_string())?;
            tokio::io::copy(&mut f.take(want), &mut out)
                .await
                .map_err(|e| e.to_string())?;
        }
        out.flush().await.map_err(|e| e.to_string())?;
    }
    let assembled = tokio::fs::metadata(&tmp)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if assembled != total {
        return Err(format!(
            "size mismatch: assembled {assembled} != total {total}"
        ));
    }
    ctrl.phase.store(PHASE_FINALIZING, Ordering::Relaxed);
    // Validate assembled temp BEFORE dedup/rename: on failure delete temp
    // only — final dest (pre-existing/other-task file) stays untouched.
    match validate_output(&tmp, &task.filename) {
        Ok(_) => {}
        Err(ValidateFail::TsInMp4) => {
            let fixed = ensure_mp4_or_ts_validated(&tmp).await?;
            let is_ts = fixed.extension().and_then(|e| e.to_str()) == Some("ts");
            let dest0 = dedup_dest(&dest, Some(total));
            let dest = if is_ts {
                dest0.with_extension("ts")
            } else {
                dest0
            };
            if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
                if name != task.filename {
                    task.filename = name;
                }
            }
            tokio::fs::rename(&fixed, &dest)
                .await
                .map_err(|e| e.to_string())?;
            let _ = tokio::fs::remove_dir_all(&part_dir).await;
            finish_check(&dest, &task).await?;
            task.status = TaskStatus::Complete;
            task.downloaded = total;
            return Ok(task);
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            let _ = tokio::fs::remove_dir_all(&part_dir).await;
            return Err(format!("invalid output: {e}"));
        }
    }
    let dest = dedup_dest(&dest, Some(total));
    if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
        if name != task.filename {
            task.filename = name;
        }
    }
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| e.to_string())?;
    let _ = tokio::fs::remove_dir_all(&part_dir).await;
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete;
    task.downloaded = total;
    task.final_url = fetch_url.clone();
    Ok(task)
}

/// Claim work: prefer unclaimed unfinished segment; else (only while
/// `segs.len() < cap`) split largest remaining active segment and claim new
/// tail. None when no claimable work for this worker.
#[allow(clippy::too_many_arguments)] // flat list mirrors call sites; grouping churn not worth it
async fn claim_or_split(
    segs: &Arc<tokio::sync::Mutex<Vec<SegmentState>>>,
    active: &Arc<tokio::sync::Mutex<std::collections::HashSet<usize>>>,
    cap: usize,
    part_dir: &Path,
    url: &str,
    etag: &Arc<str>,
    total: u64,
    ctrl: &Controls,
    target: u64,
) -> Option<usize> {
    let mut s = segs.lock().await;
    let mut act = active.lock().await;
    let new_idx = match try_claim_or_split_t(&mut s, &mut act, cap, target) {
        Claim::Pending(i) => return Some(i),
        Claim::Split(i) => i,
        Claim::None => return None,
    };
    let snapshot: Vec<SegmentState> = s.iter().map(seg_clone).collect();
    {
        let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
        *pg = snapshot
            .iter()
            .map(|x| {
                if x.merged {
                    (
                        x.end.saturating_add(1).saturating_sub(x.start),
                        x.end.saturating_add(1).saturating_sub(x.start),
                    )
                } else {
                    (x.done, x.end.saturating_add(1).saturating_sub(x.start))
                }
            })
            .collect();
    }
    drop(act);
    drop(s);
    let et_opt = if etag.is_empty() {
        None
    } else {
        Some(etag.to_string())
    };
    save_state(part_dir, url, &et_opt, total, &snapshot);
    Some(new_idx)
}

/// Download one claimed segment index until its (possibly shrunk) end.
/// On split-shrink: only bytes up to current end counted/committed; tail
/// bytes in part file discarded at assemble, re-downloaded by thief.
#[allow(clippy::too_many_arguments)]
async fn run_segment(
    i: usize,
    segs: &Arc<tokio::sync::Mutex<Vec<SegmentState>>>,
    client: &reqwest::Client,
    task: &DownloadTask,
    part_dir: &Path,
    etag: &Arc<str>,
    total: u64,
    ctrl: &Arc<Controls>,
    gate: &Arc<ConcurrencyGate>,
    active: &Arc<tokio::sync::Mutex<std::collections::HashSet<usize>>>,
) -> Result<(), String> {
    let mut attempt = 0;
    // Fresh-plan save: state.json exists before first byte, so Stop before
    // any segment completes still resumes (was: file written only on split).
    {
        let s = segs.lock().await;
        save_state(
            part_dir,
            &task.url,
            &if etag.is_empty() {
                None
            } else {
                Some(etag.to_string())
            },
            total,
            &s,
        );
    }
    #[allow(unused_assignments)]
    let mut last_err = String::new();
    loop {
        let (start, end, done) = {
            let s = segs.lock().await;
            let sg = &s[i];
            (sg.start, sg.end, sg.done)
        };
        if done > end - start {
            return Ok(());
        }
        let range_start = start + done;
        if range_start > end {
            return Ok(());
        }
        // Checkpoint: persist byte progress + live counters per ~1MiB.
        let ck_segs = segs.clone();
        let ck_part_dir = part_dir.to_path_buf();
        let ck_url = task.url.clone();
        let ck_etag = if etag.is_empty() {
            None
        } else {
            Some(etag.to_string())
        };
        let ck_ctrl = ctrl.clone();
        let checkpoint = move |abs_done: u64| {
            if let Ok(mut s) = ck_segs.try_lock() {
                let new_len = s[i].end + 1 - s[i].start;
                s[i].done = abs_done.min(new_len);
                let snap: Vec<SegmentState> = s.iter().map(seg_clone).collect();
                drop(s);
                if let Ok(mut pg) = ck_ctrl.parts.lock() {
                    *pg = snap
                        .iter()
                        .map(|x| {
                            if x.merged {
                                (
                                    x.end.saturating_add(1).saturating_sub(x.start),
                                    x.end.saturating_add(1).saturating_sub(x.start),
                                )
                            } else {
                                (x.done, x.end.saturating_add(1).saturating_sub(x.start))
                            }
                        })
                        .collect();
                }
                save_state(&ck_part_dir, &ck_url, &ck_etag, total, &snap);
            }
        };
        // Permit BEFORE every GET incl. 429 retries: this is the actual
        // concurrency enforcement (claim-time rank check raced).
        let _permit = gate.acquire(ctrl).await?;
        match download_range(
            client,
            task,
            range_start,
            end,
            part_dir.join(format!("part{i}")).as_path(),
            ctrl,
            done,
            Some(i),
            Some(&checkpoint),
            Some(gate),
        )
        .await
        {
            Ok(RangeOutcome::Throttled {
                written,
                retry_after,
            }) => {
                if written > 0 {
                    // Commit checkpointed bytes like a partial success.
                    let keep = {
                        let s = segs.lock().await;
                        seg_keep(s[i].end, start + done, written)
                    };
                    {
                        let mut s = segs.lock().await;
                        let new_len = s[i].end + 1 - s[i].start;
                        s[i].done = (s[i].done.min(new_len) + keep).min(new_len);
                    }
                    apply_shrink_correction(ctrl, written, keep);
                }
                drop(_permit);
                // First throttle: collapse queued pieces into big ranges
                // (stable indexes; merged slots skipped everywhere).
                {
                    let mut s = segs.lock().await;
                    let act = active.lock().await;
                    if coalesce_pending(&mut s, &act) {
                        let snap: Vec<SegmentState> = s.iter().map(seg_clone).collect();
                        drop(act);
                        drop(s);
                        let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                        *pg = snap
                            .iter()
                            .map(|x| {
                                if x.merged {
                                    (
                                        x.end.saturating_add(1).saturating_sub(x.start),
                                        x.end.saturating_add(1).saturating_sub(x.start),
                                    )
                                } else {
                                    (x.done, x.end.saturating_add(1).saturating_sub(x.start))
                                }
                            })
                            .collect();
                        drop(pg);
                        save_state(
                            part_dir,
                            &task.url,
                            &if etag.is_empty() {
                                None
                            } else {
                                Some(etag.to_string())
                            },
                            total,
                            &snap,
                        );
                    }
                }
                if gate.throttle_gen() > 24 {
                    return Err("server keeps throttling (429) after 24 waits".into());
                }
                let extra = throttle_jitter(gate.throttle_gen(), i);
                wait_backoff(ctrl, retry_after + extra / 1000).await?;
                tokio::time::sleep(Duration::from_millis(extra % 1000)).await;
                continue;
            }
            Ok(RangeOutcome::Done(written)) => {
                let keep = {
                    let s = segs.lock().await;
                    let sg = &s[i];
                    // Victim end may have shrunk mid-stream: clamp committed
                    // done to new length, discard count for tail bytes.
                    seg_keep(sg.end, start + done, written)
                };
                {
                    let mut s = segs.lock().await;
                    let new_len = s[i].end + 1 - s[i].start;
                    s[i].done = (s[i].done.min(new_len) + keep).min(new_len);
                }
                apply_shrink_correction(ctrl, written, keep);
                let snapshot = {
                    let s = segs.lock().await;
                    s.iter().map(seg_clone).collect::<Vec<_>>()
                };
                {
                    let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                    *pg = snapshot
                        .iter()
                        .map(|x| {
                            if x.merged {
                                (
                                    x.end.saturating_add(1).saturating_sub(x.start),
                                    x.end.saturating_add(1).saturating_sub(x.start),
                                )
                            } else {
                                (x.done, x.end.saturating_add(1).saturating_sub(x.start))
                            }
                        })
                        .collect();
                }
                let et_opt = if etag.is_empty() {
                    None
                } else {
                    Some(etag.to_string())
                };
                save_state(part_dir, &task.url, &et_opt, total, &snapshot);
                if keep < written {
                    continue;
                }
                gate.on_clean();
                return Ok(());
            }
            Err(e) => {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err(e);
                }
                last_err = e.clone();
                attempt += 1;
                if attempt > 5 {
                    return Err(format!(
                        "segment download failed after retries (last: {last_err})"
                    ));
                }
                drop(_permit);
                tokio::time::sleep(std::time::Duration::from_millis(
                    500 * (1 << attempt.min(4)),
                ))
                .await;
            }
        }
    }
}

/// Parse `Retry-After` (delta-seconds only; HTTP-date ignored). Default 5s,
/// clamp 1..=60s.
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(5)
        .clamp(1, 60)
}

/// Sleep honoring pause/cancel (200ms poll like body reads).
async fn wait_backoff(ctrl: &Controls, secs: u64) -> Result<(), String> {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        wait_not_paused(ctrl).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// Bytes since last persisted checkpoint; save_state every this many.
pub const CHECKPOINT_BYTES: u64 = 1024 * 1024;

#[allow(clippy::too_many_arguments)]
pub enum RangeOutcome {
    Done(u64),
    /// 429: bytes checkpointed this call; caller waits, re-ranges.
    Throttled {
        written: u64,
        retry_after: u64,
    },
}

#[allow(clippy::too_many_arguments)]
async fn download_range(
    client: &reqwest::Client,
    task: &DownloadTask,
    start: u64,
    end: u64,
    dest: &Path,
    ctrl: &Controls,
    prior: u64,
    part_index: Option<usize>,
    on_checkpoint: Option<&(dyn Fn(u64) + Send + Sync)>,
    gate: Option<&ConcurrencyGate>,
) -> Result<RangeOutcome, String> {
    let resp = client
        .get(&task.url)
        .headers(headers_for(task))
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() == 429 {
        let ra = retry_after_secs(resp.headers());
        if let Some(g) = gate {
            g.on_throttled();
        }
        return Ok(RangeOutcome::Throttled {
            written: 0,
            retry_after: ra,
        });
    }
    if resp.status().as_u16() != 206 {
        return Err(format!("expected 206 got {}", resp.status()));
    }
    if let Some(g) = gate {
        g.mark_streaming();
    }
    // Unmark on every exit below via scope guard (fn has multiple returns).
    struct StreamMark<'a>(Option<&'a ConcurrencyGate>);
    impl Drop for StreamMark<'_> {
        fn drop(&mut self) {
            if let Some(g) = self.0 {
                g.unmark_streaming();
            }
        }
    }
    let _stream_mark = StreamMark(gate);
    // Truncate to caller-known done: drops bytes from interrupted attempt.
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dest)
        .await
        .map_err(|e| e.to_string())?;
    file.set_len(prior).await.map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(prior))
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream();
    let mut n: u64 = 0;
    let mut err: Option<String> = None;
    loop {
        let chunk = match next_chunk(&mut stream, ctrl).await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        match chunk {
            Ok(chunk) => {
                throttle(ctrl, chunk.len() as u64).await;
                match file.write_all(&chunk).await {
                    Ok(()) => {
                        n += chunk.len() as u64;
                        ctrl.downloaded
                            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        if let Some(pi) = part_index {
                            let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(part) = pg.get_mut(pi) {
                                part.0 = (prior + n).min(part.1);
                            }
                        }
                        if let Some(cb) = on_checkpoint {
                            if n % CHECKPOINT_BYTES < chunk.len() as u64 {
                                cb(prior + n);
                            }
                        }
                    }
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
            Err(e) => {
                err = Some(e.to_string());
                break;
            }
        }
    }
    if err.is_none() {
        if let Err(e) = file.flush().await {
            err = Some(e.to_string());
        }
    }
    // Live-counted bytes roll back on failure: retry truncates part to `prior` and recounts.
    if let Some(e) = err {
        ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
        if let Some(pi) = part_index {
            if let Some(part) = ctrl
                .parts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(pi)
            {
                part.0 = prior.min(part.1);
            }
        }
        return Err(e);
    }
    let expected = end - start + 1;
    if n != expected {
        return Err(format!("short range: got {n} expected {expected}"));
    }
    Ok(RangeOutcome::Done(n))
}

async fn finish_check(dest: &Path, task: &DownloadTask) -> Result<(), String> {
    if let Some(want) = &task.sha256 {
        let mut f = tokio::fs::File::open(dest)
            .await
            .map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let n = f.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(want) {
            // Checksum auto-repair: quarantine corrupt output as `<name>.bad`
            // (overwrite prior .bad) so retry re-downloads instead of
            // resuming/verifying same corrupt bytes. Error still propagates:
            // app retry policy requeues, parts already cleaned by caller.
            let bad = PathBuf::from(format!("{}.bad", dest.display()));
            let _ = tokio::fs::remove_file(&bad).await;
            let _ = tokio::fs::rename(dest, &bad).await;
            return Err(format!(
                "sha256 mismatch: got {got} want {want} (quarantined as {})",
                bad.display()
            ));
        }
    }
    Ok(())
}

pub type MediaTracks = Vec<(String, Vec<(Option<String>, String)>)>;
/// Download list of media segments (DASH/HLS) and concatenate. Separate tracks muxed via ffmpeg if available.
/// Prior-run segment lengths recorded in `tracks.json` (beside `seg-*.bin`
/// caches). Legacy snapshots stored only `(kind, count)` per track; those
/// parse to counts-only and yield no lengths (distrust cache, refetch).
/// Fresh snapshots append per-segment on-disk lengths as a third tuple
/// element, keyed `seg-{kind}-{i}`.
type SegLenSnapshot = std::collections::BTreeMap<String, u64>;

fn seg_len_key(kind: &str, i: usize) -> String {
    format!("seg-{kind}-{i}")
}

/// Load recorded segment length, if snapshot has one for this segment.
/// Missing entry / missing / unparsable snapshot = no recorded length.
fn recorded_seg_len(snapshot: &SegLenSnapshot, kind: &str, i: usize) -> Option<u64> {
    snapshot.get(&seg_len_key(kind, i)).copied()
}

/// Cache trustworthy only when it exists, is non-empty, and (when snapshot
/// recorded a length) matches it exactly. Length-less legacy cache = poison
/// risk (partial write from killed run), so caller refetches.
fn cache_trusted(cache: &Path, recorded: Option<u64>) -> bool {
    let Ok(md) = std::fs::metadata(cache) else {
        return false;
    };
    let len = md.len();
    if len == 0 {
        return false;
    }
    match recorded {
        Some(want) => len == want,
        None => false,
    }
}

/// Serialize tracks.json: `{"tracks": [[kind, count]], "seg_lens":
/// [[seg_key, len]]}`. `parse_seg_snapshot` also accepts legacy bare
/// `[[kind, count]]` (no lengths -> caches distrusted).
fn snapshot_json(tracks: &MediaTracks, lens: &SegLenSnapshot) -> String {
    let tracks_v: Vec<serde_json::Value> = tracks
        .iter()
        .map(|(k, s)| serde_json::json!([k, s.len()]))
        .collect();
    let lens_v: Vec<serde_json::Value> = lens
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();
    serde_json::json!({"tracks": tracks_v, "seg_lens": lens_v}).to_string()
}

/// Parse tracks.json in either form: legacy bare array `[[kind, count]]`
/// (no lengths) or object `{"tracks": [...], "seg_lens": [[key, len]]}`.
fn parse_seg_snapshot(text: &str) -> SegLenSnapshot {
    let mut out = SegLenSnapshot::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let Some(arr) = v.get("seg_lens").and_then(|x| x.as_array()) else {
        return out;
    };
    for e in arr {
        if let (Some(k), Some(n)) = (
            e.get(0).and_then(|x| x.as_str()),
            e.get(1).and_then(|x| x.as_u64()),
        ) {
            out.insert(k.to_string(), n);
        }
    }
    out
}

pub async fn download_media(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<Controls>,
    tracks: MediaTracks,
) -> Result<DownloadTask, String> {
    // tracks: (kind, [(init, segment_url)...]) — one segment list per track, init repeated per entry ignored except first
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let mut task = task;
    let work = task
        .dest_dir
        .join(format!(".downloadcraft-media-{}", task.id));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    // Manifest snapshot BEFORE fetch: kill-resume reuses seg caches below.
    // Record prior completed segment lengths first (cache-size validation);
    // snapshot rewritten with fresh lengths after each segment completes.
    let mut known_lens: SegLenSnapshot = std::fs::read_to_string(work.join("tracks.json"))
        .map(|t| parse_seg_snapshot(&t))
        .unwrap_or_default();
    let _ = std::fs::write(
        work.join("tracks.json"),
        snapshot_json(&tracks, &known_lens),
    );
    let mut track_files: Vec<PathBuf> = Vec::new();
    let mut subtitle_files: Vec<PathBuf> = Vec::new();
    // Segment sizes unknown upfront: progress in bytes, task.total stays None.
    let mut done_bytes: u64 = 0;
    task.total = None;
    task.downloaded = 0;
    ctrl.downloaded.store(0, Ordering::Relaxed);
    {
        let total_segs: u64 = tracks.iter().map(|(_, sg)| sg.len() as u64).sum();
        ctrl.parts_are_segments.store(true, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(0, total_segs)];
    }
    for (kind, segs) in tracks.iter() {
        let out = work.join(format!("track-{kind}.bin"));
        {
            let mut file = tokio::fs::File::create(&out)
                .await
                .map_err(|e| e.to_string())?;
            let mut seen_init: Option<String> = None;
            for (i, (init, seg_url)) in segs.iter().enumerate() {
                wait_not_paused(&ctrl).await?;
                // Kill-resume: cached segment file from prior run is replayed, not refetched.
                let cache = work.join(format!("seg-{kind}-{i}.bin"));
                let seg_result = async {
                    if let Some(init) = init {
                        if seen_init.as_deref() != Some(init.as_str()) {
                            seen_init = Some(init.clone());
                            done_bytes +=
                                fetch_into_inner(&client, &task, init, &mut file, &ctrl).await?;
                        }
                    }
                    // Replay only size-validated cache: missing/empty/length-
                    // mismatch (partial write from killed run) refetches instead
                    // of poisoning output. No recorded length = untrusted.
                    let recorded = recorded_seg_len(&known_lens, kind, i);
                    if !cache_trusted(&cache, recorded) {
                        let mut cf = tokio::fs::File::create(&cache)
                            .await
                            .map_err(|e| e.to_string())?;
                        // Streaming fetch counts live progress; undo that count here —
                        // append below counts segment exactly once (fetch or replay).
                        let n = fetch_into_inner(&client, &task, seg_url, &mut cf, &ctrl).await?;
                        ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
                        cf.flush().await.map_err(|e| e.to_string())?;
                        drop(cf);
                        // Record fresh length for next kill-resume validation.
                        if n > 0 {
                            known_lens.insert(seg_len_key(kind, i), n);
                            let _ = std::fs::write(
                                work.join("tracks.json"),
                                snapshot_json(&tracks, &known_lens),
                            );
                        }
                    }
                    let bytes = tokio::fs::read(&cache).await.map_err(|e| e.to_string())?;
                    if bytes.is_empty() {
                        return Err(format!("empty segment fetched: {seg_url}"));
                    }
                    done_bytes += bytes.len() as u64;
                    ctrl.downloaded
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    file.write_all(&bytes).await.map_err(|e| e.to_string())?;
                    Ok::<(), String>(())
                }
                .await;
                if seg_result.is_ok() {
                    let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(first) = pg.first_mut() {
                        first.0 += 1;
                    }
                }
                if let Err(e) = seg_result {
                    if ctrl.cancelled.load(Ordering::Relaxed) || e == "cancelled" {
                        task.status = TaskStatus::Paused;
                        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
                        return Ok(task);
                    }
                    return Err(e);
                }
                task.downloaded = done_bytes;
                task.total = None;
            }
            file.flush().await.map_err(|e| e.to_string())?;
        }
        if kind == "text" {
            subtitle_files.push(out);
        } else {
            track_files.push(out);
        }
    }
    // Subtitle tracks land as sidecar next to final file (ffmpeg stream mapping for
    // arbitrary subtitle segment bytes is unreliable; sidecar is what players pick up).
    let incoming = track_files
        .first()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    let dest = dedup_dest(&task.dest_dir.join(&task.filename), incoming);
    if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
        if name != task.filename {
            task.filename = name;
        }
    }
    let text_url_ext = tracks
        .iter()
        .find(|(k, _)| k == "text")
        .and_then(|(_, s)| s.first())
        .map(|(_, u)| {
            u.rsplit('.')
                .next()
                .unwrap_or("vtt")
                .split(['?', '#'])
                .next()
                .unwrap_or("vtt")
                .to_string()
        });
    for (i, sub) in subtitle_files.iter().enumerate() {
        let stem = task
            .filename
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(&task.filename);
        let ext = text_url_ext.clone().unwrap_or_else(|| "vtt".into());
        let suffix = if subtitle_files.len() > 1 {
            format!(".{i}")
        } else {
            String::new()
        };
        let _ = tokio::fs::copy(sub, task.dest_dir.join(format!("{stem}{suffix}.{ext}"))).await;
    }
    if track_files.is_empty() && !subtitle_files.is_empty() {
        // Subtitle-only download: sidecar IS the product.
        tokio::fs::copy(&subtitle_files[0], &dest)
            .await
            .map_err(|e| e.to_string())?;
    } else if track_files.len() == 1 {
        // Single track: raw concat renamed dest may be TS bytes under .mp4
        // name (unplayable). Remux to mp4 or rename .ts before Complete.
        match validate_output(&track_files[0], &task.filename) {
            Err(ValidateFail::TsInMp4) => {
                // Remux track file in place; if ffmpeg absent it becomes .ts.
                let fixed = ensure_mp4_or_ts(&track_files[0]).await?;
                let is_ts = fixed.extension().and_then(|e| e.to_str()) == Some("ts");
                let target = if is_ts {
                    dest.with_extension("ts")
                } else {
                    dest.clone()
                };
                tokio::fs::rename(&fixed, &target)
                    .await
                    .map_err(|e| e.to_string())?;
                if let Some(name) = target.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    task.filename = name;
                }
                let _ = tokio::fs::remove_dir_all(&work).await;
                finish_check(&target, &task).await?;
                task.status = TaskStatus::Complete;
                task.downloaded = done_bytes;
                task.total = None;
                return Ok(task);
            }
            Err(e) => {
                // Assembled segments (raw concat, pre-remux): Empty/Playlist
                // hard-fail; generic magic mismatch tolerated (fMP4 concat
                // not standalone container, per-segment guard already ran).
                if matches!(e, ValidateFail::Empty | ValidateFail::Playlist) {
                    let _ = tokio::fs::remove_dir_all(&work).await;
                    task.status = TaskStatus::Error;
                    task.error = format!("invalid output: {e}");
                    return Ok(task);
                }
                tokio::fs::rename(&track_files[0], &dest)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(_) => {
                tokio::fs::rename(&track_files[0], &dest)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    } else {
        // Try ffmpeg mux; fallback: keep first track and note error (muxer missing handled by caller docs)
        match mux_tracks(&track_files, &dest).await {
            Ok(()) => {}
            Err(e) => {
                // Fallback: rename video/first track so file exists, record error.
                // TS bytes must not stay under .mp4 name: rename dest to .ts.
                tokio::fs::copy(&track_files[0], &dest)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = tokio::fs::remove_dir_all(&work).await;
                if matches!(
                    validate_output(&dest, &task.filename),
                    Err(ValidateFail::TsInMp4)
                ) {
                    let ts_dest = dest.with_extension("ts");
                    if tokio::fs::rename(&dest, &ts_dest).await.is_ok() {
                        if let Some(name) = ts_dest
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                        {
                            task.filename = name;
                        }
                    }
                }
                task.status = TaskStatus::Error;
                task.error = format!("mux failed ({e}); kept first track only");
                task.downloaded = done_bytes;
                task.total = None;
                return Ok(task);
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    // Final gate: muxed/multi-track output must not be manifest/empty/wrong magic.
    // Subtitle-only dest is text sidecar copy: skip media magic there.
    if track_files.is_empty() && !subtitle_files.is_empty() {
        // subtitle-only: only empty/missing check applies (strict would
        // magic-reject text; non-media validate still errors Empty).
        if matches!(validate_output(&dest, "x.bin"), Err(ValidateFail::Empty)) {
            task.status = TaskStatus::Error;
            task.error = "invalid output: subtitle file empty/missing".into();
            return Ok(task);
        }
    } else if track_files.len() == 1 {
        // Single-track raw concat already gated tolerantly above; reaching
        // here means it passed/renamed. Only Empty/Playlist hard-fail.
        if matches!(
            validate_output(&dest, &task.filename),
            Err(ValidateFail::Empty | ValidateFail::Playlist)
        ) {
            task.status = TaskStatus::Error;
            task.error = "invalid output: empty/playlist".into();
            return Ok(task);
        }
    } else {
        // Post-mux final gate (strict): ANY validate error hard-fails —
        // ffmpeg/mux output corrupt under any extension.
        match validate_output_strict(&dest, &task.filename, true) {
            Ok(_) => {}
            Err(ValidateFail::TsInMp4) => {
                let fixed = ensure_mp4_or_ts(&dest).await?;
                if let Some(name) = fixed.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    task.filename = name;
                }
                let final_dest = task.dest_dir.join(&task.filename);
                finish_check(&final_dest, &task).await?;
                task.status = TaskStatus::Complete;
                task.downloaded = done_bytes;
                task.total = None;
                return Ok(task);
            }
            Err(e) => {
                task.status = TaskStatus::Error;
                task.error = format!("invalid output: {e}");
                return Ok(task);
            }
        }
    }
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete;
    task.downloaded = done_bytes;
    task.total = None;
    Ok(task)
}

async fn fetch_into_inner(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    file: &mut tokio::fs::File,
    ctrl: &Controls,
) -> Result<u64, String> {
    let mut attempt = 0;
    let start = file
        .seek(std::io::SeekFrom::Current(0))
        .await
        .map_err(|e| e.to_string())?;
    loop {
        match fetch_once_counted_impl(client, task, url, file, ctrl).await {
            Ok(n) => return Ok(n),
            Err(e) => {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err(e);
                }
                attempt += 1;
                if attempt > 4 {
                    return Err(e);
                }
                // Roll back partial bytes so retry does not duplicate/count twice.
                file.set_len(start).await.map_err(|x| x.to_string())?;
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|x| x.to_string())?;
                tokio::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
            }
        }
    }
}
async fn fetch_once_counted_impl(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    file: &mut tokio::fs::File,
    ctrl: &Controls,
) -> Result<u64, String> {
    let req = client.get(url).headers(headers_for(task));
    // byte-range segment stored in headers via special marker? HLS BYTERANGE passed through task headers Range by caller when needed.
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    let mut stream = resp.bytes_stream();
    let mut n = 0u64;
    // First-chunk guard: manifest/HTML bytes must never become media segments.
    let mut first = true;
    loop {
        let Some(chunk) = (match next_chunk(&mut stream, ctrl).await {
            Ok(v) => v,
            Err(e) => {
                ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
                return Err(e);
            }
        }) else {
            break;
        };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
                return Err(e.to_string());
            }
        };
        if first {
            first = false;
            if crate::stream::looks_like_manifest_bytes_pub(&chunk) {
                return Err(format!("segment is manifest/HTML, not media: {url}"));
            }
        }
        throttle(ctrl, chunk.len() as u64).await;
        if let Err(e) = file.write_all(&chunk).await {
            ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
            return Err(e.to_string());
        }
        n += chunk.len() as u64;
        ctrl.downloaded
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    Ok(n)
}

/// Fetch segment honoring optional byte range (HLS EXT-X-BYTERANGE).
pub async fn fetch_bytes(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    range: Option<(u64, Option<u64>)>,
    dest: &Path,
) -> Result<(), String> {
    fetch_bytes_counted(client, task, url, range, dest)
        .await
        .map(|_| ())
}
pub async fn fetch_bytes_counted(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    range: Option<(u64, Option<u64>)>,
    dest: &Path,
) -> Result<u64, String> {
    let mut req = client.get(url).headers(headers_for(task));
    if let Some((len, off)) = range {
        let start = off.unwrap_or(0);
        // len==0 (hostile/garbled HLS byterange) underflows start+len-1: debug panic, release wrap.
        let end = start
            .checked_add(len)
            .and_then(|e| e.checked_sub(1))
            .ok_or("invalid byte range length 0")?;
        req = req.header("Range", format!("bytes={start}-{end}"));
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if crate::stream::looks_like_manifest_bytes_pub(&bytes) {
        return Err(format!("segment is manifest/HTML, not media: {url}"));
    }
    let n = bytes.len() as u64;
    tokio::fs::write(dest, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    Ok(n)
}

/// Output validation failure. `TsInMp4`: dest name is mp4-family but bytes
/// are MPEG-TS (first byte 0x47) — caller remuxes or renames to .ts.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateFail {
    Msg(String),
    TsInMp4,
    Empty,
    Playlist,
}
impl std::fmt::Display for ValidateFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidateFail::Msg(m) => write!(f, "{m}"),
            ValidateFail::TsInMp4 => write!(f, "output is TS, not mp4"),
            ValidateFail::Empty => write!(f, "output empty/missing"),
            ValidateFail::Playlist => write!(f, "output is playlist/HTML, not media"),
        }
    }
}

/// TS stream: 188-byte packets each start 0x47. Single leading 0x47 is not
/// enough: 'G' (GIF, text) also equals 0x47. Require a full packet plus a
/// sync byte at the next packet boundary when present.
pub fn is_ts(bytes: &[u8]) -> bool {
    bytes.len() >= 188
        && bytes[0] == 0x47
        && (bytes.len() == 188 || bytes[188] == 0x47)
        && (bytes.len() < 377 || bytes[376] == 0x47)
}

fn is_mp4_ext(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "mov" | "m4a")
}

#[derive(Debug, PartialEq)]
enum Mp4Scan {
    HasMedia,
    NoMedia,
    MoofWithoutMoov,
}

/// Walk top-level MP4 boxes in `head`. HasMedia: mdat seen, or moof seen
/// after moov (fMP4). MoofWithoutMoov: fragment init (moov) missing.
fn mp4_scan(head: &[u8]) -> Mp4Scan {
    let mut off = 0usize;
    let mut seen_moov = false;
    while off + 8 <= head.len() {
        let size32 = u32::from_be_bytes(head[off..off + 4].try_into().unwrap());
        let typ = &head[off + 4..off + 8];
        let (hdr, box_size): (usize, u64) = match size32 {
            1 => {
                if off + 16 > head.len() {
                    break;
                }
                (
                    16,
                    u64::from_be_bytes(head[off + 8..off + 16].try_into().unwrap()),
                )
            }
            0 => (8, (head.len() - off) as u64), // extends to EOF
            s => (8, s as u64),
        };
        if box_size < hdr as u64 {
            break;
        }
        match typ {
            b"mdat" => return Mp4Scan::HasMedia,
            b"moof" => {
                return if seen_moov {
                    Mp4Scan::HasMedia
                } else {
                    Mp4Scan::MoofWithoutMoov
                }
            }
            b"moov" => seen_moov = true,
            _ => {}
        }
        if box_size > (head.len() - off) as u64 {
            break; // box payload extends beyond scanned buffer
        }
        off += box_size as usize;
    }
    Mp4Scan::NoMedia
}

#[cfg(test)]
fn mp4_has_media(head: &[u8]) -> bool {
    mp4_scan(head) == Mp4Scan::HasMedia
}

const MEDIA_EXTS: &[&str] = &[
    "mp4", "m4a", "m4v", "mov", "mkv", "webm", "mp3", "ts", "m4s", "opus", "ogg", "flac", "wav",
    "aac",
];

fn is_media_ext(ext: &str) -> bool {
    MEDIA_EXTS.contains(&ext)
}

fn ext_of_str(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Validate finished output. Strict checks (empty, playlist/HTML body,
/// magic) ONLY for media-family dest (mp4,m4a,mov,mkv,webm,mp3,ts,m4s,
/// opus,ogg,flac,wav,aac) by dest or context filename extension. Generic
/// downloads always pass (Ok(len), missing file still errors) — legit
/// 0-byte/.html/legacy-.mov/ADTS-.aac files must not false-reject.
/// Pure: never renames. Returns file size.
pub fn validate_output(dest: &Path, filename_for_error_context: &str) -> Result<u64, ValidateFail> {
    validate_output_impl(dest, filename_for_error_context, false)
}

/// `strict=true`: force media checks regardless of extension. Post-mux
/// gate uses this — ffmpeg output under any name must be valid media,
/// any validate error there is a hard failure (corrupt output).
pub fn validate_output_strict(
    dest: &Path,
    filename_for_error_context: &str,
    strict: bool,
) -> Result<u64, ValidateFail> {
    if strict {
        validate_output_impl(dest, filename_for_error_context, true)
    } else {
        validate_output(dest, filename_for_error_context)
    }
}

fn validate_output_impl(
    dest: &Path,
    filename_for_error_context: &str,
    force_media: bool,
) -> Result<u64, ValidateFail> {
    let msg = |m: &str| ValidateFail::Msg(format!("{m} ({filename_for_error_context})"));
    // First 64KB is plenty for magic; whole-file read would be wasteful on
    // multi-GB outputs, size comes from metadata.
    use std::io::Read as _;
    let size = std::fs::metadata(dest)
        .map_err(|_| ValidateFail::Empty)?
        .len();
    let ctx_ext = ext_of_str(filename_for_error_context);
    let dest_ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !force_media && !is_media_ext(&ctx_ext) && !is_media_ext(&dest_ext) {
        return Ok(size);
    }
    if size == 0 {
        return Err(ValidateFail::Empty);
    }
    let mut head = vec![0u8; 64 * 1024];
    let n = {
        let mut f = std::fs::File::open(dest)
            .map_err(|e| ValidateFail::Msg(format!("output empty/missing: {e}")))?;
        f.read(&mut head)
            .map_err(|e| ValidateFail::Msg(format!("output read: {e}")))?
    };
    head.truncate(n);
    let head = &head[..];
    let text = {
        let t = String::from_utf8_lossy(head);
        let t = t.trim_start_matches('\u{feff}');
        t.trim_start().to_string()
    };
    if text.starts_with("#EXTM3U")
        || text.starts_with("<MPD")
        || text.to_ascii_lowercase().starts_with("<metalink")
        || text.to_ascii_lowercase().starts_with("<!doctype html")
        || text.to_ascii_lowercase().starts_with("<html")
    {
        return Err(ValidateFail::Playlist);
    }
    // Extension from filename context first: media track temp files are
    // `.bin` on disk but must validate against final dest extension.
    let ext = Path::new(filename_for_error_context)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| {
            dest.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
        });
    // TS-in-mp4 checked before generic magic: TS bytes are valid media,
    // just wrong container name — caller decides remux vs .ts rename.
    if is_mp4_ext(&ext) && is_ts(head) {
        return Err(ValidateFail::TsInMp4);
    }
    let ok = match ext.as_str() {
        // mov legacy: permissive (no ftyp, e.g. legacy QuickTime). mp4/m4v:
        // ftyp OR styp start; styp files still subject to box walk below.
        // m4a excluded: ADTS-.m4a has no ftyp.
        "mp4" | "m4v" => head.len() >= 8 && (&head[4..8] == b"ftyp" || &head[4..8] == b"styp"),
        "webm" | "mkv" => head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        "mp3" => {
            head.starts_with(b"ID3")
                || (head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0)
        }
        "ts" => is_ts(head),
        _ => true,
    };
    // Strict (post-mux): unknown/generic extension has no magic rule to
    // pass — corrupt ffmpeg output must not slip through as `_ => true`.
    let ok = ok && !(force_media && !is_media_ext(&ext));
    if !ok {
        return Err(msg("output magic bytes do not match extension"));
    }
    // mp4/m4v/mov box-walk when head starts ftyp OR styp (fragmented files
    // start with styp, no ftyp). Legacy mov without ftyp/styp keeps passing
    // above (no walk). Require mdat, or moof after moov (fMP4). Scan cap
    // 4MB: no mdat/moof within 4MB of larger file accepted (media may
    // start later); fully-scanned small file without media = fragment.
    if matches!(ext.as_str(), "mp4" | "m4v" | "mov")
        && head.len() >= 8
        && (&head[4..8] == b"ftyp" || &head[4..8] == b"styp")
    {
        const SCAN_CAP: u64 = 4 * 1024 * 1024;
        let mut f = std::fs::File::open(dest).map_err(|e| msg(&format!("output read: {e}")))?;
        let mut buf = vec![0u8; SCAN_CAP.min(size.max(1)) as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            match f.read(&mut buf[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(m) => filled += m,
            }
        }
        buf.truncate(filled);
        match mp4_scan(&buf) {
            Mp4Scan::HasMedia => {}
            Mp4Scan::MoofWithoutMoov => {
                return Err(msg(
                    "mp4 moof without preceding moov — likely init segment fragment",
                ));
            }
            Mp4Scan::NoMedia if size <= SCAN_CAP => {
                return Err(msg(
                    "mp4 has no media data (mdat/moof missing) — likely init segment fragment",
                ));
            }
            Mp4Scan::NoMedia => {}
        }
    }
    Ok(size)
}

/// Remux TS-in-mp4 dest to real mp4 via ffmpeg; on failure/absent ffmpeg,
/// rename dest to .ts (honest extension). Returns final path. Only called
/// after validate_output returned TsInMp4 (or dest known TS).
pub async fn ensure_mp4_or_ts(dest: &Path) -> Result<PathBuf, String> {
    let tmp = dest.with_extension("remux.mp4");
    match mux_tracks(&[dest.to_path_buf()], &tmp).await {
        Ok(()) => {
            // Re-validate remux result; corrupt ffmpeg output falls back to .ts.
            if validate_output_strict(&tmp, "x.mp4", true).is_ok() {
                std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
                return Ok(dest.to_path_buf());
            }
            let _ = std::fs::remove_file(&tmp);
            let ts = dest.with_extension("ts");
            std::fs::rename(dest, &ts).map_err(|e| e.to_string())?;
            Ok(ts)
        }
        Err(_) => {
            let ts = dest.with_extension("ts");
            std::fs::rename(dest, &ts).map_err(|e| e.to_string())?;
            Ok(ts)
        }
    }
}

/// ensure_mp4_or_ts for temp files whose on-disk name lacks the final
/// extension (`.partial-x`, track `.bin`): validate/remux against the
/// final filename, then restore temp naming for the caller's rename.
/// Falls back to `.ts` rename (of the temp) when remux output invalid.
pub async fn ensure_mp4_or_ts_validated(tmp: &Path) -> Result<PathBuf, String> {
    // Give temp an .mp4 sibling name so ensure_mp4_or_ts logic applies.
    let probe = tmp.with_extension("mp4");
    std::fs::rename(tmp, &probe).map_err(|e| e.to_string())?;
    match ensure_mp4_or_ts(&probe).await {
        Ok(p) => Ok(p),
        Err(e) => Err(e),
    }
}

/// Quarantine bad output as `<name>.bad` then report Err. ponytail: never
/// delete a pre-existing `.bad` (may belong to another task) — first free
/// `.bad`, `.bad-1`, ... wins.
pub fn quarantine_bad(dest: &Path, why: &ValidateFail) -> String {
    let mut bad = PathBuf::from(format!("{}.bad", dest.display()));
    let mut n = 1u32;
    while bad.exists() {
        bad = PathBuf::from(format!("{}.bad-{n}", dest.display()));
        n += 1;
    }
    let _ = std::fs::rename(dest, &bad);
    format!("invalid output: {why} (quarantined as {})", bad.display())
}

pub async fn mux_tracks(tracks: &[PathBuf], dest: &Path) -> Result<(), String> {
    // ffmpeg sidecar: PATH lookup (bundled ffmpeg next to exe preferred)
    let mut candidates = vec![PathBuf::from("ffmpeg")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            candidates.insert(
                0,
                d.join(if cfg!(windows) {
                    "ffmpeg.exe"
                } else {
                    "ffmpeg"
                }),
            );
        }
    }
    let mut last_err = String::new();
    for cand in &candidates {
        let mut cmd = tokio::process::Command::new(cand);
        crate::platform::no_window_tokio(&mut cmd);
        cmd.arg("-y");
        for t in tracks {
            cmd.args(["-i"]).arg(t);
        }
        cmd.args(["-c", "copy"]);
        cmd.arg(dest);
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        match cmd.output().await {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => {
                last_err = String::from_utf8_lossy(&o.stderr)
                    .chars()
                    .take(300)
                    .collect()
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_headers(pairs: &[(&str, &str)]) -> DownloadTask {
        DownloadTask {
            headers: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn headers_for_custom_user_agent_and_bearer_suppresses_basic() {
        let mut t = task_with_headers(&[
            ("User-Agent", "MyAgent/1.0"),
            ("Authorization", "Bearer tok123"),
        ]);
        t.username = "u".into();
        t.password = "p".into();
        let h = headers_for(&t);
        assert_eq!(h["user-agent"], "MyAgent/1.0");
        assert_eq!(h["authorization"], "Bearer tok123");
    }

    #[test]
    fn headers_for_basic_added_without_authorization() {
        let mut t = task_with_headers(&[]);
        t.username = "u".into();
        t.password = "p".into();
        let h = headers_for(&t);
        // base64("u:p")
        assert_eq!(h["authorization"], "Basic dTpw");
    }

    #[test]
    fn headers_for_drops_invalid_and_pseudo() {
        let t = task_with_headers(&[
            ("Bad Name", "v"),
            ("X-Good", "ok"),
            ("X-Bad", "a\r\nb"),
            ("x-ytdlp", "1"),
            ("x-page-url", "https://ex.com/page"),
            ("x-page-title", "t"),
            ("x-nzb", "1"),
            ("x-yt-format", "18"),
        ]);
        let h = headers_for(&t);
        assert_eq!(h["x-good"], "ok");
        assert!(!h.contains_key("bad name"));
        assert!(!h.contains_key("x-bad"));
        for k in [
            "x-ytdlp",
            "x-page-url",
            "x-page-title",
            "x-nzb",
            "x-yt-format",
        ] {
            assert!(!h.contains_key(k), "pseudo-header leaked: {k}");
        }
    }

    #[test]
    fn headers_for_cookies_field_beats_cookie_row() {
        let mut t = task_with_headers(&[("Cookie", "row=1"), ("User-Agent", "UA/2")]);
        t.cookies = "field=1".into();
        let h = headers_for(&t);
        assert_eq!(h["cookie"], "field=1");
        assert_eq!(h["user-agent"], "UA/2");
    }

    #[test]
    fn headers_for_referrer_field_beats_referer_row() {
        let mut t = task_with_headers(&[("Referer", "https://row.example/")]);
        t.referrer = "https://field.example/".into();
        let h = headers_for(&t);
        assert_eq!(h["referer"], "https://field.example/");
    }

    #[test]
    fn headers_for_plain_rows_pass_through() {
        let mut t = task_with_headers(&[
            ("User-Agent", "UA/1"),
            ("Accept-Language", "en-US"),
            ("Authorization", "Bearer b"),
        ]);
        t.referrer = "https://ref.example/".into();
        t.cookies = "a=b".into();
        let h = headers_for(&t);
        assert_eq!(h["user-agent"], "UA/1");
        assert_eq!(h["accept-language"], "en-US");
        assert_eq!(h["authorization"], "Bearer b");
        assert_eq!(h["referer"], "https://ref.example/");
        assert_eq!(h["cookie"], "a=b");
    }
    fn tmp_file(tag: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wc-validate-{}-{}", std::process::id(), tag));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
    #[test]
    fn mp4_box_walk() {
        fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(typ);
            v.extend_from_slice(payload);
            v
        }
        let ftyp = bx(b"ftyp", b"isom");
        let moov = bx(b"moov", b"meta");
        let mdat = bx(b"mdat", b"data");
        let moof = bx(b"moof", b"frag");
        let styp = bx(b"styp", b"msdh");
        let cat = |parts: &[&Vec<u8>]| {
            parts
                .iter()
                .flat_map(|p| p.iter().copied())
                .collect::<Vec<u8>>()
        };
        // init segment only (ftyp+moov): no media -> reject
        assert!(!mp4_has_media(&cat(&[&ftyp, &moov])));
        assert!(!mp4_has_media(&cat(&[&ftyp])));
        // progressive moov+mdat, and mdat-first (non-faststart): ok
        assert!(mp4_has_media(&cat(&[&ftyp, &moov, &mdat])));
        assert!(mp4_has_media(&cat(&[&ftyp, &mdat])));
        assert!(mp4_has_media(&cat(&[&ftyp, &mdat, &moov])));
        // fMP4: moov before moof ok; moof without preceding moov reject
        assert!(mp4_has_media(&cat(&[&ftyp, &moov, &moof, &mdat])));
        assert!(!mp4_has_media(&cat(&[&styp, &moof, &mdat])));
        assert!(!mp4_has_media(&cat(&[&ftyp, &moof])));
        // largesize (size==1) and to-EOF (size==0) boxes walk correctly
        let mut large_mdat = 1u32.to_be_bytes().to_vec();
        large_mdat.extend_from_slice(b"mdat");
        large_mdat.extend_from_slice(&24u64.to_be_bytes());
        large_mdat.extend_from_slice(b"12345678");
        assert!(mp4_has_media(&cat(&[&ftyp, &moov, &large_mdat])));
        let mut eof_mdat = 0u32.to_be_bytes().to_vec();
        eof_mdat.extend_from_slice(b"mdat");
        eof_mdat.extend_from_slice(b"data");
        assert!(mp4_has_media(&cat(&[&ftyp, &eof_mdat])));
        // end-to-end via validate_output
        let p = tmp_file("bw1", "v.mp4", &cat(&[&ftyp, &moov]));
        assert!(
            matches!(validate_output(&p, "v.mp4"), Err(ValidateFail::Msg(m)) if m.contains("mdat/moof missing")),
            "init-only mp4 rejected"
        );
        let p = tmp_file("bw2", "v.mp4", &cat(&[&ftyp, &moov, &mdat]));
        assert!(validate_output(&p, "v.mp4").is_ok());
        let p = tmp_file("bw3", "v.mp4", &cat(&[&ftyp, &mdat]));
        assert!(validate_output(&p, "v.mp4").is_ok());
        let p = tmp_file("bw4", "v.mp4", &cat(&[&styp, &moof]));
        assert!(
            matches!(validate_output(&p, "v.mp4"), Err(ValidateFail::Msg(m)) if m.contains("moof without preceding moov")),
            "styp+moof rejected via walk (moof without moov)"
        );
        let p = tmp_file("bw5", "v.mp4", &cat(&[&ftyp, &moof, &mdat]));
        assert!(
            matches!(validate_output(&p, "v.mp4"), Err(ValidateFail::Msg(m)) if m.contains("moof without preceding moov")),
            "moof-without-moov rejected even with later mdat"
        );
        // styp fragmented start: walk decides, magic alone insufficient.
        let p = tmp_file("bw6", "v.mp4", &cat(&[&styp, &moof, &mdat]));
        assert!(
            matches!(validate_output(&p, "v.mp4"), Err(ValidateFail::Msg(m)) if m.contains("moof without preceding moov")),
            "mp4 styp+moof+mdat without moov rejected"
        );
        let p = tmp_file("bw7", "v.mp4", &cat(&[&styp, &moov, &moof, &mdat]));
        assert!(
            validate_output(&p, "v.mp4").is_ok(),
            "styp+moov+moof accepted"
        );
        let p = tmp_file("bw8", "v.mp4", &cat(&[&styp, &moov, &moof]));
        assert!(
            validate_output(&p, "v.mp4").is_ok(),
            "styp+moov+moof (no mdat) accepted"
        );
        // .mov: walk runs when ftyp/styp present; legacy no-ftyp/no-styp stays permissive.
        let p = tmp_file("bw9", "v.mov", &cat(&[&ftyp, &moof]));
        assert!(
            matches!(validate_output(&p, "v.mov"), Err(ValidateFail::Msg(m)) if m.contains("moof without preceding moov")),
            "mov ftyp+moof without moov rejected"
        );
        let p = tmp_file("bw10", "v.mov", &cat(&[&styp, &moov, &moof, &mdat]));
        assert!(
            validate_output(&p, "v.mov").is_ok(),
            "mov styp+moov+moof accepted"
        );
    }

    #[test]
    fn validate_output_magic_table() {
        // Real boxes: ftyp + moov + mdat (24-byte total also used sub-size below).
        let mut mp4 = Vec::new();
        for (typ, payload) in [
            (b"ftyp", &b"isom"[..]),
            (b"moov", &b"meta"[..]),
            (b"mdat", &[0u8; 16][..]),
        ] {
            mp4.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
            mp4.extend_from_slice(typ);
            mp4.extend_from_slice(payload);
        }
        let p = tmp_file("mp4", "a.mp4", &mp4);
        assert!(validate_output(&p, "a.mp4").is_ok(), "valid ftyp mp4 ok");
        let p = tmp_file("m3u", "b.mp4", b"#EXTM3U\n#EXT-X-VERSION:3\n");
        assert_eq!(
            validate_output(&p, "b.mp4"),
            Err(ValidateFail::Playlist),
            "playlist in mp4 err"
        );
        let mut ts = vec![0x47u8];
        ts.extend_from_slice(&[0u8; 187]);
        let p = tmp_file("ts", "c.mp4", &ts);
        assert_eq!(
            validate_output(&p, "c.mp4"),
            Err(ValidateFail::TsInMp4),
            "TS in mp4 sentinel"
        );
        let p = tmp_file("empty", "d.mp4", b"");
        assert_eq!(
            validate_output(&p, "d.mp4"),
            Err(ValidateFail::Empty),
            "empty err"
        );
        let p = tmp_file("webm", "e.webm", &[0x1A, 0x45, 0xDF, 0xA3, 1, 2]);
        assert!(validate_output(&p, "e.webm").is_ok(), "webm magic ok");
        let p = tmp_file("mp3", "f.mp3", b"ID3xxxx");
        assert!(validate_output(&p, "f.mp3").is_ok(), "mp3 ID3 ok");
        let p = tmp_file("badmp4", "g.mp4", b"not-media-at-all......");
        assert!(
            matches!(validate_output(&p, "g.mp4"), Err(ValidateFail::Msg(_))),
            "bad mp4 magic err"
        );
        let p = tmp_file("html", "h.mp4", b"<html><body>x</body></html>");
        assert_eq!(
            validate_output(&p, "h.mp4"),
            Err(ValidateFail::Playlist),
            "HTML err"
        );
    }
    #[test]
    fn validate_output_generic_passes() {
        // 0-byte generic file passes.
        let p = tmp_file("g0", "a.bin", b"");
        assert_eq!(validate_output(&p, "a.bin"), Ok(0));
        // .html file with HTML body passes (it IS the product).
        let p = tmp_file("g1", "page.html", b"<html><body>x</body></html>");
        assert!(validate_output(&p, "page.html").is_ok());
        // .txt with playlist-looking text passes.
        let p = tmp_file("g2", "list.txt", b"#EXTM3U\nfoo\n");
        assert!(validate_output(&p, "list.txt").is_ok());
        // Legacy .mov without ftyp passes (no magic rule for mov).
        let p = tmp_file("g3", "old.mov", b"legacy-quicktime-no-ftyp....");
        assert!(validate_output(&p, "old.mov").is_ok());
        // ADTS in .m4a passes (no ftyp).
        let p = tmp_file("g3b", "a.m4a", &[0xFF, 0xF1, 1, 2, 3, 4, 5, 6]);
        assert!(validate_output(&p, "a.m4a").is_ok());
        // ADTS .aac: media ext, no magic rule -> passes.
        let p = tmp_file("g4", "a.aac", &[0xFF, 0xF1, 1, 2, 3, 4]);
        assert!(validate_output(&p, "a.aac").is_ok());
        // Playlist intentionally downloaded as .m3u8 passes (not media ext).
        let p = tmp_file("g5", "pl.m3u8", b"#EXTM3U\n#EXT-X-VERSION:3\n");
        assert!(validate_output(&p, "pl.m3u8").is_ok());
        // Missing file still errors.
        let p = tmp_file("g6", "nope.bin", b"x");
        let _ = std::fs::remove_file(&p);
        assert!(validate_output(&p, "nope.bin").is_err());
    }
    #[test]
    fn validate_output_strict_forces_media() {
        let p = tmp_file("s0", "out.bin", b"not-media-at-all......");
        assert!(validate_output(&p, "out.bin").is_ok(), "generic passes");
        assert!(
            validate_output_strict(&p, "out.bin", true).is_err(),
            "strict rejects generic magic"
        );
        let p = tmp_file("s1", "pl.bin", b"#EXTM3U\nx\n");
        assert_eq!(
            validate_output_strict(&p, "pl.bin", true),
            Err(ValidateFail::Playlist)
        );
    }
    #[test]
    fn is_ts_needs_packet_sync() {
        // GIF starts 'G'=0x47 but is not TS.
        assert!(!is_ts(b"GIF89a................"));
        let mut gif = b"GIF89a".to_vec();
        gif.resize(200, 0);
        assert!(!is_ts(&gif), "GIF padded not TS");
        // Single full packet ok; two packets with sync ok; bad 2nd sync no.
        let mut one = vec![0x47u8];
        one.resize(188, 0);
        assert!(is_ts(&one));
        let mut two = one.clone();
        two.extend_from_slice(&one);
        assert!(is_ts(&two));
        two[188] = 0x00;
        assert!(!is_ts(&two));
    }
    #[test]
    fn quarantine_never_overwrites_existing_bad() {
        let p = tmp_file("q0", "x.mp4", b"new-bad");
        let prior = PathBuf::from(format!("{}.bad", p.display()));
        std::fs::write(&prior, b"prior-task-bad").unwrap();
        let msg = quarantine_bad(&p, &ValidateFail::Empty);
        assert_eq!(std::fs::read(&prior).unwrap(), b"prior-task-bad");
        assert!(msg.contains(".bad-1"), "msg: {msg}");
        assert!(!p.exists());
    }
    #[test]
    fn seg_snapshot_and_cache_trust() {
        // Legacy [[kind,count]] snapshot: parses, yields no lengths.
        let legacy = parse_seg_snapshot(r#"[["video",2],["audio",1]]"#);
        assert!(legacy.is_empty());
        assert_eq!(recorded_seg_len(&legacy, "video", 0), None);
        // Fresh object snapshot round-trips per-segment lengths.
        let tracks: MediaTracks = vec![
            (
                "video".into(),
                vec![(None, "s0".into()), (None, "s1".into())],
            ),
            ("audio".into(), vec![(None, "a0".into())]),
        ];
        let mut lens = SegLenSnapshot::new();
        lens.insert(seg_len_key("video", 0), 100);
        lens.insert(seg_len_key("video", 1), 50);
        let back = parse_seg_snapshot(&snapshot_json(&tracks, &lens));
        assert_eq!(recorded_seg_len(&back, "video", 0), Some(100));
        assert_eq!(recorded_seg_len(&back, "video", 1), Some(50));
        assert_eq!(recorded_seg_len(&back, "audio", 0), None);
        // Garbage snapshot = no lengths, never panics.
        assert!(parse_seg_snapshot("not json").is_empty());
        // Trust: exact recorded len ok; mismatch / empty / missing /
        // no-recorded-length all untrusted.
        let dir = std::env::temp_dir().join(format!("wc-segtrust-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let good = dir.join("seg-video-0.bin");
        std::fs::write(&good, vec![7u8; 100]).unwrap();
        assert!(cache_trusted(&good, Some(100)));
        assert!(!cache_trusted(&good, Some(99)), "partial cache rejected");
        assert!(!cache_trusted(&good, None), "legacy cache untrusted");
        let empty = dir.join("seg-video-1.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(!cache_trusted(&empty, Some(0)));
        assert!(!cache_trusted(&empty, None));
        assert!(!cache_trusted(&dir.join("nope.bin"), Some(10)));
    }
    #[test]
    fn plan_segments_unchanged() {
        assert_eq!(plan_segments(0, 8), vec![]);
        assert_eq!(plan_segments(10, 3), vec![(0, 2), (3, 5), (6, 9)]);
        assert_eq!(plan_segments(5, 8).len(), 5);
    }
    #[test]
    fn compute_rate_cases() {
        assert_eq!(compute_rate(0, 1000, 2.0), 500.0);
        assert_eq!(compute_rate(500, 1500, 0.5), 2000.0);
        assert_eq!(compute_rate(0, 1000, 0.0), 0.0, "zero dt");
        assert_eq!(compute_rate(0, 1000, -1.0), 0.0, "negative dt");
        assert_eq!(compute_rate(1000, 1000, 1.0), 0.0, "no progress");
        assert_eq!(compute_rate(1000, 500, 1.0), 0.0, "counter reset");
    }
    #[test]
    fn rate_bps_first_call_zero() {
        let c = Controls::new(0);
        c.downloaded.store(1234, Ordering::Relaxed);
        assert_eq!(rate_bps(&c), 0.0);
    }
    #[test]
    fn split_point_halves_remaining() {
        let mb = 1024 * 1024u64;
        assert_eq!(split_point(0, 16 * mb - 1, 0), Some(8 * mb - 1));
        assert_eq!(
            split_point(0, 2 * MIN_SPLIT - 2, 0),
            None,
            "remaining < 2*MIN_SPLIT never splits"
        );
        assert_eq!(
            split_point(0, 10 * mb - 1, 4 * mb),
            None,
            "done counts toward remaining"
        );
        assert_eq!(
            split_point(100, 100 + 16 * mb - 1, 8 * mb),
            Some(100 + 12 * mb - 1)
        );
        assert_eq!(split_point(0, 10, 20), None);
        // both halves >= MIN_SPLIT at exact threshold
        assert_eq!(split_point(0, 2 * MIN_SPLIT - 1, 0), Some(MIN_SPLIT - 1));
        // old 256KB-floor sizes no longer split
        assert_eq!(split_point(0, 1024 * 1024 - 1, 0), None);
    }

    fn mk_segs(ranges: &[(u64, u64)]) -> Vec<SegmentState> {
        ranges
            .iter()
            .map(|&(start, end)| SegmentState {
                start,
                end,
                done: 0,
                merged: false,
            })
            .collect()
    }

    #[test]
    fn claim_cap_never_exceeds_pool() {
        // 2.9 GB, wanted 8: plan gives <=8, repeated claims/splits stay <=8.
        let total: u64 = 2_900_000_000;
        let plan = plan_segments(total, 8);
        assert!(plan.len() <= 8 && !plan.is_empty());
        let mut segs = mk_segs(&plan);
        let cap = segs.len();
        let mut active: std::collections::HashSet<usize> = (0..cap).collect();
        // Simulate idle workers repeatedly trying to steal work.
        for _ in 0..100 {
            // free a slot, leave others active with plenty remaining
            active.remove(&0);
            match try_claim_or_split(&mut segs, &mut active, cap) {
                Claim::None => {}
                Claim::Pending(i) | Claim::Split(i) => {
                    assert!(active.contains(&i));
                }
            }
            assert!(
                segs.iter().filter(|s| !s.merged).count() <= parts_cap(total, cap as u32),
                "total segments exceeded parts_cap"
            );
            // ranges stay disjoint, ordered, cover [0, total)
            let mut expect = 0u64;
            // segs stay in start order: splits only append tail of a victim
            for sg in &segs {
                assert_eq!(sg.start, expect, "gap/overlap in segments");
                assert!(sg.end >= sg.start);
                expect = sg.end + 1;
            }
            assert_eq!(expect, total);
        }
    }

    #[test]
    fn redact_url_hides_token() {
        let r = redact_url("https://cdn.example.com/d/SECRETTOKEN/file.zip?sig=abc#x");
        assert_eq!(r, "https://cdn.example.com/.../file.zip");
        assert!(!r.contains("SECRETTOKEN") && !r.contains("sig"));
        assert_eq!(redact_url("not a url"), "...");
    }

    #[test]
    fn part_size_scales_with_file() {
        let gib = 1024u64 * 1024 * 1024;
        // 100MiB @8: floor-bound 4MiB parts, 25 parts (not 8).
        // Auto cap tiers: 64 -> 128 -> 192 -> 256 MiB.
        assert_eq!(auto_max_part(2 * gib), 64 * 1024 * 1024);
        assert_eq!(auto_max_part(8 * gib), 128 * 1024 * 1024);
        assert_eq!(auto_max_part(32 * gib), 192 * 1024 * 1024);
        assert_eq!(auto_max_part(100 * gib), 256 * 1024 * 1024);
        // Override wins and clamps; 0 = auto.
        assert_eq!(
            effective_max_part(100 * gib, 64 * 1024 * 1024),
            64 * 1024 * 1024
        );
        assert_eq!(effective_max_part(100 * gib, 0), 256 * 1024 * 1024);
        assert_eq!(effective_max_part(gib, 9999 * 1024 * 1024), MAX_PART_LIMIT);
        // Huge file auto: far fewer parts than 64 MiB cap would give.
        assert_eq!(target_part_size(100 * gib, 8), 256 * 1024 * 1024);
        assert_eq!(plan_parts(100 * gib, 8).len(), 400);
        // Manual 64 MiB on huge file restores old count.
        assert_eq!(plan_parts_with(100 * gib, 8, 64 * 1024 * 1024).len(), 1600);
        // Explicit part size is a TARGET, not ceiling (user report:
        // 512 MiB set on 6 GiB file still gave 49 x ~126 MiB auto parts).
        // 6 GiB / 512 MiB = 12 parts; no wave-slack re-split.
        let six = 6 * gib;
        assert_eq!(
            target_part_size_with(six, 8, 512 * 1024 * 1024),
            512 * 1024 * 1024
        );
        assert_eq!(plan_parts_with(six, 8, 512 * 1024 * 1024).len(), 12);
        assert_eq!(parts_cap_with(six, 8, 512 * 1024 * 1024), 12);
        // 256 MiB manual on 6 GiB: 24 parts.
        assert_eq!(plan_parts_with(six, 8, 256 * 1024 * 1024).len(), 24);
        // Tiny file still parallelizes: at least pool parts.
        assert_eq!(
            plan_parts_with(32 * 1024 * 1024, 8, 512 * 1024 * 1024).len(),
            8
        );
        // Auto fingerprint (old bug): 6 GiB auto = 128 MiB cap tier.
        assert_eq!(target_part_size(six, 8), 128 * 1024 * 1024);
        assert_eq!(plan_parts(six, 8).len(), 48);
        assert_eq!(target_part_size(100 * 1024 * 1024, 8), MIN_SPLIT);
        assert_eq!(plan_parts(100 * 1024 * 1024, 8).len(), 25);
        // 2GiB @8: 64MiB cap-bound, 32 parts (no 1MiB shredding).
        assert_eq!(target_part_size(2 * gib, 8), MAX_PART);
        assert_eq!(plan_parts(2 * gib, 8).len(), 32);
        // 2GiB @16: still 64MiB (waves would give 32MiB >? 2GiB/64=32MiB -> clamp floor? no: 32MiB in [4,64])
        assert_eq!(plan_parts(2 * gib, 16).len(), 64);
        // tiny file -> one part per pool worker (floor relaxes)
        assert_eq!(plan_parts(1024, 8).len(), 8);
        // parts tiles exactly
        let plan = plan_parts(2 * gib + 123, 8);
        let mut expect = 0u64;
        for (s, e) in &plan {
            assert_eq!(*s, expect);
            expect = e + 1;
        }
        assert_eq!(expect, 2 * gib + 123);
        // split respects target: 64MiB remaining, target 64MiB -> no split
        assert_eq!(split_point_min(0, 64 * 1024 * 1024 - 1, 0, MAX_PART), None);
        assert!(split_point_min(0, 128 * 1024 * 1024 - 1, 0, MAX_PART).is_some());
    }

    #[test]
    fn gate_learned_max_and_permit() {
        let g = Arc::new(ConcurrencyGate::new(8));
        let p1 = g.try_acquire().unwrap();
        let p2 = g.try_acquire().unwrap();
        let _p3 = g.try_acquire().unwrap();
        // Two 206-admitted streamers -> learned max 2 on 429.
        g.mark_streaming();
        g.mark_streaming();
        assert_eq!(g.on_throttled(), 2);
        assert_eq!(g.limit(), 2);
        g.unmark_streaming();
        g.unmark_streaming();
        assert!(g.try_acquire().is_none());
        drop(p1);
        assert!(g.try_acquire().is_none());
        drop(p2);
        assert!(g.try_acquire().is_some());
        // Floor 1: throttle with nothing streaming.
        let g1 = Arc::new(ConcurrencyGate::new(8));
        let _a = g1.try_acquire().unwrap();
        assert_eq!(g1.on_throttled(), 1);
        assert_eq!(g1.limit(), 1);
        for _ in 0..GATE_REGAIN_STREAK {
            g1.on_clean();
        }
        assert_eq!(g1.limit(), 2);
        // Jitter spreads wake times deterministically per gen/idx.
        assert_ne!(throttle_jitter(1, 0), throttle_jitter(1, 3));
        assert!(throttle_jitter(9, 7) < 5000);
        // Retry-After parse: seconds, default, clamp.
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_secs(&h), 5);
        h.insert(reqwest::header::RETRY_AFTER, "10".parse().unwrap());
        assert_eq!(retry_after_secs(&h), 10);
        h.insert(reqwest::header::RETRY_AFTER, "999".parse().unwrap());
        assert_eq!(retry_after_secs(&h), 60);
    }

    #[test]
    fn coalesce_pending_merges_only_unstarted() {
        let mb = 1024 * 1024u64;
        // 8 x 10MiB, idx0-1 active, idx2 done>0, rest pending contiguous.
        let mut segs = mk_segs(&[
            (0, 10 * mb - 1),
            (10 * mb, 20 * mb - 1),
            (20 * mb, 30 * mb - 1),
            (30 * mb, 40 * mb - 1),
            (40 * mb, 50 * mb - 1),
            (50 * mb, 60 * mb - 1),
        ]);
        segs[2].done = 5 * mb;
        let active: std::collections::HashSet<usize> = [0, 1].into();
        assert!(coalesce_pending(&mut segs, &active));
        // pending 3,4,5 collapse into 3; active/started untouched.
        assert_eq!(segs[3].end, 60 * mb - 1);
        assert!(segs[4].merged && segs[5].merged);
        assert!(!segs[0].merged && !segs[1].merged && !segs[2].merged);
        // tiles [0,total) over non-merged
        let mut expect = 0u64;
        for sg in segs.iter().filter(|s| !s.merged) {
            assert_eq!(sg.start, expect);
            expect = sg.end + 1;
        }
        assert_eq!(expect, 60 * mb);
        // idempotent
        assert!(!coalesce_pending(&mut segs, &active));
    }

    #[test]
    fn claim_no_split_at_cap_or_below_floor() {
        let mb = 1024 * 1024u64;
        // At live cap: no split even with huge remaining (1 done + 1 live, cap 1).
        let mut segs = mk_segs(&[(0, 100 * mb - 1), (100 * mb, 200 * mb - 1)]);
        let mut active: std::collections::HashSet<usize> = [0, 1].into();
        segs[1].done = 100 * mb;
        active.remove(&1);
        assert!(matches!(
            try_claim_or_split(&mut segs, &mut active, 1),
            Claim::None
        ));
        assert_eq!(segs.len(), 2);
        // Parts cap 3 (2 existing + 1): done segment still counts, split ok.
        let mut segs = mk_segs(&[(0, 100 * mb - 1), (100 * mb, 200 * mb - 1)]);
        let mut active: std::collections::HashSet<usize> = [0, 1].into();
        segs[1].done = 100 * mb;
        active.remove(&1);
        assert!(matches!(
            try_claim_or_split(&mut segs, &mut active, 3),
            Claim::Split(_)
        ));
        // Below cap but remaining < 2*MIN_SPLIT: no split.
        let mut segs = mk_segs(&[(0, 4 * mb - 1)]);
        let mut active: std::collections::HashSet<usize> = [0].into();
        assert!(matches!(
            try_claim_or_split(&mut segs, &mut active, 8),
            Claim::None
        ));
        assert_eq!(segs.len(), 1);
        // Below cap + large remaining: splits exactly once into halves >= floor.
        let mut segs = mk_segs(&[(0, 64 * mb - 1)]);
        let mut active: std::collections::HashSet<usize> = [0].into();
        match try_claim_or_split(&mut segs, &mut active, 8) {
            Claim::Split(1) => {}
            _ => panic!("expected split to idx 1"),
        }
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].end + 1, segs[1].start);
        assert!(segs[0].end + 1 - segs[0].start >= MIN_SPLIT);
        assert!(segs[1].end + 1 - segs[1].start >= MIN_SPLIT);
    }

    #[test]
    fn claim_pending_preferred_and_overcap_legacy_ok() {
        let mb = 1024 * 1024u64;
        // Pending unclaimed wins over split.
        let mut segs = mk_segs(&[(0, 64 * mb - 1), (64 * mb, 128 * mb - 1)]);
        let mut active: std::collections::HashSet<usize> = [0].into();
        assert!(matches!(
            try_claim_or_split(&mut segs, &mut active, 8),
            Claim::Pending(1)
        ));
        assert_eq!(segs.len(), 2);
        // Legacy over-split state (30 segs, cap 8): loads, claims pending,
        // never splits further.
        let plan = plan_segments(300 * mb, 30);
        assert_eq!(plan.len(), 30);
        let mut segs = mk_segs(&plan);
        let mut active: std::collections::HashSet<usize> = [0].into();
        assert!(matches!(
            try_claim_or_split(&mut segs, &mut active, 8),
            Claim::Pending(_)
        ));
        active = (0..30).collect();
        segs[1].done = segs[1].end + 1 - segs[1].start; // finish one
        active.remove(&1);
        assert!(matches!(
            try_claim_or_split(&mut segs, &mut active, 8),
            Claim::None
        ));
        assert_eq!(segs.len(), 30);
    }

    #[test]
    fn seg_keep_and_shrink_correction_accounting() {
        // range [1000, 1999], streamed 800, victim end shrunk to 1499
        assert_eq!(seg_keep(1499, 1000, 800), 500);
        // no shrink: keep all
        assert_eq!(seg_keep(1999, 1000, 800), 800);
        // written not yet past split: keep all written
        assert_eq!(seg_keep(1499, 1000, 300), 300);
        // correction subtracts exactly discarded bytes, once
        let c = Controls::new(0);
        c.downloaded.store(1800, Ordering::Relaxed);
        apply_shrink_correction(&c, 800, 500);
        assert_eq!(c.downloaded.load(Ordering::Relaxed), 1500);
        apply_shrink_correction(&c, 500, 500);
        assert_eq!(
            c.downloaded.load(Ordering::Relaxed),
            1500,
            "no double subtract"
        );
        // invariant: after victim commits keep + thief commits tail,
        // global counter == committed sum, no byte counted twice.
        let victim_keep = seg_keep(1499, 1000, 800);
        let thief_bytes = 1999 - 1500 + 1; // stolen tail [1500,1999]
        let committed = victim_keep + thief_bytes;
        assert_eq!(committed, 1000);
        // retry recount: download_range re-adds from committed done each
        // attempt, attempt-token reset makes re-add idempotent per attempt;
        // committed_total helper pins sum used for resume/part sync.
        let segs = vec![
            SegmentState {
                start: 0,
                end: 999,
                done: 400,
                merged: false,
            },
            SegmentState {
                start: 1000,
                end: 1999,
                done: 500,
                merged: false,
            },
        ];
        assert_eq!(committed_total(&segs), 900);
    }
    #[test]
    fn cd_rfc5987_utf8() {
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''%E2%82%AC%20rates.pdf")
                .as_deref(),
            Some("€ rates.pdf")
        );
    }
    #[test]
    fn cd_star_wins_over_plain() {
        assert_eq!(
            parse_content_disposition(
                "attachment; filename=\"plain.zip\"; filename*=UTF-8''star%20name.zip"
            )
            .as_deref(),
            Some("star name.zip")
        );
    }
    #[test]
    fn cd_plain_quoted_and_unquoted() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"my file.zip\"").as_deref(),
            Some("my file.zip")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=report.pdf").as_deref(),
            Some("report.pdf")
        );
    }
    #[test]
    fn cd_sanitizes_traversal_and_controls() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"../../evil.exe\"").as_deref(),
            Some("evil.exe")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''..%2F..%2Fevil.exe").as_deref(),
            Some("evil.exe")
        );
        assert_eq!(parse_content_disposition("attachment"), None);
        assert_eq!(parse_content_disposition("attachment; filename=\"\""), None);
    }
    #[test]
    fn cd_latin1_star() {
        // 0xE9 = é in ISO-8859-1
        assert_eq!(
            parse_content_disposition("attachment; filename*=ISO-8859-1''caf%E9.txt").as_deref(),
            Some("café.txt")
        );
    }
    #[test]
    fn adopt_only_generic_names() {
        let cd = Some("attachment; filename=\"real-name.zip\"");
        assert_eq!(
            adopt_server_filename("download", cd, None).as_deref(),
            Some("real-name.zip")
        );
        assert_eq!(
            adopt_server_filename("", cd, None).as_deref(),
            Some("real-name.zip")
        );
        assert_eq!(
            adopt_server_filename("index", cd, None).as_deref(),
            Some("real-name.zip")
        );
        // User-renamed specific name never overridden.
        assert_eq!(adopt_server_filename("my-rename.zip", cd, None), None);
        assert_eq!(adopt_server_filename("holiday.mp4", cd, None), None);
        // Generic without extension + no CD: MIME fallback.
        assert_eq!(
            adopt_server_filename("download", None, Some("video/mp4")).as_deref(),
            Some("download.mp4")
        );
        assert_eq!(
            adopt_server_filename("download.bin", None, Some("video/mp4")),
            None,
            "has extension already"
        );
        assert_eq!(adopt_server_filename("download", None, None), None);
    }
    #[test]
    fn generic_detection() {
        assert!(is_generic_filename(""));
        assert!(is_generic_filename("download"));
        assert!(is_generic_filename("download.bin"));
        assert!(is_generic_filename("index.html"));
        assert!(is_generic_filename("noext"));
        assert!(!is_generic_filename("report.pdf"));
    }
    #[test]
    fn dedup_picks_suffix_on_size_conflict() {
        let dir = std::env::temp_dir().join(format!("wc-dedup-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join("file.zip");
        std::fs::write(&dest, b"12345").unwrap(); // 5 bytes
        assert_eq!(dedup_dest(&dest, Some(5)), dest, "same size keeps dest");
        assert_eq!(
            dedup_dest(&dest, Some(99)),
            dir.join("file (1).zip"),
            "different size dedups"
        );
        std::fs::write(dir.join("file (1).zip"), b"x").unwrap();
        assert_eq!(dedup_dest(&dest, Some(99)), dir.join("file (2).zip"));
        assert_eq!(
            dedup_dest(&dir.join("absent.zip"), Some(1)),
            dir.join("absent.zip")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
