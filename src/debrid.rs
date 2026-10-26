//! Debrid normalization layer over Real-Debrid (`crate::debrid_rd`),
//! AllDebrid (`crate::debrid_ad`), TorBox (`crate::debrid_tb`).
//!
//! Rules: tokens + resolved direct URLs never logged; persisted tasks keep
//! original magnet/hoster URL (direct URLs expire / IP-bound), resolution
//! happens fresh on every download start with same reqwest client used for
//! download (IP binding). Any debrid failure -> caller falls back.

use crate::model::{DebridProviderCfg, Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Rd,
    Ad,
    Tb,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Rd => "Real-Debrid",
            Provider::Ad => "AllDebrid",
            Provider::Tb => "TorBox",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Provider::Rd => "rd",
            Provider::Ad => "ad",
            Provider::Tb => "tb",
        }
    }
    pub fn from_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rd" | "real-debrid" | "realdebrid" => Some(Provider::Rd),
            "ad" | "alldebrid" => Some(Provider::Ad),
            "tb" | "torbox" => Some(Provider::Tb),
            _ => None,
        }
    }
    pub fn cfg(self, s: &Settings) -> &DebridProviderCfg {
        match self {
            Provider::Rd => &s.debrid_rd,
            Provider::Ad => &s.debrid_ad,
            Provider::Tb => &s.debrid_tb,
        }
    }
}

pub const ALL: [Provider; 3] = [Provider::Rd, Provider::Ad, Provider::Tb];

/// Enabled+configured providers in preference order (preferred first,
/// then RD, AD, TB).
pub fn enabled_providers(s: &Settings) -> Vec<Provider> {
    let mut out: Vec<Provider> = Vec::new();
    if let Some(p) = Provider::from_id(&s.debrid_preferred) {
        if p.cfg(s).usable() {
            out.push(p);
        }
    }
    for p in ALL {
        if p.cfg(s).usable() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

pub fn any_for_hosters(s: &Settings) -> bool {
    enabled_providers(s)
        .iter()
        .any(|p| p.cfg(s).use_for_hosters)
}
pub fn any_for_magnets(s: &Settings) -> bool {
    enabled_providers(s)
        .iter()
        .any(|p| p.cfg(s).use_for_magnets)
}

/// Prefix `e` with `label:` unless it already carries that prefix
/// (provider clients like RD/AD/TB self-prefix their error strings;
/// double-prefixing confuses the user).
fn prefixed(label: &str, e: &str) -> String {
    if e.starts_with(&format!("{label}:")) {
        e.to_string()
    } else {
        format!("{label}: {e}")
    }
}

/// Pick this task's file from RD `/unrestrict/folder` entries: the picked
/// file's handle appearing in the entry link wins, then exact filename,
/// then a sole entry. `None` = no confident match; caller falls through.
pub fn pick_rd_folder_link<'a>(
    entries: &'a [crate::debrid_rd::RdFolderLink],
    handle: &str,
    name: &str,
) -> Option<&'a crate::debrid_rd::RdFolderLink> {
    if !handle.is_empty() {
        if let Some(e) = entries.iter().find(|e| e.link.contains(handle)) {
            return Some(e);
        }
    }
    if !name.is_empty() {
        if let Some(e) = entries.iter().find(|e| e.filename == name) {
            return Some(e);
        }
    }
    if entries.len() == 1 {
        return entries.first();
    }
    None
}

/// Per-download providers allowed by `choice` (task `debrid_choice`):
/// `"none"` -> none; `"rd"|"ad"|"tb"` -> just that provider when usable;
/// anything else (`"auto"`) -> `enabled_providers`. Magnet/hoster
/// `use_for_*` flags still gate inside resolvers, so a forced provider
/// with that flag off errors and caller falls back (local for hosters,
/// local torrent for magnets when start-path resolution fails).
pub fn providers_for(s: &Settings, choice: &str) -> Vec<Provider> {
    match choice {
        "none" => Vec::new(),
        "rd" | "ad" | "tb" => Provider::from_id(choice)
            .filter(|p| p.cfg(s).usable())
            .into_iter()
            .collect(),
        _ => enabled_providers(s),
    }
}

/// Mirrors resolvers' gating for one kind without network: hosters need a
/// provider match anyway, so hoster UI key is provider allowance only.
pub fn any_for_magnets_choice(s: &Settings, choice: &str) -> bool {
    providers_for(s, choice)
        .iter()
        .any(|p| p.cfg(s).use_for_magnets)
}

#[derive(Clone, Debug, Default)]
pub struct Account {
    pub provider: &'static str,
    pub username: String,
    pub email: String,
    pub plan: String,
    pub is_premium: bool,
    /// Unix seconds.
    pub expires_at: Option<i64>,
}

impl Account {
    pub fn summary(&self) -> String {
        match days_left(self.expires_at, now_unix()) {
            Some(d) if d >= 0 => format!(
                "{}: {} <{}> plan {} premium until {} ({}d left)",
                self.provider,
                self.username,
                self.email,
                self.plan,
                fmt_date(self.expires_at.unwrap_or(0)),
                d
            ),
            _ if self.is_premium => format!(
                "{}: {} <{}> plan {} premium (no expiry reported)",
                self.provider, self.username, self.email, self.plan
            ),
            _ => format!(
                "{}: {} <{}> plan {} NOT premium (free/expired)",
                self.provider, self.username, self.email, self.plan
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostStatus {
    pub name: String,
    /// Primary domain (first known), kept for display/back-compat.
    pub domain: String,
    /// All alias domains for this hoster, incl. `domain`.
    pub domains: Vec<String>,
    pub up: bool,
    pub supported: bool,
}

/// Well-known alias groups providers report inconsistently. RD `/hosts`
/// keys Mega only as `mega.co.nz` and Rapidgator only as `rapidgator.net`,
/// while user links use `mega.nz` / `rg.to`; AD/TB list all of these.
/// ponytail: static groups for known mismatches; expand when a provider
/// reports a new alias split that breaks routing.
const ALIAS_GROUPS: &[&[&str]] = &[
    &["mega.nz", "mega.co.nz", "mega.io"],
    &["rapidgator.net", "rg.to", "rapidgator.asia"],
];

fn alias_group(domain: &str) -> Option<&'static [&'static str]> {
    let d = domain.trim().to_ascii_lowercase();
    ALIAS_GROUPS
        .iter()
        .find(|g| g.contains(&d.as_str()))
        .copied()
}

/// Lowercase/dedupe domains and expand any known alias group members.
fn expand_domains<I: IntoIterator<Item = String>>(src: I) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in src {
        let d = url_host(&d);
        if d.is_empty() || out.contains(&d) {
            continue;
        }
        out.push(d.clone());
        if let Some(g) = alias_group(&d) {
            for a in g {
                if !out.iter().any(|x| x == a) {
                    out.push((*a).to_string());
                }
            }
        }
    }
    out
}

fn domain_matches(want: &str, listed: &str) -> bool {
    let listed = url_host(listed);
    if listed.is_empty() {
        return false;
    }
    if want == listed || want.ends_with(&format!(".{listed}")) {
        return true;
    }
    // Alias equivalence: want is (subdomain of) an alias of listed.
    if let Some(g) = alias_group(&listed) {
        return g
            .iter()
            .any(|a| want == *a || want.ends_with(&format!(".{a}")));
    }
    false
}

/// Jaro similarity 0..1 (hand-rolled; no dep for one metric).
fn jaro(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() && b.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    if a == b {
        return 1.0;
    }
    let dist = (a.len().max(b.len()) / 2).saturating_sub(1);
    let (mut am, mut bm) = (vec![false; a.len()], vec![false; b.len()]);
    let mut m = 0usize;
    for i in 0..a.len() {
        let lo = i.saturating_sub(dist);
        let hi = (i + dist + 1).min(b.len());
        for j in lo..hi {
            if !bm[j] && a[i] == b[j] {
                am[i] = true;
                bm[j] = true;
                m += 1;
                break;
            }
        }
    }
    if m == 0 {
        return 0.0;
    }
    let mut t = 0usize;
    let mut k = 0usize;
    for i in 0..a.len() {
        if am[i] {
            while !bm[k] {
                k += 1;
            }
            if a[i] != b[k] {
                t += 1;
            }
            k += 1;
        }
    }
    let m = m as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - t as f64 / 2.0) / m) / 3.0
}

