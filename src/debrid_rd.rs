//! Real-Debrid API client (`https://api.real-debrid.com/rest/1.0`).
//!
//! Standalone module: lead wires `pub mod debrid_rd;` into `lib.rs` and adapts
//! these types to the unified debrid trait in `src/debrid.rs`.
//!
//! Conventions (match `update.rs` / `stream.rs`):
//! - reqwest has no `json`/`form` features: POST bodies are form-encoded by
//!   hand (`url::form_urlencoded`), responses parsed via
//!   `serde_json::from_str` on `resp.text()`.
//! - All fallible ops return `Result<_, String>` with user-readable messages.
//! - Token is sent as `Authorization: Bearer …` and never appears in errors
//!   or logs; resolved download URLs are never logged either (reqwest error
//!   strings embed the request URL, so network errors are mapped to a
//!   generic message instead of `e.to_string()`).
//! - Pure `parse_*` functions take response text so they are unit-testable
//!   without network. Async methods are plain: call from worker threads /
//!   tokio tasks, never block the GUI thread on them.
//!
//! API notes (docs + unauthenticated live check, 2026-10-19):
//! - Rate limit 250 req/min; over-limit returns HTTP 429.
//! - Errors: HTTP 4xx/5xx with JSON `{"error": str, "error_code": int?}`.
//! - `GET /hosts` is public and returns a map `domain -> {id, name, image,
//!   image_big}`; `GET /hosts/status` (auth) returns `domain -> {status:
//!   "up"|"down"|…, supported: 0|1, check_time}`. A host is usable iff
//!   `supported == 1 && status == "up"`.
//! - `POST /unrestrict/check` is public; `POST /unrestrict/link` (auth)
//!   returns one link object, or an array for multi-link hosts (YouTube).

use serde::Deserialize;

pub const BASE_URL: &str = "https://api.real-debrid.com/rest/1.0";

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// `GET /user` response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdUser {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    /// `"premium"` or `"free"`.
    #[serde(default, rename = "type")]
    pub account_type: String,
    #[serde(default)]
    pub points: i64,
    /// Premium seconds remaining.
    #[serde(default)]
    pub premium: i64,
    /// ISO-8601 expiry timestamp (empty for free accounts).
    #[serde(default)]
    pub expiration: Option<String>,
}

impl RdUser {
    /// Premium iff account type says so or premium seconds remain.
    /// (`type` alone can lag; `premium` seconds are authoritative.)
    pub fn is_premium(&self) -> bool {
        premium_active(&self.account_type, self.premium)
    }

    /// Whole premium days left, from `premium` seconds (see
    /// [`premium_days_left`]).
    pub fn days_left(&self) -> i64 {
        premium_days_left(self.premium)
    }
}

/// Pure premium check, split out for testing.
pub fn premium_active(account_type: &str, premium_secs: i64) -> bool {
    account_type.eq_ignore_ascii_case("premium") || premium_secs > 0
}

/// Whole days left from premium-seconds-remaining (floor; negative → 0).
pub fn premium_days_left(premium_secs: i64) -> i64 {
    if premium_secs <= 0 {
        0
    } else {
        premium_secs / 86_400
    }
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

/// One entry of `GET /hosts` (public): map key is the domain.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdHost {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub image_big: String,
}

/// One entry of `GET /hosts/status` (auth).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdHostState {
    /// RD sends `0`/`1` integers.
    #[serde(default)]
    pub supported: i64,
    /// `"up"`, `"down"`, or other RD status strings.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub check_time: String,
}

/// Host merged from `/hosts` + `/hosts/status`, keyed by `domain`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RdHostStatus {
    /// Lowercase domain key, e.g. `rapidgator.net` (may itself be a
    /// subdomain, e.g. `down.fast-down.com`).
    pub domain: String,
    pub id: String,
    pub name: String,
    /// Raw status (`"up"`/`"down"`/…); empty when `/hosts/status` lacks it.
    pub status: String,
    pub supported: bool,
    pub check_time: String,
}

impl RdHostStatus {
    /// Usable for unrestricting: RD supports it and it is currently up.
    pub fn is_up(&self) -> bool {
        self.supported && self.status.eq_ignore_ascii_case("up")
    }
}

/// SLD label: second-last dot label, collapsing known multi-label
/// suffixes first (mega.co.nz -> mega, not co).
fn rd_sld(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    for sfx in ["co.nz", "co.uk", "com.au", "co.in", "com.br", "co.za"] {
        if let Some(p) = h.strip_suffix(&format!(".{sfx}")) {
            return p.rsplit('.').next().unwrap_or("").to_string();
        }
    }
    let parts: Vec<&str> = h.split('.').collect();
    if parts.len() >= 2 {
        parts[parts.len() - 2].to_string()
    } else {
        parts.first().copied().unwrap_or("").to_string()
    }
}

/// Merge `/hosts` and `/hosts/status` bodies (either may be the empty-map
/// `{}` shape). Domains present in only one side still appear, with the
/// other side's fields defaulted.
pub fn merge_hosts(hosts_json: &str, status_json: &str) -> Result<Vec<RdHostStatus>, String> {
    let hosts = parse_hosts(hosts_json)?;
    let states = parse_hosts_status(status_json)?;
    // /hosts and /hosts/status may key same hoster under alias domains
    // (mega.nz vs mega.co.nz): fall back to entry sharing SLD label.
    // Exact match wins first.
    let find_state = |domain: &str| -> Option<&RdHostState> {
        states
            .iter()
            .find(|(d, _)| d == domain)
            .or_else(|| states.iter().find(|(d, _)| rd_sld(d) == rd_sld(domain)))
            .map(|(_, s)| s)
    };
    let mut out: Vec<RdHostStatus> = Vec::new();
    for (domain, h) in &hosts {
        let st = find_state(domain);
        out.push(RdHostStatus {
            domain: domain.clone(),
            id: h.id.clone(),
            name: h.name.clone(),
            status: st.map(|s| s.status.clone()).unwrap_or_default(),
            supported: st.map(|s| s.supported != 0).unwrap_or(false),
            check_time: st.map(|s| s.check_time.clone()).unwrap_or_default(),
        });
    }
    for (domain, s) in &states {
        if hosts.iter().any(|(d, _)| d == domain) {
            continue;
        }
        out.push(RdHostStatus {
            domain: domain.clone(),
            id: String::new(),
            name: String::new(),
            status: s.status.clone(),
            supported: s.supported != 0,
            check_time: s.check_time.clone(),
        });
    }
    out.sort_by(|a, b| a.domain.cmp(&b.domain));
    Ok(out)
}

/// Host (domain) portion of a URL, lowercase, `www.` stripped. `None` when
/// unparseable or hostless. Also accepts bare `host/path` strings.
pub fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim())
        .ok()
        .or_else(|| url::Url::parse(&format!("https://{}", url.trim())).ok())?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(strip_www(&host).to_string())
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// Find host entry for a URL: exact domain match, or URL host is a
/// subdomain of a listed domain (`cdn.rapidgator.net` → `rapidgator.net`),
/// case-insensitive, `www.` ignored on both sides.
pub fn find_host<'a>(hosts: &'a [RdHostStatus], url: &str) -> Option<&'a RdHostStatus> {
    let host = url_host(url)?;
    hosts.iter().find(|h| {
        let lower = h.domain.to_ascii_lowercase();
        let d = strip_www(&lower);
        host == d || host.ends_with(&format!(".{d}"))
    })
}

/// True iff URL's host is RD-supported and currently up.
pub fn host_supported(hosts: &[RdHostStatus], url: &str) -> bool {
    find_host(hosts, url).map(|h| h.is_up()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Unrestrict
// ---------------------------------------------------------------------------

/// `POST /unrestrict/check` response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdCheck {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    /// Bytes; `None` when RD omits it.
    #[serde(default)]
    pub filesize: Option<u64>,
    #[serde(default)]
    pub host: String,
    /// Original hoster link, echoed back.
    #[serde(default)]
    pub link: String,
    /// RD sends `0`/`1`.
    #[serde(default)]
    pub supported: i64,
}

impl RdCheck {
    pub fn is_supported(&self) -> bool {
        self.supported != 0
    }
}

/// One link from `POST /unrestrict/folder`. Upstream shape is not
/// verifiable without an API key, so [`parse_unrestrict_folder`] accepts
/// several documented/observed forms and normalizes to this.
// No Debug: holds resolved download URLs, which must never be logged.
#[derive(Clone, Default, PartialEq)]
pub struct RdFolderLink {
    /// Resolved download URL, or the original hoster link when the folder
    /// entry was not yet unrestricted. Sensitive: never log.
    pub link: String,
    /// File name when upstream provides one; empty otherwise.
    pub filename: String,
}

/// Alternative generated link (`alternative[]` in unrestrict response).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdAlternative {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    /// Resolved download URL. Sensitive: never log.
    #[serde(default)]
    pub download: String,
}

/// `POST /unrestrict/link` response (single-link form). Multi-link hosts
/// return a JSON array instead; [`parse_unrestrict_link`] takes the first.
// No Debug: holds resolved download URLs, which must never be logged.
#[derive(Clone, Default, Deserialize)]
pub struct RdDownload {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
    /// Bytes; `None` when RD omits it.
    #[serde(default)]
    pub filesize: Option<u64>,
    /// Original hoster link, echoed back.
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub chunks: i64,
    /// RD sends `0`/`1`.
    #[serde(default)]
    pub crc: i64,
    /// Resolved download URL. Sensitive: never log.
    #[serde(default)]
    pub download: String,
    /// RD sends `0`/`1`.
    #[serde(default)]
    pub streamable: i64,
    #[serde(default)]
    pub alternative: Vec<RdAlternative>,
}

// ---------------------------------------------------------------------------
// Torrents
// ---------------------------------------------------------------------------

/// One file inside [`RdTorrentInfo`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RdTorrentFile {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub bytes: u64,
    /// RD sends `0`/`1`.
    #[serde(default)]
    pub selected: i64,
}

impl RdTorrentFile {
    pub fn is_selected(&self) -> bool {
        self.selected != 0
    }
}

/// `GET /torrents/info/{id}` response. `status` is kept as RD's raw string
/// (`magnet_error`, `magnet_processing`, `waiting_files_selection`,
/// `queued`, `downloading`, `downloaded`, `error`, `virus`, `compressing`,
/// `uploading`) so new RD states don't break parsing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdTorrentInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub original_filename: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub original_bytes: u64,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub split: i64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub added: String,
    #[serde(default)]
    pub files: Vec<RdTorrentFile>,
    /// Resolved download URLs (populated after files are selected).
    /// Sensitive: never log.
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub ended: Option<String>,
    #[serde(default)]
    pub speed: Option<i64>,
    #[serde(default)]
    pub seeders: Option<i64>,
}