/// Jaro-Winkler: prefix boost (domains share TLD starts often; still
/// gated below by label+family rules).
fn jaro_winkler(a: &str, b: &str) -> f64 {
    let j = jaro(a, b);
    let prefix = a
        .chars()
        .zip(b.chars())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count();
    j + prefix as f64 * 0.1 * (1.0 - j)
}

/// Registrable-ish split: label = SLD, family = suffix. Known
/// multi-label suffixes (co.nz, co.uk, com.au...) collapse first:
/// mega.co.nz -> (mega, co.nz). ponytail: short suffix table, not full
/// public-suffix list; unknown multi-label suffixes just miss fuzzy.
const MULTI_SUFFIXES: &[&str] = &[
    "co.nz", "co.uk", "com.au", "co.in", "com.br", "co.za", "co.kr", "com.cn", "com.tw", "com.mx",
    "co.jp", "com.sg", "com.hk",
];

fn domain_parts(host: &str) -> (String, String) {
    let h = url_host(host);
    for sfx in MULTI_SUFFIXES {
        if h == *sfx {
            return (String::new(), sfx.to_string());
        }
        if let Some(label) = h
            .strip_suffix(&format!(".{sfx}"))
            .and_then(|p| p.rsplit('.').next())
            .filter(|l| !l.is_empty())
        {
            return (label.to_string(), sfx.to_string());
        }
    }
    let mut it = h.rsplitn(3, '.');
    let tld = it.next().unwrap_or("").to_string();
    let label = it.next().unwrap_or("").to_string();
    (label, tld)
}

/// Suffix family: trailing bare TLD (nz from co.nz) for ccTLD-family
/// check; full suffix when equal.
fn suffix_family(a: &str, b: &str) -> bool {
    a == b
        || (a.rsplit('.').next() == b.rsplit('.').next()
            && a.rsplit('.').next().is_some_and(|t| t.len() == 2)
            && b.rsplit('.').next().is_some_and(|t| t.len() == 2))
}

/// Domain similarity 0..1 for routing fallback only. Exact/subdomain/
/// alias handled by `domain_matches` before this runs. Rules keep fuzzy
/// safe: SLD labels compared, TLDs must share family (both ccTLD-len-2
/// or equal) so mega.nz~mega.co.nz passes, mega.nz~mega.com does not,
/// and lookalikes (notmega.nz) score low on label prefix.
pub fn domain_similarity(want: &str, listed: &str) -> f64 {
    let want = url_host(want);
    let listed = url_host(listed);
    if want.is_empty() || listed.is_empty() {
        return 0.0;
    }
    let (wl, wt) = domain_parts(&want);
    let (ll, lt) = domain_parts(&listed);
    if wl.is_empty() || ll.is_empty() {
        return 0.0;
    }
    let label_sim = jaro_winkler(&wl, &ll);
    if wt == lt {
        label_sim
    } else if suffix_family(&wt, &lt) || (wl == ll && wl.len() >= 4) {
        // Different suffix, same label: mega.nz vs mega.co.nz (ccTLD
        // family), rapidgator.net vs rapidgator.asia (gTLD move).
        // Full-label equality required for cross-suffix credit, so
        // typos get none here — they need same-suffix path above.
        // Short labels excluded (ul.to-style abbreviations).
        label_sim * 0.95
    } else {
        label_sim * 0.5
    }
}

/// Fuzzy routing threshold: user asked >85%.
pub const SIMILARITY_THRESHOLD: f64 = 0.85;

/// Best domain score for one hoster entry: 1.0 exact/alias via
/// `domain_matches`, else best `domain_similarity` over its domains.
fn host_score(h: &HostStatus, want: &str) -> f64 {
    let mut best = 0.0f64;
    for d in std::iter::once(h.domain.as_str()).chain(h.domains.iter().map(String::as_str)) {
        if domain_matches(want, d) {
            return 1.0;
        }
        best = best.max(domain_similarity(want, d));
    }
    best
}

#[derive(Clone, Debug)]
pub struct ResolvedFile {
    pub name: String,
    pub size: Option<u64>,
    /// Direct download URL. Never persist, never log.
    pub url: String,
    /// Provider already knew file cached (instant availability). `None`
    /// = API gave no cheap signal. Display only; never persist.
    pub cached: Option<bool>,
}

/// Pure builder for `DownloadTask::debrid_info`:
/// `"<Provider label> · <hoster host> · <cached|cached: unknown|not cached>"`.
/// Magnet URLs have no hoster host; caller passes name-derived label
/// (`"magnet"`). Empty host -> omitted only if caller passes empty string;
/// keeps format exact for hoster path (host always from `url_host`).
pub fn format_debrid_info(provider: &str, host: &str, cached: Option<bool>) -> String {
    let cache = match cached {
        Some(true) => "cached",
        Some(false) => "not cached",
        None => "cached: unknown",
    };
    format!("{provider} · {host} · {cache}")
}