impl RdTorrentInfo {
    pub fn is_waiting_files_selection(&self) -> bool {
        self.status == "waiting_files_selection"
    }
    /// Files selected and links generated.
    pub fn is_downloaded(&self) -> bool {
        self.status == "downloaded"
    }
    pub fn is_error(&self) -> bool {
        matches!(self.status.as_str(), "error" | "magnet_error" | "virus")
    }
}

/// `POST /torrents/addMagnet` response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RdAddedTorrent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub uri: String,
}

/// `GET /torrents/instantAvailability/{hash}` result. RD keys the map by
/// hash (case may differ from the request) and nests variants under `rd`;
/// an empty object / missing key means not cached.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RdInstantAvailability {
    /// Hash echoed from the response key (request hash when absent).
    pub hash: String,
    pub cached: bool,
    /// Number of cached file variants across all `rd` entries.
    pub file_count: usize,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// RD error body: `{"error": "…", "error_code": 5}`.
#[derive(Debug, Clone, Default, Deserialize)]
struct RdErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_code: Option<i64>,
}

/// Map an HTTP status + response body to a user-readable message. Never
/// includes the token or any URL.
pub fn format_rd_error(status: u16, body: &str) -> String {
    let parsed: RdErrorBody = serde_json::from_str(body).unwrap_or_default();
    let detail = parsed.error.trim();
    if detail.eq_ignore_ascii_case("disabled_endpoint") {
        return match parsed.error_code {
            Some(code) => format!(
                "Real-Debrid: instantAvailability endpoint disabled upstream (error {code}, HTTP {status})"
            ),
            None => format!(
                "Real-Debrid: instantAvailability endpoint disabled upstream (HTTP {status})"
            ),
        };
    }
    match status {
        401 => "Real-Debrid: bad or expired API token (HTTP 401)".to_string(),
        403 => {
            if detail.is_empty() {
                "Real-Debrid: permission denied — account locked or not premium (HTTP 403)"
                    .to_string()
            } else {
                format!("Real-Debrid: permission denied — {detail} (HTTP 403)")
            }
        }
        404 => "Real-Debrid: not found (HTTP 404)".to_string(),
        429 => "Real-Debrid: rate limited (250 req/min) — wait and retry (HTTP 429)".to_string(),
        503 => {
            if detail.is_empty() {
                "Real-Debrid: file unavailable (HTTP 503)".to_string()
            } else {
                format!("Real-Debrid: {detail} (HTTP 503)")
            }
        }
        _ => {
            if detail.is_empty() {
                format!("Real-Debrid: request failed (HTTP {status})")
            } else if let Some(code) = parsed.error_code {
                format!("Real-Debrid: {detail} (error {code}, HTTP {status})")
            } else {
                format!("Real-Debrid: {detail} (HTTP {status})")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure parsers (response text -> typed values)
// ---------------------------------------------------------------------------

pub fn parse_user(json: &str) -> Result<RdUser, String> {
    serde_json::from_str(json).map_err(|e| format!("Real-Debrid /user parse failed: {e}"))
}

/// Parse `GET /hosts` body into `(domain, host)` pairs, sorted by domain.
pub fn parse_hosts(json: &str) -> Result<Vec<(String, RdHost)>, String> {
    let map: std::collections::BTreeMap<String, RdHost> =
        serde_json::from_str(json).map_err(|e| format!("Real-Debrid /hosts parse failed: {e}"))?;
    Ok(map
        .into_iter()
        .map(|(d, h)| (d.to_ascii_lowercase(), h))
        .collect())
}

/// Parse `GET /hosts/status` body into `(domain, state)` pairs, sorted.
pub fn parse_hosts_status(json: &str) -> Result<Vec<(String, RdHostState)>, String> {
    let map: std::collections::BTreeMap<String, RdHostState> = serde_json::from_str(json)
        .map_err(|e| format!("Real-Debrid /hosts/status parse failed: {e}"))?;
    Ok(map
        .into_iter()
        .map(|(d, s)| (d.to_ascii_lowercase(), s))
        .collect())
}

pub fn parse_unrestrict_check(json: &str) -> Result<RdCheck, String> {
    serde_json::from_str(json)
        .map_err(|e| format!("Real-Debrid /unrestrict/check parse failed: {e}"))
}

/// Parse `POST /unrestrict/link` body. Accepts the single-object form and
/// the array form (multi-link hosts) — first entry wins; check
/// `alternative` on it for the rest of the single-object variant.
pub fn parse_unrestrict_link(json: &str) -> Result<RdDownload, String> {
    let trimmed = json.trim_start();
    if trimmed.starts_with('[') {
        let list: Vec<RdDownload> = serde_json::from_str(json)
            .map_err(|e| format!("Real-Debrid /unrestrict/link parse failed: {e}"))?;
        list.into_iter()
            .next()
            .ok_or_else(|| "Real-Debrid /unrestrict/link returned no links".to_string())
    } else {
        serde_json::from_str(json)
            .map_err(|e| format!("Real-Debrid /unrestrict/link parse failed: {e}"))
    }
}

/// Normalize one `/unrestrict/folder` element: plain string link, or an
/// object with any of `download`/`link`/`url` and `filename`/`name`.
/// Returns `None` when no usable (non-empty) URL is present.
fn folder_link_from_value(v: &serde_json::Value) -> Option<RdFolderLink> {
    if let Some(s) = v.as_str() {
        let link = s.trim().to_string();
        return (!link.is_empty()).then_some(RdFolderLink {
            link,
            filename: String::new(),
        });
    }
    let obj = v.as_object()?;
    let link = ["download", "link", "url"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|x| x.as_str()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let filename = ["filename", "name"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|x| x.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    Some(RdFolderLink { link, filename })
}

/// Parse `POST /unrestrict/folder` body. The exact upstream shape is not
/// verifiable without an API key, so this is defensive: accepts a JSON
/// array of entries or a single object/entry; wrappers such as
/// `{"files": …}`, `{"links": …}`, `{"data": …}`, or an object keyed by
/// id/handle are unwrapped one level. Each entry may be a plain string
/// (a link), an object with any of `download`/`link`/`url` for the URL
/// and `filename`/`name` for the name, or an unrestrict/link-style object
/// (has `download` + `filename` + `host`). Entries with no usable URL are
/// skipped; errors only when nothing usable parses.
pub fn parse_unrestrict_folder(json: &str) -> Result<Vec<RdFolderLink>, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("Real-Debrid /unrestrict/folder parse failed: {e}"))?;
    let items: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Object(o) => {
            let wrapped = ["files", "links", "data"]
                .iter()
                .find_map(|k| o.get(*k))
                .filter(|x| x.is_array() || x.is_object() || x.is_string());
            match wrapped {
                Some(serde_json::Value::Array(a)) => a.iter().collect(),
                Some(other) => vec![other],
                None => {
                    // Either one link object, or a map keyed by id/handle.
                    if folder_link_from_value(&v).is_some() {
                        vec![&v]
                    } else {
                        o.values().collect()
                    }
                }
            }
        }
        other => vec![other],
    };
    let out: Vec<RdFolderLink> = items.iter().filter_map(|i| folder_link_from_value(i)).collect();
    if out.is_empty() {
        return Err("Real-Debrid /unrestrict/folder returned no usable links".to_string());
    }
    Ok(out)
}

pub fn parse_torrent_info(json: &str) -> Result<RdTorrentInfo, String> {
    serde_json::from_str(json).map_err(|e| format!("Real-Debrid /torrents/info parse failed: {e}"))
}

pub fn parse_added_torrent(json: &str) -> Result<RdAddedTorrent, String> {
    serde_json::from_str(json)
        .map_err(|e| format!("Real-Debrid /torrents/addMagnet parse failed: {e}"))
}

/// Parse `GET /torrents/instantAvailability/{hash}` body for `hash`.
/// Shape: `{"<hash>": {"rd": [{"<file_id>": {"filename": …, "filesize": …},
/// …}]}}`; `{}` (or hash absent) means not cached. Parsed via `Value`
/// because file-id keys are dynamic.
pub fn parse_instant_availability(json: &str, hash: &str) -> Result<RdInstantAvailability, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("Real-Debrid instantAvailability parse failed: {e}"))?;
    let mut out = RdInstantAvailability {
        hash: hash.to_string(),
        cached: false,
        file_count: 0,
    };
    let Some(obj) = v.as_object() else {
        return Ok(out);
    };
    // Key case differs between RD responses; match case-insensitively.
    let entry = obj
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(hash))
        .map(|(k, val)| {
            out.hash = k.clone();
            val
        });
    let Some(rd) = entry.and_then(|e| e.get("rd")).and_then(|r| r.as_array()) else {
        return Ok(out);
    };
    for variant in rd {
        if let Some(files) = variant.as_object() {
            out.file_count += files.len();
        }
    }
    out.cached = out.file_count > 0;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Real-Debrid client. Holds the API token; never `Debug` (would leak it).
pub struct RdClient {
    token: String,
    http: reqwest::Client,
}

impl RdClient {
    pub fn new(token: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            token: token.into(),
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{BASE_URL}{path}")
    }

    /// GET with Bearer auth; returns body text or a mapped RD error.
    /// Network failures map to a generic message: reqwest error strings
    /// embed the request URL, which must not surface (resolved URLs).
    async fn get_text(&self, path: &str) -> Result<String, String> {
        let resp = self
            .http
            .get(self.url(path))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .send()
            .await
            .map_err(|_| "Real-Debrid: network request failed".to_string())?;
        read_body(resp).await
    }

    /// POST form-encoded fields with Bearer auth. `fields` values may hold
    /// hoster/magnet URLs: they only go on the wire, never into errors.
    async fn post_form_text(&self, path: &str, fields: &[(&str, &str)]) -> Result<String, String> {
        let mut body = String::new();
        for (k, v) in fields {
            if !body.is_empty() {
                body.push('&');
            }
            body.extend(url::form_urlencoded::byte_serialize(k.as_bytes()));
            body.push('=');
            body.extend(url::form_urlencoded::byte_serialize(v.as_bytes()));
        }
        let resp = self
            .http
            .post(self.url(path))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| "Real-Debrid: network request failed".to_string())?;
        read_body(resp).await
    }

    /// `GET /user` — account info incl. premium seconds left.
    pub async fn user(&self) -> Result<RdUser, String> {
        parse_user(&self.get_text("/user").await?)
    }

    /// Merged hosts: `GET /hosts` (names) + `GET /hosts/status` (up /
    /// supported). Sorted by domain; filter with [`RdHostStatus::is_up`]
    /// or match URLs via [`host_supported`].
    pub async fn hosts_status(&self) -> Result<Vec<RdHostStatus>, String> {
        let hosts = self.get_text("/hosts").await?;
        let status = self.get_text("/hosts/status").await?;
        merge_hosts(&hosts, &status)
    }

    /// `POST /unrestrict/check` — is this hoster link downloadable, and
    /// what filename/size? (Endpoint is public upstream; token sent anyway.)
    pub async fn unrestrict_check(&self, url: &str) -> Result<RdCheck, String> {
        let text = self
            .post_form_text("/unrestrict/check", &[("link", url)])
            .await?;
        parse_unrestrict_check(&text)
    }

    /// `POST /unrestrict/link` — resolve a hoster link. Returned
    /// `RdDownload.download` is a signed resolved URL: never log it.
    pub async fn unrestrict_link(&self, url: &str) -> Result<RdDownload, String> {
        let text = self
            .post_form_text("/unrestrict/link", &[("link", url)])
            .await?;
        parse_unrestrict_link(&text)
    }

    /// Convenience: just the resolved download URL from
    /// [`Self::unrestrict_link`]. Sensitive: never log the result.
    pub async fn unrestrict_download_url(&self, url: &str) -> Result<String, String> {
        Ok(self.unrestrict_link(url).await?.download)
    }

    /// `POST /unrestrict/folder` — list/unrestrict a hoster folder link
    /// (e.g. a MEGA folder share). Folder links that need a decryption
    /// fragment carry it only on the wire in the `link` field; never log
    /// `url` or the returned links.
    pub async fn unrestrict_folder(&self, url: &str) -> Result<Vec<RdFolderLink>, String> {
        let text = self
            .post_form_text("/unrestrict/folder", &[("link", url)])
            .await?;
        parse_unrestrict_folder(&text)
    }

    /// `GET /torrents/instantAvailability/{hash}` — is this torrent hash
    /// already cached on RD (instant download, no waiting)?
    pub async fn instant_availability(&self, hash: &str) -> Result<RdInstantAvailability, String> {
        let hash = hash.trim();
        if hash.is_empty() {
            return Err("Real-Debrid: empty torrent hash".to_string());
        }
        let text = self
            .get_text(&format!("/torrents/instantAvailability/{hash}"))
            .await?;
        parse_instant_availability(&text, hash)
    }

    /// Cached check only: see [`Self::instant_availability`].
    pub async fn is_cached(&self, hash: &str) -> Result<bool, String> {
        Ok(self.instant_availability(hash).await?.cached)
    }

    /// `POST /torrents/addMagnet` — returns the RD torrent id. Then poll
    /// [`Self::torrent_info`] until `waiting_files_selection`, call
    /// [`Self::select_files`], and poll again for `links`.
    pub async fn add_magnet(&self, magnet: &str) -> Result<String, String> {
        let text = self
            .post_form_text("/torrents/addMagnet", &[("magnet", magnet)])
            .await?;
        let added = parse_added_torrent(&text)?;
        if added.id.is_empty() {
            return Err("Real-Debrid /torrents/addMagnet returned no id".to_string());
        }
        Ok(added.id)
    }

    /// `GET /torrents/info/{id}`.
    pub async fn torrent_info(&self, id: &str) -> Result<RdTorrentInfo, String> {
        let text = self.get_text(&format!("/torrents/info/{id}")).await?;
        parse_torrent_info(&text)
    }

    /// `POST /torrents/selectFiles/{id}`. `files` is RD's selector:
    /// `"all"` or comma-separated file ids (e.g. `"1,3"`) as in
    /// [`RdTorrentFile::id`]. Succeeds with an empty 204 body.
    pub async fn select_files(&self, id: &str, files: &str) -> Result<(), String> {
        self.post_form_text(&format!("/torrents/selectFiles/{id}"), &[("files", files)])
            .await?;
        Ok(())
    }

    /// `DELETE /torrents/delete/{id}` (204 on success).
    pub async fn delete_torrent(&self, id: &str) -> Result<(), String> {
        let resp = self
            .http
            .delete(self.url(&format!("/torrents/delete/{id}")))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .send()
            .await
            .map_err(|_| "Real-Debrid: network request failed".to_string())?;
        read_body(resp).await?;
        Ok(())
    }
}

/// Success → body text; failure → [`format_rd_error`]. Covers 204 (empty).
async fn read_body(resp: reqwest::Response) -> Result<String, String> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|_| "Real-Debrid: failed reading response body".to_string())?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(format_rd_error(status.as_u16(), &text))
    }
}

// ---------------------------------------------------------------------------
// Tests (recorded/documented shapes; no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const USER_PREMIUM: &str = r#"{
        "id": 123456,
        "username": "someuser",
        "email": "user@example.com",
        "points": 450,
        "locale": "en",
        "avatar": "https://fcdn.real-debrid.com/0840/images/avatars/guest.png",
        "type": "premium",
        "premium": 2592000,
        "expiration": "2026-11-18T10:00:00.000Z"
    }"#;

    const USER_EXPIRED: &str = r#"{
        "id": 123456,
        "username": "someuser",
        "email": "user@example.com",
        "points": 0,
        "locale": "en",
        "avatar": "",
        "type": "free",
        "premium": 0,
        "expiration": "2020-01-01T00:00:00.000Z"
    }"#;

    const HOSTS: &str = r#"{
        "rapidgator.net": {"id": "rg", "name": "RapidGator", "image": "https://x/rg.png", "image_big": "https://x/rg_big.png"},
        "nitroflare.com": {"id": "nf", "name": "Nitroflare", "image": "", "image_big": ""},
        "1fichier.com": {"id": "1f", "name": "1Fichier", "image": "", "image_big": ""}
    }"#;

    const HOSTS_STATUS: &str = r#"{
        "rapidgator.net": {"supported": 1, "status": "up", "check_time": "2026-10-19T08:00:00.000Z"},
        "nitroflare.com": {"supported": 1, "status": "down", "check_time": "2026-10-19T08:00:00.000Z"},
        "1fichier.com": {"supported": 0, "status": "up", "check_time": "2026-10-19T08:00:00.000Z"}
    }"#;

    #[test]
    fn parse_user_premium() {
        let u = parse_user(USER_PREMIUM).unwrap();
        assert_eq!(u.username, "someuser");
        assert_eq!(u.email, "user@example.com");
        assert_eq!(u.account_type, "premium");
        assert_eq!(u.premium, 2_592_000);
        assert_eq!(u.expiration.as_deref(), Some("2026-11-18T10:00:00.000Z"));
        assert!(u.is_premium());
        assert_eq!(u.days_left(), 30);
    }

    #[test]
    fn parse_user_expired_free() {
        let u = parse_user(USER_EXPIRED).unwrap();
        assert_eq!(u.account_type, "free");
        assert!(!u.is_premium());
        assert_eq!(u.days_left(), 0);
        // Type lags but seconds remain -> still premium; type alone counts too.
        assert!(premium_active("free", 60));
        assert!(premium_active("premium", 0));
        assert!(!premium_active("free", 0));
        assert_eq!(premium_days_left(90_000), 1);
        assert_eq!(premium_days_left(-5), 0);
    }

    #[test]
    fn merge_hosts_up_down_unsupported() {
        let hosts = merge_hosts(HOSTS, HOSTS_STATUS).unwrap();
        assert_eq!(hosts.len(), 3);
        let rg = hosts.iter().find(|h| h.domain == "rapidgator.net").unwrap();
        assert_eq!(rg.name, "RapidGator");
        assert!(rg.is_up());
        let nf = hosts.iter().find(|h| h.domain == "nitroflare.com").unwrap();
        assert!(nf.supported && !nf.is_up(), "down host not usable");
        let f = hosts.iter().find(|h| h.domain == "1fichier.com").unwrap();
        assert!(!f.supported && !f.is_up(), "unsupported host not usable");
        // Status-only / hosts-only entries still surface.
        let merged = merge_hosts(HOSTS, "{}").unwrap();
        assert!(merged.iter().all(|h| !h.is_up()));
        assert!(merge_hosts("{}", HOSTS_STATUS)
            .unwrap()
            .iter()
            .find(|h| h.domain == "rapidgator.net")
            .unwrap()
            .is_up());
    }

    #[test]
    fn merge_hosts_alias_domain_status() {
        // /hosts keys mega.nz, /hosts/status keys mega.co.nz (alias split):
        // status must still join, else Mega reads down/unsupported and
        // resolver skips RD for mega.nz links.
        let hosts = r#"{"mega.nz":{"id":"mega","name":"Mega","icon":"i"}}"#;
        let status = r#"{"mega.co.nz":{"supported":1,"status":"up","check_time":"2026-10-19T08:00:00.000Z"}}"#;
        let m = merge_hosts(hosts, status).unwrap();
        let mega = m.iter().find(|h| h.domain == "mega.nz").unwrap();
        assert_eq!(mega.status, "up");
        assert!(mega.supported);
        assert!(mega.is_up());
        // Different hoster sharing no SLD: no join.
        let hosts2 = r#"{"rapidgator.net":{"id":"rg","name":"RG","icon":"i"}}"#;
        let status2 = r#"{"mega.co.nz":{"supported":1,"status":"up"}}"#;
        let m2 = merge_hosts(hosts2, status2).unwrap();
        let rg = m2.iter().find(|h| h.domain == "rapidgator.net").unwrap();
        assert_eq!(rg.status, "");
        assert!(!rg.is_up());
    }

    #[test]
    fn host_matching_rapidgator() {
        let hosts = merge_hosts(HOSTS, HOSTS_STATUS).unwrap();
        let url = "https://rapidgator.net/file/b6df95dee983c06f41b3caa31755a9fe/Cas1tlevBelm7ontsCur4se-elamigos.part1.rar.html";
        assert!(host_supported(&hosts, url));
        assert_eq!(find_host(&hosts, url).unwrap().name, "RapidGator");
        // Case-insensitive, www prefix, subdomain, bare host/path.
        assert!(host_supported(&hosts, "https://WWW.RapidGator.NET/file/x"));
        assert!(host_supported(&hosts, "https://www.rapidgator.net/file/x"));
        assert!(host_supported(&hosts, "https://cdn.rapidgator.net/file/x"));
        assert!(host_supported(&hosts, "rapidgator.net/file/x"));
        // Down / unsupported / unknown hosts never supported.
        assert!(!host_supported(&hosts, "https://nitroflare.com/file/x"));
        assert!(!host_supported(&hosts, "https://1fichier.com/?x"));
        assert!(!host_supported(&hosts, "https://example.org/file/x"));
        assert!(!host_supported(&hosts, "not a url at all ::"));
        assert_eq!(
            url_host("https://WWW.Example.COM/a?b=1"),
            Some("example.com".into())
        );
    }

    #[test]
    fn parse_unrestrict_check_and_link() {
        let check = parse_unrestrict_check(
            r#"{"id":"ABC123","filename":"file.part1.rar","filesize":3146063544,"host":"rapidgator.net","link":"https://rapidgator.net/file/x/file.part1.rar.html","supported":1}"#,
        )
        .unwrap();
        assert_eq!(check.filename, "file.part1.rar");
        assert_eq!(check.filesize, Some(3_146_063_544));
        assert!(check.is_supported());

        let dl = parse_unrestrict_link(
            r#"{"id":"ABC123","filename":"file.part1.rar","mimeType":"application/x-rar-compressed","filesize":3146063544,"link":"https://rapidgator.net/file/x/file.part1.rar.html","host":"rapidgator.net","chunks":16,"crc":1,"download":"https://download.real-debrid.com/d/XYZ/file.part1.rar","streamable":0}"#,
        )
        .unwrap();
        assert_eq!(
            dl.download,
            "https://download.real-debrid.com/d/XYZ/file.part1.rar"
        );
        assert_eq!(dl.filename, "file.part1.rar");
        assert_eq!(dl.mime_type, "application/x-rar-compressed");
        assert_eq!(dl.host, "rapidgator.net");

        // Array form (multi-link hosts): first entry wins.
        let multi = parse_unrestrict_link(
            r#"[{"id":"A","filename":"a.mp4","download":"https://download.real-debrid.com/d/A/a.mp4"},{"id":"B","filename":"b.mp4","download":"https://download.real-debrid.com/d/B/b.mp4"}]"#,
        )
        .unwrap();
        assert_eq!(multi.id, "A");
        assert!(parse_unrestrict_link("[]").is_err());
    }

    #[test]
    fn parse_folder_array_of_link_objects() {
        let links = parse_unrestrict_folder(
            r#"[{"download":"https://download.real-debrid.com/d/A/a.rar","filename":"a.rar","host":"mega.nz"},{"link":"https://download.real-debrid.com/d/B/b.rar","name":"b.rar"},{"url":"https://example.com/c.rar","filename":"c.rar"}]"#,
        )
        .unwrap();
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].link, "https://download.real-debrid.com/d/A/a.rar");
        assert_eq!(links[0].filename, "a.rar");
        assert_eq!(links[1].link, "https://download.real-debrid.com/d/B/b.rar");
        assert_eq!(links[1].filename, "b.rar");
        assert_eq!(links[2].link, "https://example.com/c.rar");
        assert_eq!(links[2].filename, "c.rar");
    }

    #[test]
    fn parse_folder_plain_strings_and_single_forms() {
        // Plain-string array entries.
        let links = parse_unrestrict_folder(
            r#"["https://download.real-debrid.com/d/A/a.rar","https://mega.nz/file/H#KEY"]"#,
        )
        .unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].link, "https://download.real-debrid.com/d/A/a.rar");
        assert!(links[0].filename.is_empty());
        assert_eq!(links[1].link, "https://mega.nz/file/H#KEY");
        // Single object (not an array).
        let one = parse_unrestrict_folder(
            r#"{"download":"https://download.real-debrid.com/d/A/a.rar","filename":"a.rar"}"#,
        )
        .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].filename, "a.rar");
    }

    #[test]
    fn parse_folder_unrestrict_style_and_wrappers() {
        // unrestrict/link-style object in an array.
        let links = parse_unrestrict_folder(
            r#"[{"id":"ABC123","filename":"file.part1.rar","mimeType":"application/x-rar-compressed","filesize":3146063544,"link":"https://mega.nz/folder/X/file/H","host":"mega.nz","chunks":16,"crc":1,"download":"https://download.real-debrid.com/d/XYZ/file.part1.rar","streamable":0}]"#,
        )
        .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].link,
            "https://download.real-debrid.com/d/XYZ/file.part1.rar"
        );
        assert_eq!(links[0].filename, "file.part1.rar");
        // Wrapped arrays.
        let wrapped = parse_unrestrict_folder(
            r#"{"files":[{"download":"https://download.real-debrid.com/d/A/a.rar","filename":"a.rar"}]}"#,
        )
        .unwrap();
        assert_eq!(wrapped.len(), 1);
        // Object keyed by id/handle.
        let keyed = parse_unrestrict_folder(
            r#"{"H1":{"download":"https://download.real-debrid.com/d/A/a.rar","filename":"a.rar"},"H2":"https://download.real-debrid.com/d/B/b.rar"}"#,
        )
        .unwrap();
        assert_eq!(keyed.len(), 2);
    }

    #[test]
    fn parse_folder_skips_unusable_and_errors_when_empty() {
        let links = parse_unrestrict_folder(
            r#"[{"filename":"no-url.rar"},{"download":"  "},42,null,"https://download.real-debrid.com/d/A/a.rar"]"#,
        )
        .unwrap();
        assert_eq!(links.len(), 1);
        assert!(parse_unrestrict_folder("[]").is_err());
        assert!(parse_unrestrict_folder("{}").is_err());
        assert!(parse_unrestrict_folder(r#"[{"filename":"only-a-name"}]"#).is_err());
        assert!(parse_unrestrict_folder("not json").is_err());
        assert!(parse_unrestrict_folder("").is_err());
    }

    #[test]
    fn parse_instant_availability_cached_and_not() {
        let hash = "B38E5B966A72F86834EF0EFB59388D636E82473B";
        let cached = parse_instant_availability(
            r#"{"b38e5b966a72f86834ef0efb59388d636e82473b":{"rd":[{"1":{"filename":"movie.mkv","filesize":1500000000},"2":{"filename":"sample.mkv","filesize":50000000}}]}}"#,
            hash,
        )
        .unwrap();
        assert!(cached.cached);
        assert_eq!(cached.file_count, 2);
        // Hash key case differs from request: still matched, echoed as-is.
        assert_eq!(cached.hash, "b38e5b966a72f86834ef0efb59388d636e82473b");

        let empty = parse_instant_availability("{}", hash).unwrap();
        assert!(!empty.cached && empty.file_count == 0);
        let no_rd = parse_instant_availability(
            r#"{"B38E5B966A72F86834EF0EFB59388D636E82473B":{"rd":[]}}"#,
            hash,
        )
        .unwrap();
        assert!(!no_rd.cached);
    }

    #[test]
    fn parse_torrent_info_multi_file() {
        let info = parse_torrent_info(
            r#"{
                "id": "TORRENTID1",
                "filename": "Some.Release.1080p",
                "original_filename": "Some.Release.1080p",
                "hash": "b38e5b966a72f86834ef0efb59388d636e82473b",
                "bytes": 1600000000,
                "original_bytes": 1650000000,
                "host": "rapidgator.net",
                "split": 999,
                "progress": 100,
                "status": "downloaded",
                "added": "2026-10-19T08:00:00.000Z",
                "files": [
                    {"id": 1, "path": "/movie.mkv", "bytes": 1500000000, "selected": 1},
                    {"id": 2, "path": "/sample.mkv", "bytes": 50000000, "selected": 0},
                    {"id": 3, "path": "/subs.srt", "bytes": 80000, "selected": 1}
                ],
                "links": ["https://download.real-debrid.com/t/TORRENTID1/1"],
                "ended": "2026-10-19T08:05:00.000Z",
                "speed": 0,
                "seeders": 12
            }"#,
        )
        .unwrap();
        assert_eq!(info.id, "TORRENTID1");
        assert_eq!(info.files.len(), 3);
        assert!(info.files[0].is_selected());
        assert!(!info.files[1].is_selected());
        assert_eq!(info.files.iter().filter(|f| f.is_selected()).count(), 2);
        assert_eq!(info.links.len(), 1);
        assert!(info.is_downloaded());
        assert!(!info.is_waiting_files_selection());
        assert!(!info.is_error());

        let waiting = parse_torrent_info(
            r#"{"id":"T2","filename":"","hash":"abc","bytes":0,"progress":0,"status":"waiting_files_selection","files":[],"links":[]}"#,
        )
        .unwrap();
        assert!(waiting.is_waiting_files_selection());
    }

    #[test]
    fn disabled_endpoint_distinct_message() {
        let err = format_rd_error(403, r#"{"error":"disabled_endpoint","error_code":37}"#);
        assert!(
            err.contains("instantAvailability endpoint disabled upstream"),
            "{err}"
        );
        assert!(err.contains("error 37"), "{err}");
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(!err.contains("account locked"), "{err}");
        assert!(!err.contains("permission"), "{err}");
        // Any status, case-insensitive.
        let err2 = format_rd_error(400, r#"{"error":"Disabled_Endpoint"}"#);
        assert!(err2.contains("disabled upstream"), "{err2}");
        assert!(err2.contains("HTTP 400"), "{err2}");
    }

    #[test]
    fn parse_added_torrent_and_error_shape() {
        let added = parse_added_torrent(
            r#"{"id":"TORRENTID1","uri":"https://api.real-debrid.com/rest/1.0/torrents/info/TORRENTID1"}"#,
        )
        .unwrap();
        assert_eq!(added.id, "TORRENTID1");

        assert_eq!(
            format_rd_error(401, r#"{"error":"bad_token","error_code":8}"#),
            "Real-Debrid: bad or expired API token (HTTP 401)"
        );
        assert!(
            format_rd_error(403, r#"{"error":"permission denied"}"#).contains("permission denied")
        );
        assert!(
            format_rd_error(429, r#"{"error":"too_many_requests","error_code":34}"#)
                .contains("rate limited")
        );
        assert_eq!(
            format_rd_error(400, r#"{"error":"missing_parameter","error_code":5}"#),
            "Real-Debrid: missing_parameter (error 5, HTTP 400)"
        );
        // Non-JSON / empty bodies still produce readable messages.
        assert_eq!(
            format_rd_error(500, ""),
            "Real-Debrid: request failed (HTTP 500)"
        );
        assert!(format_rd_error(503, "oops").contains("file unavailable"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_user("not json").is_err());
        assert!(parse_hosts("[]").is_err());
        assert!(parse_torrent_info("{}").is_ok(), "empty object defaults");
    }
}