/// ISO-8601 `2026-11-18T10:00:00.000Z` -> unix secs (UTC), via TB parser.
pub fn parse_iso_unix(s: &str) -> Option<i64> {
    crate::debrid_tb::parse_expiry(s)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn days_left(expires_at: Option<i64>, now: i64) -> Option<i64> {
    expires_at.map(|e| (e - now).div_euclid(86400))
}

/// YYYY-MM-DD UTC, no chrono dep.
pub fn fmt_date(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    // civil-from-days (Hinnant)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Strip scheme/www, lowercase host from URL. Empty on garbage.
pub fn url_host(url: &str) -> String {
    let s = url.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    let host = s.split(['/', '?', '#', ':']).next().unwrap_or("");
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Google Drive direct-download hosts (share pages + one-shot takeout/
/// usercontent GETs). These serve direct bytes, not hoster landing pages;
/// debrid providers cannot unrestrict them, and the fuzzy host matcher
/// would otherwise claim them (takeout host scores 1.0 vs drive.google.com).
/// Direct-only: never route through debrid.
pub fn is_google_drive_url(url: &str) -> bool {
    let h = url_host(url);
    h == "drive.google.com"
        || h == "usercontent.google.com"
        || h.ends_with(".usercontent.google.com")
        || h == "googleusercontent.com"
        || h.ends_with(".googleusercontent.com")
}

pub fn is_magnet(url: &str) -> bool {
    crate::torrent::is_magnet(url)
}

/// btih hash from magnet (hex 40 or base32 32), lowercase hex if hex.
pub fn magnet_hash(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let i = lower.find("urn:btih:")? + "urn:btih:".len();
    let rest = &lower[i..];
    let h: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    // hex40 or base32-32 both usable (providers accept magnet itself)
    if (h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())) || h.len() == 32 {
        Some(h)
    } else {
        None
    }
}

// ---------- per-provider dispatch ----------

pub async fn account(http: reqwest::Client, p: Provider, key: &str) -> Result<Account, String> {
    match p {
        Provider::Rd => {
            let c = crate::debrid_rd::RdClient::new(key, http);
            let u = c.user().await?;
            // RD expiry: ISO string `expiration`; premium secs authoritative.
            let expires_at = u
                .expiration
                .as_deref()
                .and_then(parse_iso_unix)
                .or_else(|| {
                    if u.premium > 0 {
                        Some(now_unix() + u.premium)
                    } else {
                        None
                    }
                });
            let is_premium = u.is_premium();
            Ok(Account {
                provider: "Real-Debrid",
                username: u.username,
                email: u.email,
                plan: u.account_type,
                is_premium,
                expires_at,
            })
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let u = c.user().await?;
            let is_premium = u.premium_active(now_unix());
            Ok(Account {
                provider: "AllDebrid",
                username: u.username,
                email: u.email,
                plan: if u.is_trial {
                    "trial".into()
                } else if u.is_premium {
                    "premium".into()
                } else {
                    "free".into()
                },
                is_premium,
                expires_at: u.premium_until,
            })
        }
        Provider::Tb => {
            // TbClient builds redirect-none client; account call unaffected.
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            let u = c.me().await?;
            let is_premium = u.is_premium();
            Ok(Account {
                provider: "TorBox",
                username: u.email.clone().unwrap_or_default(),
                email: u.email.unwrap_or_default(),
                plan: crate::debrid_tb::plan_label(u.plan).into(),
                is_premium,
                expires_at: u.expires_at,
            })
        }
    }
}

// ---------- hosts cache (successful lists only, 10 min TTL) ----------
const HOSTS_TTL: std::time::Duration = std::time::Duration::from_secs(600);
type HostsCache = std::collections::HashMap<(Provider, u64), (std::time::Instant, Vec<HostStatus>)>;
static HOSTS_CACHE: std::sync::OnceLock<std::sync::Mutex<HostsCache>> = std::sync::OnceLock::new();
fn hosts_cache() -> &'static std::sync::Mutex<HostsCache> {
    HOSTS_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
/// Hash only: raw key never stored/logged; token change -> cache miss.
fn key_hash(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}
fn hosts_cache_get(
    cache: &HostsCache,
    p: Provider,
    kh: u64,
    now: std::time::Instant,
) -> Option<Vec<HostStatus>> {
    let (at, hosts) = cache.get(&(p, kh))?;
    (now.duration_since(*at) < HOSTS_TTL).then(|| hosts.clone())
}
fn hosts_cache_put(
    cache: &mut HostsCache,
    p: Provider,
    kh: u64,
    now: std::time::Instant,
    hosts: Vec<HostStatus>,
) {
    cache.insert((p, kh), (now, hosts));
}

pub async fn hosts(
    http: reqwest::Client,
    p: Provider,
    key: &str,
) -> Result<Vec<HostStatus>, String> {
    let kh = key_hash(key);
    if let Some(hit) = hosts_cache()
        .lock()
        .ok()
        .and_then(|c| hosts_cache_get(&c, p, kh, std::time::Instant::now()))
    {
        return Ok(hit);
    }
    let list = hosts_uncached(http, p, key).await?;
    if let Ok(mut c) = hosts_cache().lock() {
        hosts_cache_put(&mut c, p, kh, std::time::Instant::now(), list.clone());
    }
    Ok(list)
}

async fn hosts_uncached(
    http: reqwest::Client,
    p: Provider,
    key: &str,
) -> Result<Vec<HostStatus>, String> {
    match p {
        Provider::Rd => {
            let c = crate::debrid_rd::RdClient::new(key, http);
            Ok(c.hosts_status()
                .await?
                .into_iter()
                .map(|h| {
                    let up = h.is_up();
                    let domains = expand_domains([h.domain.clone()]);
                    HostStatus {
                        name: if h.name.is_empty() {
                            h.domain.clone()
                        } else {
                            h.name
                        },
                        up,
                        domain: domains.first().cloned().unwrap_or_else(|| h.domain.clone()),
                        domains,
                        supported: h.supported,
                    }
                })
                .collect())
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            // per-account quotas when key present, else public
            let list = if key.trim().is_empty() {
                c.hosts().await?
            } else {
                c.user_hosts().await.or(c.hosts().await)?
            };
            Ok(list
                .into_iter()
                .map(|h| {
                    let mut all: Vec<String> = Vec::new();
                    if !h.host.is_empty() {
                        all.push(h.host.clone());
                    }
                    all.extend(h.domains.iter().cloned());
                    let domains = expand_domains(all);
                    HostStatus {
                        name: h.name,
                        domain: domains.first().cloned().unwrap_or_default(),
                        domains,
                        up: h.status.unwrap_or(false),
                        supported: h.status.is_some(),
                    }
                })
                .collect())
        }
        Provider::Tb => {
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            Ok(c.hosters()
                .await?
                .into_iter()
                .map(|h| {
                    // TB `url` may hold sole domain when `domains` empty.
                    let extra = if h.url.contains('.') {
                        vec![h.url.clone()]
                    } else {
                        Vec::new()
                    };
                    let domains = expand_domains(h.domains.iter().cloned().chain(extra));
                    HostStatus {
                        name: h.name,
                        domain: domains.first().cloned().unwrap_or_default(),
                        domains,
                        up: h.status,
                        supported: true,
                    }
                })
                .collect())
        }
    }
}

pub fn host_up(hosts: &[HostStatus], url: &str) -> Option<bool> {
    let want = url_host(url);
    if want.is_empty() {
        return None;
    }
    hosts
        .iter()
        .find(|h| {
            std::iter::once(h.domain.as_str())
                .chain(h.domains.iter().map(String::as_str))
                .any(|d| domain_matches(&want, d))
        })
        .map(|h| h.supported && h.up)
}

/// One provider's ranked match for a URL host: score 1.0 exact/alias,
/// `SIMILARITY_THRESHOLD..1` fuzzy, plus whether listed host is up.
/// Best-scoring entry wins; up entries beat down at equal score.
#[derive(Debug, Clone, PartialEq)]
pub struct HostMatch {
    pub score: f64,
    pub up: bool,
    pub supported: bool,
    pub name: String,
    pub domain: String,
}

pub fn host_match(hosts: &[HostStatus], url: &str) -> Option<HostMatch> {
    let want = url_host(url);
    if want.is_empty() {
        return None;
    }
    let mut best: Option<HostMatch> = None;
    for h in hosts {
        let score = host_score(h, &want);
        if score < SIMILARITY_THRESHOLD {
            continue;
        }
        let cand = HostMatch {
            score,
            up: h.up,
            supported: h.supported,
            name: h.name.clone(),
            domain: h.domain.clone(),
        };
        let better = match &best {
            None => true,
            Some(b) => score > b.score || (score == b.score && cand.up && !b.up),
        };
        if better {
            best = Some(cand);
        }
    }
    best
}

/// Check-then-sleep poll: first check immediate; interval slept only
/// before later checks, at most max_polls checks total.
async fn poll_delayed<C, CFut, S, SFut, T>(
    max_polls: usize,
    interval: std::time::Duration,
    mut sleep: S,
    mut check: C,
) -> Result<Option<T>, String>
where
    C: FnMut() -> CFut,
    CFut: std::future::Future<Output = Result<Option<T>, String>>,
    S: FnMut(std::time::Duration) -> SFut,
    SFut: std::future::Future<Output = ()>,
{
    for i in 0..max_polls {
        if i > 0 {
            sleep(interval).await;
        }
        if let Some(v) = check().await? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Resolve hoster URL to direct link via provider. Err = unsupported/fail.
pub async fn unrestrict(
    http: reqwest::Client,
    p: Provider,
    key: &str,
    url: &str,
) -> Result<ResolvedFile, String> {
    match p {
        Provider::Rd => {
            let c = crate::debrid_rd::RdClient::new(key, http);
            let r = c.unrestrict_link(url).await?;
            if r.download.trim().is_empty() {
                return Err("Real-Debrid: unrestrict returned no link".into());
            }
            // ponytail: RD hoster unrestrict/link returns no cache-residency
            // field (only magnet instantAvailability, disabled upstream);
            // cached unknown, not fabricated. Add RD check-based signal
            // only if API exposes residency.
            Ok(ResolvedFile {
                name: r.filename,
                size: r.filesize,
                url: r.download,
                cached: None,
            })
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let u = c.unlock(url).await?;
            if let Some(link) = u.link {
                return Ok(ResolvedFile {
                    name: u.filename,
                    size: u.filesize.map(|n| n.max(0) as u64),
                    url: link,
                    cached: None,
                });
            }
            if let Some(id) = u.delayed_id {
                // First delayed poll immediate (unlock satisfies AD >=5s
                // spacing), then 5s polls, up to ~60s.
                let ready = poll_delayed(
                    13,
                    std::time::Duration::from_secs(5),
                    |d| tokio::time::sleep(d),
                    || async {
                        let d = c.delayed(id).await?;
                        Ok::<Option<crate::debrid_ad::AdDelayed>, String>(d.is_ready().then_some(d))
                    },
                )
                .await?;
                if let Some(d) = ready {
                    if let Some(link) = d.link {
                        return Ok(ResolvedFile {
                            name: d.filename,
                            size: d.filesize.map(|n| n.max(0) as u64),
                            url: link,
                            cached: None,
                        });
                    }
                }
                return Err("AllDebrid: delayed link not ready after 60s".into());
            }
            Err("AllDebrid: unlock returned no link".into())
        }
        Provider::Tb => {
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            let mut web = c.create_web_download(url).await?;
            // TorBox fetches non-cached hoster links server-side; docs say
            // mylist updates live every 5s. Poll until files ready.
            // ponytail: 24x5s=2min ceiling. Job keeps running server-side
            // past timeout; retry re-polls mylist and picks it up. Raise
            // if real files regularly exceed.
            const TB_POLLS: usize = 24;
            for _ in 0..TB_POLLS {
                if !web.error.is_empty() {
                    return Err(format!("TorBox: {}", web.error));
                }
                if web.ready() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                web = c
                    .web_list(Some(web.web_id))
                    .await?
                    .into_iter()
                    .next()
                    .ok_or("TorBox: web download vanished")?;
            }
            if !web.error.is_empty() {
                return Err(format!("TorBox: {}", web.error));
            }
            if !web.ready() {
                let pct = web
                    .progress
                    .map(|p| format!(" ({:.0}%)", p * 100.0))
                    .unwrap_or_default();
                return Err(format!(
                    "TorBox still fetching {} server-side{} — retry shortly to continue",
                    web.download_state, pct
                ));
            }
            let w = web;
            let f = w
                .files
                .first()
                .ok_or("TorBox: web download has no files yet")?;
            let link = c.request_web_dl(w.web_id, f.id).await?;
            Ok(ResolvedFile {
                name: if f.name.is_empty() {
                    w.name
                } else {
                    f.name.clone()
                },
                size: Some(f.size),
                url: link,
                cached: None,
            })
        }
    }
}

/// Cached check for magnet hash. Ok(false) = not cached.
pub async fn magnet_cached(
    http: reqwest::Client,
    p: Provider,
    key: &str,
    magnet: &str,
) -> Result<bool, String> {
    match p {
        Provider::Rd => {
            let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
            let c = crate::debrid_rd::RdClient::new(key, http);
            // instantAvailability disabled upstream (403 disabled_endpoint,
            // live 2026-10-19): degrade to unknown=not-cached-confirmable,
            // callers polling add-flow will still work. Report false here:
            // cached-check is advisory; magnet_files re-checks cached_only.
            match c.instant_availability(&hash).await {
                Ok(a) => Ok(a.cached),
                Err(_) => Ok(false),
            }
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let up = c.magnet_upload(magnet).await?;
            if up.ready {
                return Ok(true);
            }
            let st = c.magnet_status(up.id).await?;
            Ok(st.is_ready())
        }
        Provider::Tb => {
            let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            Ok(c.check_cached(&hash).await?.cached)
        }
    }
}

/// Add magnet via provider, poll until files ready, return per-file links.
/// Single provider attempt; caller tries next provider / local fallback.
pub async fn magnet_files(
    http: reqwest::Client,
    p: Provider,
    key: &str,
    magnet: &str,
    cached_only: bool,
) -> Result<Vec<ResolvedFile>, String> {
    match p {
        Provider::Rd => {
            if cached_only {
                // instantAvailability disabled upstream (403 disabled_endpoint):
                // cannot confirm cache -> treat as not cached for cached_only
                // (local fallback), never hard-fail with endpoint error.
                let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
                let c0 = crate::debrid_rd::RdClient::new(key, http.clone());
                let cached = c0
                    .instant_availability(&hash)
                    .await
                    .map(|a| a.cached)
                    .unwrap_or(false);
                if !cached {
                    return Err("Real-Debrid: magnet not confirmed cached (cached_only; instantAvailability disabled upstream)".into());
                }
            }
            let c = crate::debrid_rd::RdClient::new(key, http);
            let id = c.add_magnet(magnet).await?;
            // wait download status then select all + links
            let mut info = None;
            for i in 0..30 {
                let t = c.torrent_info(&id).await?;
                if t.status == "waiting_files_selection" {
                    let ids: Vec<String> = t.files.iter().map(|f| f.id.to_string()).collect();
                    if !ids.is_empty() {
                        c.select_files(&id, &ids.join(",")).await?;
                    }
                }
                if !t.links.is_empty()
                    && (t.status == "downloaded"
                        || t.status == "waiting_files_selection"
                        || !t.files.is_empty())
                {
                    // need selected files; poll until links appear after selection
                    if t.status == "downloaded" || i > 2 {
                        info = Some(t);
                        break;
                    }
                }
                if t.status == "error" || t.status == "magnet_error" || t.status == "dead" {
                    return Err(format!("Real-Debrid torrent status {}", t.status));
                }
                info = Some(t);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            let t = info.ok_or("Real-Debrid: no torrent info")?;
            if t.links.is_empty() {
                return Err(format!(
                    "Real-Debrid: torrent not ready (status {})",
                    t.status
                ));
            }
            let mut out = Vec::new();
            // links correspond to selected files in order; unrestrict each
            let selected: Vec<_> = t.files.iter().filter(|f| f.selected == 1).collect();
            for (i, link) in t.links.iter().enumerate() {
                let r = c.unrestrict_link(link).await?;
                let name = if !r.filename.is_empty() {
                    r.filename
                } else if let Some(f) = selected.get(i) {
                    f.path
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&f.path)
                        .to_string()
                } else {
                    String::new()
                };
                out.push(ResolvedFile {
                    name,
                    size: r.filesize,
                    url: r.download,
                    // RD links present == torrent finished on RD side, but
                    // instant-availability field unavailable; unknown.
                    cached: None,
                });
            }
            Ok(out)
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let up = c.magnet_upload(magnet).await?;
            if cached_only && !up.ready {
                // check status once
                let st = c.magnet_status(up.id).await?;
                if !st.is_ready() {
                    let _ = c.magnet_delete(up.id).await;
                    return Err("AllDebrid: magnet not cached (cached_only)".into());
                }
            }
            if !up.ready {
                let mut ready = false;
                for _ in 0..30 {
                    let st = c.magnet_status(up.id).await?;
                    if st.is_ready() {
                        ready = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                if !ready {
                    return Err("AllDebrid: magnet not ready after polling".into());
                }
            }
            let files = c.magnet_files(up.id).await?;
            if files.is_empty() {
                return Err("AllDebrid: magnet has no files/links".into());
            }
            // file links are hoster links needing unlock
            let mut out = Vec::new();
            for f in files {
                let u = c.unlock(&f.link).await;
                match u {
                    Ok(u) => {
                        if let Some(link) = u.link {
                            out.push(ResolvedFile {
                                name: if f.name.is_empty() {
                                    u.filename
                                } else {
                                    f.name.clone()
                                },
                                size: f.size.map(|n| n.max(0) as u64),
                                url: link,
                                cached: Some(up.ready),
                            });
                        } else if let Some(id) = u.delayed_id {
                            // short poll
                            let mut got = None;
                            for _ in 0..6 {
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                if let Ok(d) = c.delayed(id).await {
                                    if d.is_ready() {
                                        got = d.link;
                                        break;
                                    }
                                }
                            }
                            if let Some(link) = got {
                                out.push(ResolvedFile {
                                    name: f.name.clone(),
                                    size: f.size.map(|n| n.max(0) as u64),
                                    url: link,
                                    cached: Some(up.ready),
                                });
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            if out.is_empty() {
                return Err("AllDebrid: no downloadable files resolved".into());
            }
            Ok(out)
        }
        Provider::Tb => {
            if cached_only {
                let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
                let c0 = crate::debrid_tb::TbClient::new(key);
                if !c0.check_cached(&hash).await?.cached {
                    return Err("TorBox: magnet not cached (cached_only)".into());
                }
            }
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            let created = c.create_torrent(magnet).await?;
            let tid = created.torrent_id;
            if tid == 0 {
                return Err("TorBox: createtorrent returned no id".into());
            }
            let mut files = Vec::new();
            for _ in 0..30 {
                let list = c.torrent_list(Some(tid)).await?;
                if let Some(t) = list.into_iter().next() {
                    if t.download_ready || !t.files.is_empty() && t.progress == Some(1.0) {
                        files = t.files;
                        break;
                    }
                    if !t.files.is_empty() && t.download_ready {
                        files = t.files;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            if files.is_empty() {
                return Err("TorBox: torrent not ready / no files after polling".into());
            }
            let mut out = Vec::new();
            for f in files {
                let link = c.request_torrent_dl(tid, f.id).await?;
                out.push(ResolvedFile {
                    name: f.name,
                    size: Some(f.size),
                    url: link,
                    // torrent_list at poll gives ready state but no
                    // cached flag (checkcached not called here) — unknown.
                    cached: None,
                });
            }
            Ok(out)
        }
    }
}

/// Try providers for hoster URL, ranked: exact/alias domain (score 1.0)
/// first, then fuzzy similarity above `SIMILARITY_THRESHOLD`, up before
/// down, preference order breaks remaining ties. Preference no longer
/// lets a fuzzy/down match jump ahead of another provider's exact match.
/// Only ranked known hosters attempted; normal HTTP URLs must not be
/// hijacked (or burn unrestrict quota). Hosts fetch fails -> skip
/// provider rather than guessing. Failed unrestrict falls to next.
pub async fn resolve_hoster(
    http: reqwest::Client,
    s: &Settings,
    url: &str,
) -> Result<(Provider, ResolvedFile), String> {
    let (r, _) = resolve_hoster_result(http, s, url, "auto").await;
    r
}

/// Hoster resolution, structured failure: `(error, hard)`. `hard` = a
/// provider actually attempted unrestrict and refused (e.g. RD
/// `infringing_file`): caller must surface error, NOT fall back to
/// direct HTTP (which downloads hoster HTML page as fake file).
/// `hard=false`: unsupported / no candidate / hosts check failed.
pub type ResolveHosterResult = (Result<(Provider, ResolvedFile), String>, bool);

/// `choice` = task `debrid_choice` via `providers_for`; `"auto"` =
/// `resolve_hoster`.
pub async fn resolve_hoster_choice(
    http: reqwest::Client,
    s: &Settings,
    url: &str,
    choice: &str,
) -> Result<(Provider, ResolvedFile), String> {
    let (r, _) = resolve_hoster_result(http, s, url, choice).await;
    r
}

pub async fn resolve_hoster_result(
    http: reqwest::Client,
    s: &Settings,
    url: &str,
    choice: &str,
) -> ResolveHosterResult {
    // transfer.it is debrid-unsupported (no provider lists the host):
    // handled natively in `crate::transferit`. Hard-reject so no caller
    // can translate a transfer URL into a provider call by accident.
    if crate::transferit::is_task_url(url) {
        return (Err("transfer.it is not supported by debrid providers".into()), true);
    }
    // Google Drive: direct-only. Hard-reject as backstop so no caller can
    // unrestrict a Drive URL by accident; the dispatcher skips debrid for
    // Drive links entirely (stream.rs gate), so this only fires on direct
    // calls that have no direct-download fallback of their own.
    if is_google_drive_url(url) {
        return (
            Err("Google Drive links are direct-only (not supported by debrid providers)".into()),
            true,
        );
    }
    struct Cand {
        p: Provider,
        m: HostMatch,
        pref: usize,
    }
    let providers = providers_for(s, choice);
    let mut cands: Vec<Cand> = Vec::new();
    let mut last = String::new();
    let mut tried = false;
    for (pref, p) in providers.iter().enumerate() {
        let cfg = p.cfg(s);
        if !cfg.use_for_hosters {
            continue;
        }
        tried = true;
        match hosts(http.clone(), *p, &cfg.api_key).await {
            Ok(list) => match host_match(&list, url) {
                Some(m) => cands.push(Cand { p: *p, m, pref }),
                None => last = format!("{}: hoster unsupported", p.label()),
            },
            Err(e) => last = format!("{}: hosts check failed: {e}", p.label()),
        }
    }
    if !tried {
        return (Err("no debrid provider enabled for hosters".into()), false);
    }
    cands.sort_by(|a, b| {
        b.m.score
            .partial_cmp(&a.m.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.m.up.cmp(&a.m.up))
            .then(a.pref.cmp(&b.pref))
    });
    let mut hard: Option<String> = None;
    for c in cands {
        let cfg = c.p.cfg(s);
        match unrestrict(http.clone(), c.p, &cfg.api_key, url).await {
            Ok(f) => return (Ok((c.p, f)), false),
            Err(e) => {
                let msg = prefixed(c.p.label(), &e);
                if hard.is_none() {
                    hard = Some(msg.clone());
                }
                last = msg;
            }
        }
    }
    if let Some(h) = hard {
        return (Err(h), true);
    }
    if last.is_empty() {
        last = "no enabled debrid provider supports this host".into();
    }
    (Err(last), false)
}

/// Try providers in preference order for magnet.
pub async fn resolve_magnet(
    http: reqwest::Client,
    s: &Settings,
    magnet: &str,
) -> Result<(Provider, Vec<ResolvedFile>), String> {
    resolve_magnet_choice(http, s, magnet, "auto").await
}

/// `choice` = task `debrid_choice` via `providers_for`; `"auto"` =
/// `resolve_magnet`.
pub async fn resolve_magnet_choice(
    http: reqwest::Client,
    s: &Settings,
    magnet: &str,
    choice: &str,
) -> Result<(Provider, Vec<ResolvedFile>), String> {
    let mut last = String::new();
    let mut tried = false;
    for p in providers_for(s, choice) {
        let cfg = p.cfg(s);
        if !cfg.use_for_magnets {
            continue;
        }
        tried = true;
        match magnet_files(http.clone(), p, &cfg.api_key, magnet, cfg.cached_only).await {
            Ok(f) if !f.is_empty() => return Ok((p, f)),
            Ok(_) => last = format!("{}: no files", p.label()),
            Err(e) => last = prefixed(p.label(), &e),
        }
    }
    if !tried {
        return Err("no debrid provider enabled for magnets".into());
    }
    Err(last)
}

/// Download one task via debrid: resolve original URL -> direct link(s),
/// download first/single matching file via engine, restore original URL on
/// returned task. Multi-file magnets: if task filename matches a resolved
/// file name, download that file; else if >1 files, error (Add-dialog
/// fan-out creates one task per file; never silently pick one).
pub async fn download_via_debrid(
    http: reqwest::Client,
    mut task: crate::model::DownloadTask,
    ctrl: std::sync::Arc<crate::engine::Controls>,
    settings: &Settings,
) -> Result<crate::model::DownloadTask, (String, bool)> {
    let original_url = task.url.clone();
    let choice = crate::model::debrid_choice_of(&task);
    let want_file = if !task.debrid_file_name.is_empty() {
        task.debrid_file_name.clone()
    } else {
        task.filename.clone()
    };
    // .torrent source: resolve via debrid as synthesized magnet. Infohash
    // probe failure errors here; start-path caller falls back local.
    let effective_magnet: Option<String> = if is_magnet(&original_url) {
        Some(original_url.clone())
    } else if crate::torrent::is_torrent_task(&task) {
        Some(
            crate::torrent::torrent_magnet(http.clone(), &task, settings)
                .await
                .map_err(|e| (format!("torrent debrid probe: {e}"), false))?,
        )
    } else {
        None
    };
    if let Some(magnet) = &effective_magnet {
        let original_url = original_url.clone();
        let (p, files) = resolve_magnet_choice(http.clone(), settings, magnet, choice)
            .await
            .map_err(|e| (e, false))?;
        let chosen = if files.len() == 1 {
            files.into_iter().next().unwrap()
        } else {
            match files.iter().find(|f| f.name == want_file).cloned() {
                Some(f) => f,
                None => {
                    return Err((format!(
                        "debrid magnet resolved {} files; use Add dialog (debrid fan-out creates one task per file) or local torrent. Files: {}",
                        files.len(),
                        files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
                    ), false))
                }
            }
        };
        // Magnet has no hoster host; token btih hash would be sensitive-ish
        // in UI, so literal "magnet" placeholder keeps format shape.
        // Downloaded stream path restores original URL (match spec).
        let cached = chosen.cached;
        task.url = chosen.url.clone();
        task.debrid_info = format_debrid_info(p.label(), "magnet", cached);
        if let Ok(mut l) = ctrl.live.lock() {
            *l = Some(crate::engine::LiveInfo {
                debrid_info: task.debrid_info.clone(),
                resolved_url: chosen.url.clone(),
                final_url: String::new(),
            });
        }
        let debrid_info = task.debrid_info.clone();
        if !chosen.name.is_empty() {
            task.filename = crate::model::sanitize_filename(&chosen.name);
        }
        let resolved = task.url.clone();
        let mut done = crate::engine::download_http_with_prewarm(
            http,
            task,
            ctrl,
            None,
            settings.max_part_mb * 1024 * 1024,
        )
        .await
        .map_err(|e| (e, false))?;
        done.url = original_url;
        done.debrid_info = debrid_info;
        done.debrided_url = resolved;
        // keep resolved filename (done.filename already correct)
        return Ok(done);
    }
    // hoster URL
    // MEGA folder URLs: fan-out tasks (Add dialog) carry folder-selection
    // per-file links (`/folder/<id>/file/<handle>`, fragment stripped at
    // add time) in `task.url`; single-task adds carry the folder URL.
    // Either way: list the folder, pick the task's file, then resolve
    // THAT file. A URL fragment is a MEGA decryption secret and must
    // never be sent to a provider API. Provider unrestrict of a
    // folder/selection URL returns an arbitrary (first) file, so a
    // provider result whose name differs from the picked file is a hard
    // error — never download the wrong file. With no provider (choice
    // "none" / none configured), download the node directly from MEGA
    // and decrypt while streaming. All other errors are hard so the
    // MEGA web page is never downloaded as a fake file.
    if crate::mega::is_mega_folder_url(&original_url) {
        let folder = match crate::mega::list_folder(http.clone(), &original_url).await {
            Ok(f) => f,
            Err(e) => return Err((format!("MEGA folder: {e}"), true)),
        };
        let picked = match folder.pick(&want_file) {
            Some(f) => f.clone(),
            None => {
                return Err((format!(
                    "MEGA folder has {} files but none named '{}'; re-add via Add dialog to fan out. Files: {}",
                    folder.files.len(),
                    want_file,
                    folder
                        .files
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ), true))
            }
        };
        let provider_url = match picked.link.split('#').next() {
            Some(u) => u.to_string(),
            None => picked.link.clone(),
        };
        let mut provider_err: Option<String> = None;
        // Real-Debrid first: RD rejects the fragment-less per-file
        // selection link via /unrestrict/link (hoster_unsupported); its
        // folder endpoint takes the canonical share link instead. The
        // share link carries the MEGA key fragment: wire-only, never
        // into errors or logs. Any folder-flow failure is soft: record
        // it and fall through to the generic resolver / direct MEGA.
        if choice != "none"
            && providers_for(settings, choice).contains(&Provider::Rd)
            && Provider::Rd.cfg(settings).use_for_hosters
        {
            let rd_cfg = Provider::Rd.cfg(settings);
            'rd_folder: {
                let Some(share_url) = crate::mega::folder_share_url(&original_url) else {
                    break 'rd_folder;
                };
                let rd = crate::debrid_rd::RdClient::new(rd_cfg.api_key.clone(), http.clone());
                let entry = match rd.unrestrict_folder(&share_url).await {
                    Ok(entries) => match pick_rd_folder_link(&entries, &picked.handle, &picked.name)
                    {
                        Some(e) => e.clone(),
                        None => {
                            provider_err = Some(format!(
                                "{}: MEGA folder link matched none of {} folder entries",
                                Provider::Rd.label(),
                                entries.len()
                            ));
                            break 'rd_folder;
                        }
                    },
                    Err(e) => {
                        provider_err = Some(prefixed(Provider::Rd.label(), &e));
                        break 'rd_folder;
                    }
                };
                // A hoster (MEGA) entry link still needs unrestricting;
                // anything else is the resolved download URL already.
                let resolved = if crate::debrid_rd::url_host(&entry.link)
                    .is_some_and(|h| matches!(h.as_str(), "mega.nz" | "mega.co.nz" | "mega.io"))
                {
                    match unrestrict(
                        http.clone(),
                        Provider::Rd,
                        &rd_cfg.api_key,
                        &entry.link,
                    )
                    .await
                    {
                        Ok(f) => f,
                        Err(e) => {
                            provider_err = Some(prefixed(Provider::Rd.label(), &e));
                            break 'rd_folder;
                        }
                    }
                } else {
                    ResolvedFile {
                        name: if entry.filename.is_empty() {
                            picked.name.clone()
                        } else {
                            entry.filename.clone()
                        },
                        size: None,
                        url: entry.link.clone(),
                        cached: None,
                    }
                };
                let f = resolved;
                let p = Provider::Rd;
                if !f.name.is_empty() && f.name != picked.name {
                    return Err((format!(
                        "{}: MEGA folder link resolved to '{}' but this task is '{}'; provider folder handling picked the wrong file",
                        p.label(),
                        f.name,
                        picked.name
                    ), true));
                }
                let host = url_host(&original_url);
                task.url = f.url.clone();
                task.debrid_info = format_debrid_info(p.label(), &host, f.cached);
                if let Ok(mut l) = ctrl.live.lock() {
                    *l = Some(crate::engine::LiveInfo {
                        debrid_info: task.debrid_info.clone(),
                        resolved_url: f.url.clone(),
                        final_url: String::new(),
                    });
                }
                let debrid_info = task.debrid_info.clone();
                if !f.name.is_empty() {
                    task.filename = crate::model::sanitize_filename(&f.name);
                }
                let resolved = f.url.clone();
                let mut done = crate::engine::download_http_with_prewarm(
                    http.clone(),
                    task,
                    ctrl.clone(),
                    None,
                    settings.max_part_mb * 1024 * 1024,
                )
                .await
                .map_err(|e| (e, false))?;
                done.url = original_url;
                done.debrid_info = debrid_info;
                done.debrided_url = resolved;
                return Ok(done);
            }
        }
        if choice != "none" {
            match resolve_hoster_result(http.clone(), settings, &provider_url, choice).await {
                (Ok((p, f)), _) => {
                    if !f.name.is_empty() && f.name != picked.name {
                        return Err((format!(
                            "{}: MEGA folder link resolved to '{}' but this task is '{}'; provider folder handling picked the wrong file",
                            p.label(),
                            f.name,
                            picked.name
                        ), true));
                    }
                    let host = url_host(&original_url);
                    task.url = f.url.clone();
                    task.debrid_info = format_debrid_info(p.label(), &host, f.cached);
                    if let Ok(mut l) = ctrl.live.lock() {
                        *l = Some(crate::engine::LiveInfo {
                            debrid_info: task.debrid_info.clone(),
                            resolved_url: f.url.clone(),
                            final_url: String::new(),
                        });
                    }
                    let debrid_info = task.debrid_info.clone();
                    if !f.name.is_empty() {
                        task.filename = crate::model::sanitize_filename(&f.name);
                    }
                    let resolved = f.url.clone();
                    let mut done = crate::engine::download_http_with_prewarm(
                        http,
                        task,
                        ctrl,
                        None,
                        settings.max_part_mb * 1024 * 1024,
                    )
                    .await
                    .map_err(|e| (e, false))?;
                    done.url = original_url;
                    done.debrid_info = debrid_info;
                    done.debrided_url = resolved;
                    return Ok(done);
                }
                (Err(e), _) => {
                    provider_err = Some(match provider_err {
                        Some(first) if first != e => format!("{first}; {e}"),
                        _ => e,
                    })
                }
            }
        }
        // Direct path: fetch the node's (encrypted) download URL and
        // decrypt after completion. Ticket URLs expire, so engine
        // resume state is keyed by folder+handle, not the ticket URL.
        if let Ok(mut k) = ctrl.state_key.lock() {
            *k = Some(format!("mega:{}:{}", folder.id, picked.handle));
        }
        ctrl.encrypted_payload
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut dl_url = match crate::mega::node_download_url(&http, &folder.id, &picked.handle).await {
            Ok((u, _)) => u,
            Err(e) => {
                ctrl.encrypted_payload
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut k) = ctrl.state_key.lock() {
                    *k = None;
                }
                let msg = match provider_err {
                    Some(pe) => format!("MEGA direct download failed: {e} (provider: {pe})"),
                    None => format!("MEGA direct download failed: {e}"),
                };
                return Err((msg, true));
            }
        };
        task.debrid_info = format_debrid_info("MEGA direct", &url_host(&original_url), None);
        let debrid_info = task.debrid_info.clone();
        task.filename = crate::model::sanitize_filename(&picked.name);
        // Ticket-expiry loop: re-resolve + re-enter on sentinel; parts
        // persist under state_key. Bounded at 4 rounds.
        let mut engine_result = Err("ticket-expired: re-resolve required".to_string());
        let mut task = task;
        for _attempt in 0..4 {
            task.url = dl_url.clone();
            if let Ok(mut l) = ctrl.live.lock() {
                *l = Some(crate::engine::LiveInfo {
                    debrid_info: task.debrid_info.clone(),
                    resolved_url: dl_url.clone(),
                    final_url: String::new(),
                });
            }
            engine_result = crate::engine::download_http_with_prewarm(
                http.clone(),
                task.clone(),
                ctrl.clone(),
                None,
                settings.max_part_mb * 1024 * 1024,
            )
            .await;
            match &engine_result {
                Err(e) if e.contains("ticket-expired") => {
                    match crate::mega::node_download_url(&http, &folder.id, &picked.handle).await {
                        Ok((u, _)) => {
                            dl_url = u;
                            continue;
                        }
                        Err(e2) => {
                            engine_result = Err(e2);
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        ctrl.encrypted_payload
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut k) = ctrl.state_key.lock() {
            *k = None;
        }
        let mut done = engine_result.map_err(|e| (e, false))?;
        // Paused/cancelled: parts stay encrypted for resume; decrypt
        // only on completion (done.filename is post-rename).
        if done.status == crate::model::TaskStatus::Paused {
            done.url = original_url;
            done.debrid_info = debrid_info;
            return Ok(done);
        }
        let out_path = std::path::Path::new(&done.dest_dir).join(&done.filename);
        // Decrypt the completed (still-encrypted) file in place.
        crate::mega::decrypt_file_in_place(&out_path, &picked.key)
            .map_err(|e| (format!("MEGA decryption failed: {e}"), true))?;
        crate::engine::validate_output(&out_path, &done.filename)
            .map_err(|e| (format!("invalid output after decryption: {e}"), true))?;
        done.url = original_url;
        done.debrid_info = debrid_info;
        done.debrided_url = dl_url;
        return Ok(done);
    }
    let (p, f) = match resolve_hoster_result(http.clone(), settings, &original_url, choice).await {
        (Ok(v), _) => v,
        (Err(e), hard) => return Err((e, hard)),
    };
    let host = url_host(&original_url);
    task.url = f.url.clone();
    task.debrid_info = format_debrid_info(p.label(), &host, f.cached);
    if let Ok(mut l) = ctrl.live.lock() {
        *l = Some(crate::engine::LiveInfo {
            debrid_info: task.debrid_info.clone(),
            resolved_url: f.url.clone(),
            final_url: String::new(),
        });
    }
    let debrid_info = task.debrid_info.clone();
    if !f.name.is_empty() {
        task.filename = crate::model::sanitize_filename(&f.name);
    }
    let resolved = f.url.clone();
    let mut done = crate::engine::download_http_with_prewarm(
        http,
        task,
        ctrl,
        None,
        settings.max_part_mb * 1024 * 1024,
    )
    .await
    .map_err(|e| (e, false))?;
    done.url = original_url;
    done.debrid_info = debrid_info;
    done.debrided_url = resolved;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_drive_url_detection() {
        assert!(is_google_drive_url(
            "https://takeout-download-drive.usercontent.google.com/download/drive-download-20261026T143258Z-1-001.zip?j=x&i=0&user=1&authuser=0"
        ));
        assert!(is_google_drive_url(
            "https://drive.google.com/file/d/abc123/view?usp=sharing"
        ));
        assert!(is_google_drive_url(
            "https://drive.usercontent.google.com/download?id=abc"
        ));
        assert!(is_google_drive_url(
            "https://lh3.googleusercontent.com/abc"
        ));
        assert!(!is_google_drive_url("https://example.com/file.zip"));
        assert!(!is_google_drive_url(
            "https://notdrive.google.com.evil.com/file.zip"
        ));
        assert!(!is_google_drive_url("https://google.com/search?q=x"));
        assert!(!is_google_drive_url("magnet:?xt=urn:btih:abc"));
    }

    #[test]
    fn provider_ids_and_order() {
        let mut s = Settings::default();
        assert!(enabled_providers(&s).is_empty());
        s.debrid_rd.enabled = true;
        s.debrid_rd.api_key = "k".into();
        s.debrid_ad.enabled = true;
        s.debrid_ad.api_key = "k".into();
        s.debrid_preferred = "ad".into();
        assert_eq!(enabled_providers(&s), vec![Provider::Ad, Provider::Rd]);
    }

    #[test]
    fn providers_for_choice_override() {
        let mut s = Settings::default();
        assert!(providers_for(&s, "none").is_empty());
        assert!(providers_for(&s, "rd").is_empty());
        s.debrid_rd.enabled = true;
        s.debrid_rd.api_key = "k".into();
        s.debrid_ad.enabled = true;
        s.debrid_ad.api_key = "k".into();
        assert_eq!(providers_for(&s, "none"), Vec::new());
        assert_eq!(providers_for(&s, "rd"), vec![Provider::Rd]);
        // forced provider not configured -> empty (caller falls back)
        assert!(providers_for(&s, "tb").is_empty());
        assert_eq!(providers_for(&s, "auto"), vec![Provider::Rd, Provider::Ad]);
        assert!(!any_for_magnets_choice(&s, "none"));
        assert!(any_for_magnets_choice(&s, "rd"));
        s.debrid_rd.use_for_magnets = false;
        assert!(!any_for_magnets_choice(&s, "rd"));
    }

    #[test]
    fn prefixed_no_double_prefix() {
        assert_eq!(
            prefixed("Real-Debrid", "Real-Debrid: hoster_unsupported (error 16, HTTP 400)"),
            "Real-Debrid: hoster_unsupported (error 16, HTTP 400)"
        );
        assert_eq!(
            prefixed("Real-Debrid", "network request failed"),
            "Real-Debrid: network request failed"
        );
        assert_eq!(
            prefixed("AllDebrid", "AllDebrid: bad apikey"),
            "AllDebrid: bad apikey"
        );
        // A different provider's prefix is not this label's: still prefix.
        assert_eq!(
            prefixed("Real-Debrid", "AllDebrid: oops"),
            "Real-Debrid: AllDebrid: oops"
        );
    }

    #[test]
    fn pick_rd_folder_link_matching() {
        let mk = |link: &str, filename: &str| crate::debrid_rd::RdFolderLink {
            link: link.to_string(),
            filename: filename.to_string(),
        };
        let entries = vec![
            mk("https://download.real-debrid.com/d/A/a.rar", "a.rar"),
            mk("https://mega.nz/folder/X/file/HANDLE2", "b.rar"),
            mk("https://download.real-debrid.com/d/C/c.rar", "c.rar"),
        ];
        // Handle match wins even when names differ.
        assert_eq!(
            pick_rd_folder_link(&entries, "HANDLE2", "nope.rar").map(|e| e.filename.as_str()),
            Some("b.rar")
        );
        // Filename match next.
        assert_eq!(
            pick_rd_folder_link(&entries, "MISSING", "c.rar").map(|e| e.link.as_str()),
            Some("https://download.real-debrid.com/d/C/c.rar")
        );
        // No match with multiple entries -> None (caller falls through).
        assert!(pick_rd_folder_link(&entries, "MISSING", "z.rar").is_none());
        assert!(pick_rd_folder_link(&[], "H", "n.rar").is_none());
        // Sole entry is the fallback.
        let sole = vec![mk("https://download.real-debrid.com/d/A/a.rar", "")];
        assert!(pick_rd_folder_link(&sole, "MISSING", "z.rar").is_some());
    }

    #[test]
    fn expiry_days_and_date() {
        assert_eq!(days_left(Some(86400 * 10), 0), Some(10));
        assert_eq!(days_left(Some(0), 86400 * 2), Some(-2));
        assert_eq!(days_left(None, 0), None);
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(1798761600), "2027-01-01");
    }

    #[test]
    fn host_and_magnet_hash() {
        assert_eq!(
            url_host("https://www.RapidGator.net/file/x"),
            "rapidgator.net"
        );
        assert_eq!(url_host("https://rg.to/x"), "rg.to");
        assert_eq!(
            magnet_hash("magnet:?xt=urn:btih:B38E5B966A72F86834EF0EFB59388D636E82473B&dn=x")
                .as_deref(),
            Some("b38e5b966a72f86834ef0efb59388d636e82473b")
        );
        assert_eq!(magnet_hash("https://example.com"), None);
    }

    #[test]
    fn settings_token_roundtrip() {
        // Token save/load roundtrip through Settings JSON (state.json shape).
        let mut s = Settings::default();
        s.debrid_rd.enabled = true;
        s.debrid_rd.api_key = "SECRET_RD".into();
        s.debrid_rd.cached_only = true;
        s.debrid_tb.enabled = true;
        s.debrid_tb.api_key = "SECRET_TB".into();
        s.debrid_preferred = "tb".into();
        let txt = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&txt).unwrap();
        assert_eq!(back.debrid_rd.api_key, "SECRET_RD");
        assert!(back.debrid_rd.cached_only && back.debrid_rd.enabled);
        assert_eq!(back.debrid_tb.api_key, "SECRET_TB");
        assert_eq!(back.debrid_preferred, "tb");
        // old state.json without debrid fields parses with defaults
        let mut v: serde_json::Value = serde_json::from_str(&txt).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("debrid_rd");
        obj.remove("debrid_ad");
        obj.remove("debrid_tb");
        obj.remove("debrid_preferred");
        let old: Settings = serde_json::from_value(v).unwrap();
        assert!(!old.debrid_rd.enabled && old.debrid_rd.api_key.is_empty());
        assert!(old.debrid_rd.use_for_hosters && old.debrid_rd.use_for_magnets);
    }

    #[test]
    fn debrid_info_format_contract() {
        assert_eq!(
            format_debrid_info("Real-Debrid", "rapidgator.net", Some(true)),
            "Real-Debrid · rapidgator.net · cached"
        );
        assert_eq!(
            format_debrid_info("Real-Debrid", "rapidgator.net", None),
            "Real-Debrid · rapidgator.net · cached: unknown"
        );
        assert_eq!(
            format_debrid_info("AllDebrid", "rg.to", Some(false)),
            "AllDebrid · rg.to · not cached"
        );
        let orig = "https://rapidgator.net/file/b6df95dee983c06f41b3caa31755a9fe/x.part1.rar.html";
        assert_eq!(url_host(orig), "rapidgator.net");
        assert_eq!(
            format_debrid_info("Real-Debrid", &url_host(orig), Some(true)),
            "Real-Debrid · rapidgator.net · cached"
        );
    }

    #[test]
    fn host_up_matching() {
        let hosts = vec![
            HostStatus {
                name: "Rapidgator".into(),
                domain: "rapidgator.net".into(),
                domains: vec!["rapidgator.net".into()],
                up: true,
                supported: true,
            },
            HostStatus {
                name: "Down".into(),
                domain: "down.example".into(),
                domains: vec!["down.example".into()],
                up: false,
                supported: true,
            },
        ];
        assert_eq!(host_up(&hosts, "https://rapidgator.net/file/x"), Some(true));
        assert_eq!(
            host_up(&hosts, "https://www.rapidgator.net/file/x"),
            Some(true)
        );
        assert_eq!(host_up(&hosts, "https://down.example/x"), Some(false));
        assert_eq!(host_up(&hosts, "https://unknown.example/x"), None);
    }

    #[test]
    fn host_up_alias_domains() {
        // RD shape: hoster keyed only by mega.co.nz; user link on mega.nz.
        let rd = vec![HostStatus {
            name: "Mega".into(),
            domain: "mega.co.nz".into(),
            domains: expand_domains(["mega.co.nz".to_string()]),
            up: true,
            supported: true,
        }];
        assert_eq!(
            host_up(&rd, "https://mega.nz/file/dXQTFSqT#ENgUuZqYxmxUWK5tB7ei"),
            Some(true)
        );
        assert_eq!(host_up(&rd, "https://mega.co.nz/file/x#k"), Some(true));
        assert_eq!(host_up(&rd, "https://www.mega.nz/file/x#k"), Some(true));
        // Reverse: listed as mega.nz only, link on mega.co.nz.
        let nz = vec![HostStatus {
            name: "Mega".into(),
            domain: "mega.nz".into(),
            domains: vec!["mega.nz".into()],
            up: true,
            supported: true,
        }];
        assert_eq!(host_up(&nz, "https://mega.co.nz/file/x#k"), Some(true));
        // Rapidgator alias rg.to both directions.
        let rg = vec![HostStatus {
            name: "Rapidgator".into(),
            domain: "rapidgator.net".into(),
            domains: expand_domains(["rapidgator.net".to_string()]),
            up: true,
            supported: true,
        }];
        assert_eq!(host_up(&rg, "https://rg.to/file/x"), Some(true));
        // AD shape: domains list preserved, not just first.
        let ad = vec![HostStatus {
            name: "mega".into(),
            domain: "mega.co.nz".into(),
            domains: expand_domains(["mega.co.nz".to_string(), "mega.nz".to_string()]),
            up: true,
            supported: true,
        }];
        assert_eq!(host_up(&ad, "https://mega.nz/file/x#k"), Some(true));
        // Lookalikes must not match.
        assert_eq!(host_up(&rd, "https://mega.nz.evil.example/x"), None);
        assert_eq!(host_up(&rd, "https://notmega.nz/x"), None);
    }

    #[test]
    fn domain_similarity_routing() {
        // TLD-family split (no alias group needed beyond mega itself).
        assert!(
            domain_similarity("mega.nz", "mega.co.nz") >= SIMILARITY_THRESHOLD,
            "{}",
            domain_similarity("mega.nz", "mega.co.nz")
        );
        assert!(domain_similarity("rapidgator.net", "rapidgator.asia") >= SIMILARITY_THRESHOLD);
        // Lookalikes / different hosters stay below threshold.
        assert!(domain_similarity("notmega.nz", "mega.nz") < SIMILARITY_THRESHOLD);
        assert!(domain_similarity("mega.nz", "megaupload.com") < SIMILARITY_THRESHOLD);
        assert!(domain_similarity("mega.nz", "mega.nz.evil.example") < SIMILARITY_THRESHOLD);
        // Identical long label across suffix = canonical-domain move.
        assert!(domain_similarity("example.com", "example.org") >= SIMILARITY_THRESHOLD);
        // Abbreviations known gap: needs alias group, not fuzzy.
        assert!(domain_similarity("uploaded.net", "ul.to") < SIMILARITY_THRESHOLD);
    }

    #[test]
    fn host_match_scores_and_up_preference() {
        let fuzzy = vec![HostStatus {
            name: "Filefactory".into(),
            domain: "filefactoryy.com".into(),
            domains: vec!["filefactoryy.com".into()],
            up: false,
            supported: true,
        }];
        // Without alias expansion, similarity still ranks candidate;
        // down status kept on match (resolver attempts anyway).
        let m = host_match(&fuzzy, "https://filefactory.com/file/x").unwrap();
        assert!(m.score >= SIMILARITY_THRESHOLD && m.score < 1.0);
        assert!(!m.up);
        // Exact beats fuzzy.
        let exact = vec![HostStatus {
            name: "Mega".into(),
            domain: "mega.nz".into(),
            domains: vec!["mega.nz".into()],
            up: true,
            supported: true,
        }];
        let m = host_match(&exact, "https://mega.nz/file/x#k").unwrap();
        assert_eq!(m.score, 1.0);
        assert!(m.up);
        // User URL from report: RD mega.co.nz-only list matches.
        let rd = vec![HostStatus {
            name: "Mega".into(),
            domain: "mega.co.nz".into(),
            domains: expand_domains(["mega.co.nz".to_string()]),
            up: true,
            supported: true,
        }];
        let m = host_match(
            &rd,
            "https://mega.nz/file/dXQTFSqT#ENgUuZqYxmxUWK5tB7ei-mhSurDLjtVzxAEvNdI1GzM",
        )
        .unwrap();
        assert_eq!(m.score, 1.0);
        assert!(m.up);
        // Unrelated host: no candidate.
        assert!(host_match(&exact, "https://example.com/x").is_none());
    }

    #[test]
    fn account_summary_expired_warns() {
        let a = Account {
            provider: "Real-Debrid",
            username: "bob".into(),
            email: "b@x".into(),
            plan: "premium".into(),
            is_premium: false,
            expires_at: Some(0),
        };
        assert!(a.summary().contains("NOT premium"));
        let b = Account {
            provider: "TorBox",
            username: "a@b".into(),
            email: "a@b".into(),
            plan: "Standard".into(),
            is_premium: true,
            expires_at: Some(1798761600),
        };
        assert!(b.summary().contains("2027-01-01"));
    }
}
