//! transfer.it (`https://transfer.it/t/<xh>`) transfers: MEGA-operated
//! file sharing. A transfer is a public MEGA node tree served through
//! the MEGA API with the transfer handle as `?x=<xh>` context instead
//! of a folder `?n=<id>`; node keys are stored in plain composite-key
//! form:
//! - folder keys: raw 16-byte AES key for its attributes;
//! - file keys: raw 32-byte composite key — attributes use the
//!   XOR-folded AES-128 key (see `crate::mega::decrypt_file_attrs`).
//!
//! Bytes are NOT downloaded via the `a=g` ticket + local decryption:
//! the official web client (`getDownloadLink(n, direct = true)`) uses
//! `GET <apipath>cs/g?x=<xh>&n=<handle>&fn=<urlencoded-name>`, which
//! 302-redirects to a `gfs…/dl/…` URL serving PLAINTEXT (server-side
//! decrypted) bytes with HTTP Range support. Verified live: the first
//! MiB of a real ISO transfer fetched that way is plaintext ISO9660
//! (`CD001` at 32769 after a 32 KiB zero system area); CTR-decrypting
//! it with the node key yields garbage. `Referer: https://transfer.it/t/<xh>`
//! (and `Origin: https://transfer.it`) are required, else `cs/g`
//! 302-redirects back to the transfer landing page.
//!
//! Listing reuses `crate::mega`'s attribute helpers. Debrid providers
//! do not support transfer.it (no provider lists the host); downloads
//! are direct-only in this module, and debrid resolution hard-rejects
//! transfer.it URLs as a backstop. Keys are never logged anywhere in
//! this module.

/// MEGA API endpoint used by the transfer.it app bundle
/// (`apipath`, default `https://bt7.api.mega.co.nz/`).
const API: &str = "https://bt7.api.mega.co.nz/cs";

#[derive(Clone, Debug, PartialEq)]
pub struct TransferFile {
    pub name: String,
    pub size: Option<u64>,
    pub handle: String,
    /// Parent folder handle; the root's handle for top-level files
    /// (root itself is never a file). Path is rebuilt from the tree.
    pub parent: String,
    /// 32-byte composite key (from the node itself, plain form).
    /// Never logged, never persisted.
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub xh: String,
    /// Title from `xi` (`t` field): transfer-level base64 name.
    pub name: String,
    pub files: Vec<TransferFile>,
    /// `pw` present in `xi`: password-protected transfer. The API
    /// rejects listings without a derived `pw` token (`EKEY`);
    /// supported by detecting and failing with a clear message.
    pub passworded: bool,
}

/// Parse `https://transfer.it/t/<xh>` (also `/t/<xh>/...` subpaths
/// and query/fragment suffixes). `xh` is the 12-char transfer handle;
/// accept 8..=24 url-safe chars to be tolerant of future lengths.
pub fn parse_transfer_url(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let (host, after_host) = match rest.find(['/', '#', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "transfer.it" {
        return None;
    }
    let path = after_host.split(['#', '?']).next().unwrap_or("");
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    if segs.next() != Some("t") {
        return None;
    }
    let xh = segs.next()?;
    let ok = (8..=24).contains(&xh.len())
        && xh
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return None;
    }
    Some(xh.to_string())
}

pub fn is_transfer_url(url: &str) -> bool {
    parse_transfer_url(url).is_some()
}

/// Per-file task URL: the transfer URL plus a handle segment
/// (`/t/<xh>/<handle>`). Add fan-out creates one task per file with
/// this URL; at download time the node is picked by handle, so
/// duplicate names and renamed tasks still resolve the right node.
/// Deliberately NOT matched by [`parse_transfer_url`] callers that
/// want the bare transfer — see [`parse_task_url`].
pub fn task_url(xh: &str, handle: &str) -> String {
    format!("https://transfer.it/t/{xh}/{handle}")
}

/// Parse either a bare transfer URL (no selection) or a per-file
/// task URL. Returns `(xh, Option<handle>)`.
pub fn parse_task_url(url: &str) -> Option<(String, Option<String>)> {
    let xh = parse_transfer_url(url)?;
    // Re-scan the path for a third segment (the node handle).
    let url = url.trim();
    let (_, rest) = url.split_once("://")?;
    let after_host = match rest.find('/') {
        Some(i) => &rest[i..],
        None => return Some((xh, None)),
    };
    let path = after_host.split(['#', '?']).next().unwrap_or("");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segs.len() {
        2 => Some((xh, None)),
        3 if !segs[2].is_empty() => Some((xh, Some(segs[2].to_string()))),
        _ => None,
    }
}

pub fn is_task_url(url: &str) -> bool {
    parse_task_url(url).is_some()
}

/// POST one MEGA API batch for transfer `xh`. Returns the decoded
/// first result; API-level negative codes and batch rejections are
/// mapped to stable messages (no handle/key material included).
/// Transient `-3` (EAGAIN) is retried with a short bounded backoff.
async fn api_call(
    http: &reqwest::Client,
    xh: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let api_url = format!("{API}?id=0&x={xh}");
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = http
            .post(&api_url)
            .json(&serde_json::json!([payload.clone()]))
            .send()
            .await;
        let text = match resp {
            Ok(r) => r.text().await.map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        };
        match parse_api_result(&text) {
            Ok(v) => return Ok(v),
            Err(e) if e == "MEGA API error -3" && attempt < 4 => {
                tokio::time::sleep(std::time::Duration::from_millis(
                    250 * u64::from(attempt),
                ))
                .await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn parse_api_result(text: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid MEGA API response: {e}"))?;
    // Batch rejection / error: bare negative number, or [negative].
    let negative = match &value {
        serde_json::Value::Number(n) => n.as_i64().filter(|n| *n < 0),
        serde_json::Value::Array(arr) => match arr.first() {
            Some(serde_json::Value::Number(n)) => n.as_i64().filter(|n| *n < 0),
            _ => None,
        },
        _ => None,
    };
    if let Some(n) = negative {
        return Err(api_error(n));
    }
    value
        .get(0)
        .cloned()
        .ok_or_else(|| "MEGA API response missing result".to_string())
}

fn api_error(code: i64) -> String {
    match code {
        -2 | -16 | -19 => "transfer unavailable (API error)".to_string(),
        -8 => "transfer expired".to_string(),
        -9 => "transfer not found (invalid or expired link)".to_string(),
        _ => format!("MEGA API error {code}"),
    }
}

/// `xi` transfer info: name + password flag.
async fn fetch_info(http: &reqwest::Client, xh: &str) -> Result<(String, bool), String> {
    let obj = api_call(http, xh, serde_json::json!({"a": "xi", "xh": xh})).await?;
    parse_info(&obj)
}

fn parse_info(obj: &serde_json::Value) -> Result<(String, bool), String> {
    let passworded = obj.get("pw").is_some_and(|v| !v.is_null());
    let name = obj
        .get("t")
        .and_then(|t| t.as_str())
        .and_then(crate::mega::b64d)
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| "transfer.it".to_string());
    Ok((name, passworded))
}

/// Raw node list for the transfer (`a=f`, recursive).
async fn fetch_nodes(http: &reqwest::Client, xh: &str) -> Result<serde_json::Value, String> {
    let obj = api_call(http, xh, serde_json::json!({"a": "f", "c": 1, "r": 1})).await?;
    obj.get("f")
        .cloned()
        .ok_or_else(|| "MEGA API response missing file list".to_string())
}

/// List all files of a transfer. Password-protected transfers and
/// empty transfers fail with clear messages. Node-name decryption
/// failures skip that file (never fabricate a name).
pub async fn list_transfer(http: reqwest::Client, url: &str) -> Result<Transfer, String> {
    let xh = parse_transfer_url(url).ok_or_else(|| "not a transfer.it URL".to_string())?;
    list_transfer_xh(&http, &xh).await
}

async fn list_transfer_xh(http: &reqwest::Client, xh: &str) -> Result<Transfer, String> {
    let (name, passworded) = fetch_info(http, xh).await?;
    if passworded {
        return Err(
            "password-protected transfer.it links are not supported yet (remove the password or download from the transfer.it page)"
                .to_string(),
        );
    }
    let nodes_val = fetch_nodes(http, xh).await?;
    let transfer = decrypt_transfer(xh, &name, false, &nodes_val);
    if transfer.files.is_empty() {
        return Err("transfer contains no downloadable files".to_string());
    }
    Ok(transfer)
}

/// Build the file list from the raw `f` node array. Every node's key
/// is used directly (plain form): folders decrypt with their 16-byte
/// key, files with the folded 32-byte composite key — same attribute
/// scheme as MEGA shares, minus the share-key wrapping. A node whose
/// `k` is share-style wrapped (`<owner>:<encrypted>`) cannot be
/// unwrapped without the transfer root key; it is skipped only when
/// its attributes do not decrypt — never silently when they do.
fn decrypt_transfer(xh: &str, name: &str, passworded: bool, nodes: &serde_json::Value) -> Transfer {
    let empty = Vec::new();
    let nodes = nodes.as_array().unwrap_or(&empty);
    // Root key (the `t == 1` node with empty parent): used to unwrap
    // share-style `owner:encrypted` file keys when present.
    let root_key = nodes
        .iter()
        .find(|n| {
            n.get("t").and_then(|t| t.as_u64()) == Some(1)
                && n.get("p").and_then(|p| p.as_str()) == Some("")
        })
        .and_then(|n| n.get("k").and_then(|k| k.as_str()))
        .and_then(crate::mega::b64d)
        .filter(|b| b.len() == 16 || b.len() == 32);
    // Folder paths are not reconstructed: multiple files keep bare
    // names (MEGA folder fan-out does the same; collisions are handled
    // by the task-name gate).
    let mut files = Vec::new();
    let mut skipped_wrapped = 0usize;
    for node in nodes
        .iter()
        .filter(|n| n.get("t").and_then(|t| t.as_u64()) == Some(0))
    {
        let (Some(handle), Some(k), Some(a)) = (
            node.get("h").and_then(|h| h.as_str()),
            node.get("k").and_then(|k| k.as_str()),
            node.get("a").and_then(|a| a.as_str()),
        ) else {
            continue;
        };
        // Candidate keys in order: plain 32-byte composite key; then
        // share-style `<owner>:<enc>` unwrapped under the root key.
        let mut candidates: Vec<Vec<u8>> = Vec::new();
        if let Some(key) = crate::mega::b64d(k).filter(|b| b.len() == 32) {
            candidates.push(key);
        } else if let Some(enc) = k.split_once(':').map(|(_, e)| e).filter(|e| !e.is_empty()) {
            if let (Some(enc_bytes), Some(root)) = (crate::mega::b64d(enc), root_key.as_ref()) {
                if enc_bytes.len() == 32 {
                    if let Some(unwrapped) = crate::mega::unwrap_node_key(&enc_bytes, root) {
                        candidates.push(unwrapped);
                    }
                }
            }
        }
        let mut picked: Option<(Vec<u8>, String)> = None;
        for key in candidates {
            if let Some(fname) = crate::mega::decrypt_file_attrs(a, &key) {
                picked = Some((key, fname));
                break;
            }
        }
        let Some((key, fname)) = picked else {
            skipped_wrapped += usize::from(k.contains(':'));
            continue;
        };
        let parent = node
            .get("p")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        files.push(TransferFile {
            name: fname,
            size: node.get("s").and_then(|s| s.as_u64()),
            handle: handle.to_string(),
            parent,
            key,
        });
    }
    if files.is_empty() && skipped_wrapped > 0 {
        crate::applog::append_default(
            crate::applog::LV_WARN,
            "transfer",
            &format!(
                "transfer.it: {skipped_wrapped} file(s) use wrapped keys that could not be unwrapped"
            ),
        );
    }
    files.sort_by(|a, b| a.name.cmp(&b.name).then(a.handle.cmp(&b.handle)));
    Transfer {
        xh: xh.to_string(),
        name: name.to_string(),
        files,
        passworded,
    }
}

/// Pick the file a task wants: exact handle first (per-file task
/// URL), then exact name, then single-file transfer.
pub fn pick<'a>(
    transfer: &'a Transfer,
    handle: Option<&str>,
    want_name: &str,
) -> Option<&'a TransferFile> {
    if let Some(h) = handle {
        if let Some(f) = transfer.files.iter().find(|f| f.handle == h) {
            return Some(f);
        }
    }
    if let Some(f) = transfer.files.iter().find(|f| f.name == want_name) {
        return Some(f);
    }
    if transfer.files.len() == 1 {
        return transfer.files.first();
    }
    None
}

/// Download one transfer file for `task`: direct download of the
/// plaintext bytes served by `cs/g` (server-side decrypted). Debrid
/// providers do not support transfer.it, so there is no debrid route
/// here. Cancel/pause returns the paused task as-is (parts kept for
/// resume). A hard failure fails the task — the transfer landing page
/// is never downloaded as a fake file. The returned task keeps the
/// original transfer task URL; the resolved `cs/g` URL is in
/// `debrided_url`.
pub async fn download_transfer(
    http: reqwest::Client,
    task: crate::model::DownloadTask,
    ctrl: std::sync::Arc<crate::engine::Controls>,
    settings: &crate::model::Settings,
) -> Result<crate::model::DownloadTask, (String, bool)> {
    let original_url = task.url.clone();
    let (xh, want_handle) =
        parse_task_url(&original_url).ok_or_else(|| ("not a transfer.it URL".to_string(), true))?;
    let want_file = if !task.debrid_file_name.is_empty() {
        task.debrid_file_name.clone()
    } else {
        task.filename.clone()
    };
    let transfer = list_transfer_xh(&http, &xh)
        .await
        .map_err(|e| (format!("transfer.it: {e}"), true))?;
    let picked = match pick(&transfer, want_handle.as_deref(), &want_file) {
        Some(f) => f.clone(),
        None => {
            return Err((format!(
                "transfer.it: {} files but none matches this task; re-add via Add dialog to fan out. Files: {}",
                transfer.files.len(),
                transfer
                    .files
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ), true))
        }
    };
    // Direct-only: plaintext bytes from the `cs/g` redirector.
    match download_direct(http, task, ctrl, settings, &original_url, &xh, &picked).await {
        Ok(done) => Ok(done),
        Err(e) => Err((format!("transfer.it direct download failed: {e}"), true)),
    }
}

/// Direct route: download the plaintext bytes served by the `cs/g`
/// redirector (server-side decrypted, Range-resumable). The engine
/// follows the 302 to the `gfs…/dl/…` URL itself; its probe must carry
/// the transfer Referer or `cs/g` bounces back to the landing page.
/// `Err` = route failed; cancel/pause returns `Ok(Paused)` with parts
/// kept and the task URL restored, so a later resume re-enters here.
async fn download_direct(
    http: reqwest::Client,
    task: crate::model::DownloadTask,
    ctrl: std::sync::Arc<crate::engine::Controls>,
    settings: &crate::model::Settings,
    original_url: &str,
    xh: &str,
    picked: &TransferFile,
) -> Result<crate::model::DownloadTask, String> {
    // Stable resume identity: the resolved `gfs` URL rotates per
    // request, so engine resume state is keyed by transfer+handle.
    if let Ok(mut k) = ctrl.state_key.lock() {
        *k = Some(format!("transferit:{xh}:{}", picked.handle));
    }
    // `cs/g` serves plaintext: the engine's normal magic-sniffing
    // validation applies (no encrypted-payload bypass, no decryption).
    let dl_url = node_download_url(xh, picked);
    let mut task = task;
    // Without this Referer `cs/g` 302s to the landing page and the
    // engine would download HTML as a fake file.
    task.referrer = format!("https://transfer.it/t/{xh}");
    task.headers
        .insert("Origin".to_string(), "https://transfer.it".to_string());
    let debrid_info = crate::debrid::format_debrid_info("transfer.it direct", "transfer.it", None);
    task.debrid_info = debrid_info.clone();
    task.filename = crate::model::sanitize_filename(&picked.name);
    task.url = dl_url.clone();
    if let Ok(mut l) = ctrl.live.lock() {
        *l = Some(crate::engine::LiveInfo {
            debrid_info: task.debrid_info.clone(),
            resolved_url: dl_url.clone(),
            final_url: String::new(),
        });
    }
    let engine_result = crate::engine::download_http_with_prewarm(
        http.clone(),
        task.clone(),
        ctrl.clone(),
        None,
        settings.max_part_mb * 1024 * 1024,
    )
    .await;
    if let Ok(mut k) = ctrl.state_key.lock() {
        *k = None;
    }
    let mut done = engine_result?;
    if done.status == crate::model::TaskStatus::Paused {
        // Cancelled/paused mid-download: parts are plaintext partials;
        // restore the task URL and leave bytes for resume.
        done.url = original_url.to_string();
        return Ok(done);
    }
    if done.status != crate::model::TaskStatus::Complete {
        return Err(if done.error.is_empty() {
            format!("status {:?}", done.status)
        } else {
            done.error.clone()
        });
    }
    // Engine validated the plaintext output itself (magic sniffing was
    // never disabled on this route).
    done.url = original_url.to_string();
    done.debrid_info = debrid_info;
    done.debrided_url = dl_url;
    Ok(done)
}

/// Plaintext download URL for a node via the `cs/g` redirector — the
/// official web client's default link
/// (`getDownloadLink(n, direct = true)`):
/// `<apipath>cs/g?x=<xh>&n=<handle>&fn=<urlencoded-name>`. The GET
/// (carrying the transfer Referer) 302s to a `gfs…/dl/…` URL serving
/// server-decrypted bytes with HTTP Range support. Never log the
/// resolved URL (ticket-bearing).
pub fn node_download_url(xh: &str, file: &TransferFile) -> String {
    // API is ".../cs"; the redirector is the sibling ".../cs/g".
    let base = API.strip_suffix("/cs").unwrap_or(API);
    format!(
        "{base}/cs/g?x={xh}&n={}&fn={}",
        file.handle,
        urlencoding(&file.name)
    )
}

/// Minimal percent-encoding for the `fn=` query value (unreserved
/// bytes pass through, everything else becomes `%XX`).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live-captured fixture (2026-10-26, transfer QqkN235n1J6K):
    // keys/names are public-share material for a single-file ISO
    // transfer; kept as regression data exactly as served.
    fn fixture_nodes() -> serde_json::Value {
        serde_json::json!([
            {"h": "Ly5DkLCD", "p": "", "t": 1, "a": "xirzZ5VTJpo8raehqpIGBjmSGhHnhxb1z3LKcOAw1wmXHpU79b831RZVgMNFwIqA", "k": "eVPqlWQT4ZFixsfTrIstUA", "ts": 1792869772i64},
            {"h": "WvpXVJaA", "p": "Ly5DkLCD", "t": 0, "a": "nPf9vTIxvG_BxIidKRSBwgEhtCafbDENrLntolCWQ-Bn1zC69xe4HLKehbgVtYBi8lH4nvFWYYuoHCy7oI0VrA", "k": "TPEpgreX18iRLIZWiOVHVDGhY3bVtz2tlGn6Bhnz03w", "s": 7403370496u64, "ts": 1792875511i64}
        ])
    }

    #[test]
    fn decrypts_live_fixture() {
        let t = decrypt_transfer(
            "QqkN235n1J6K",
            "thunderbirds.iso",
            false,
            &serde_json::Value::Array(fixture_nodes().as_array().unwrap().clone()),
        );
        assert_eq!(t.files.len(), 1);
        let f = &t.files[0];
        assert_eq!(f.name, "thunderbirds.iso");
        assert_eq!(f.size, Some(7403370496));
        assert_eq!(f.handle, "WvpXVJaA");
        assert_eq!(f.parent, "Ly5DkLCD");
        assert_eq!(f.key.len(), 32);
    }

    #[test]
    fn parses_transfer_urls() {
        assert_eq!(
            parse_transfer_url("https://transfer.it/t/QqkN235n1J6K").as_deref(),
            Some("QqkN235n1J6K")
        );
        assert_eq!(
            parse_transfer_url("https://www.transfer.it/t/QqkN235n1J6K?x=1#y").as_deref(),
            Some("QqkN235n1J6K")
        );
        assert_eq!(
            parse_transfer_url("https://transfer.it/t/QqkN235n1J6K/WvpXVJaA").as_deref(),
            Some("QqkN235n1J6K")
        );
        assert_eq!(parse_transfer_url("https://transfer.it/t/"), None);
        assert_eq!(
            parse_transfer_url("https://transfer.it/x/QqkN235n1J6K"),
            None
        );
        assert_eq!(
            parse_transfer_url("https://example.com/t/QqkN235n1J6K"),
            None
        );
        assert_eq!(
            parse_transfer_url("https://transfer.it/t/QqkN!35n1J6K"),
            None
        );
        assert_eq!(parse_transfer_url("not a url"), None);
        assert!(is_transfer_url("https://transfer.it/t/QqkN235n1J6K"));
    }

    #[test]
    fn parses_task_urls() {
        assert_eq!(
            parse_task_url("https://transfer.it/t/QqkN235n1J6K"),
            Some(("QqkN235n1J6K".to_string(), None))
        );
        assert_eq!(
            parse_task_url("https://transfer.it/t/QqkN235n1J6K/WvpXVJaA"),
            Some(("QqkN235n1J6K".to_string(), Some("WvpXVJaA".to_string())))
        );
        assert_eq!(
            parse_task_url("https://transfer.it/t/QqkN235n1J6K/a/b"),
            None
        );
        assert_eq!(
            task_url("QqkN235n1J6K", "WvpXVJaA"),
            "https://transfer.it/t/QqkN235n1J6K/WvpXVJaA"
        );
        assert!(is_task_url("https://transfer.it/t/QqkN235n1J6K/WvpXVJaA"));
    }

    #[test]
    fn pick_by_handle_name_single() {
        let t = decrypt_transfer("QqkN235n1J6K", "thunderbirds.iso", false, &fixture_nodes());
        assert_eq!(
            pick(&t, Some("WvpXVJaA"), "wrong").map(|f| f.name.as_str()),
            Some("thunderbirds.iso")
        );
        assert_eq!(
            pick(&t, None, "thunderbirds.iso").map(|f| f.handle.as_str()),
            Some("WvpXVJaA")
        );
        assert_eq!(
            pick(&t, None, "anything").map(|f| f.handle.as_str()),
            Some("WvpXVJaA")
        );
        assert_eq!(
            pick(&t, Some("ZZZZZZZZ"), "nope").map(|f| f.handle.as_str()),
            Some("WvpXVJaA")
        );
    }

    #[test]
    fn api_result_errors() {
        assert!(parse_api_result("-9").unwrap_err().contains("not found"));
        assert!(parse_api_result("[-9]").unwrap_err().contains("not found"));
        assert!(parse_api_result("-8").unwrap_err().contains("expired"));
        assert_eq!(
            parse_api_result(r#"[{"a":1}]"#).unwrap(),
            serde_json::json!({"a": 1})
        );
        assert!(parse_api_result("[]").is_err());
        assert!(parse_api_result("garbage").is_err());
    }

    #[test]
    fn wrapped_key_nodes_unwrap_under_root() {
        // Synthetic: take the live fixture's plain file key, wrap it
        // share-style under the root folder key (AES-CBC block scheme),
        // and serve it as `<owner>:<wrapped>`: listing must recover it.
        use cbc::cipher::{block_padding::NoPadding, BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        let plain_key = crate::mega::b64d("TPEpgreX18iRLIZWiOVHVDGhY3bVtz2tlGn6Bhnz03w").unwrap();
        let root_key = crate::mega::b64d("eVPqlWQT4ZFixsfTrIstUA").unwrap();
        let mut enc = Vec::new();
        for chunk in plain_key.chunks(16) {
            let mut buf = chunk.to_vec();
            let rk: &[u8; 16] = root_key.as_slice().try_into().unwrap();
            let out = Aes128CbcEnc::new(rk.into(), (&[0u8; 16]).into())
                .encrypt_padded_mut::<NoPadding>(&mut buf, 16)
                .unwrap()
                .to_vec();
            enc.extend_from_slice(&out);
        }
        let wrapped = format!("OWNERHDL:{}", crate::mega::b64e_nopad(&enc));
        let nodes = serde_json::json!([
            {"h": "Ly5DkLCD", "p": "", "t": 1, "a": "xirzZ5VTJpo8raehqpIGBjmSGhHnhxb1z3LKcOAw1wmXHpU79b831RZVgMNFwIqA", "k": "eVPqlWQT4ZFixsfTrIstUA"},
            {"h": "WvpXVJaA", "p": "Ly5DkLCD", "t": 0, "a": "nPf9vTIxvG_BxIidKRSBwgEhtCafbDENrLntolCWQ-Bn1zC69xe4HLKehbgVtYBi8lH4nvFWYYuoHCy7oI0VrA", "k": wrapped, "s": 7403370496u64}
        ]);
        let t = decrypt_transfer("QqkN235n1J6K", "thunderbirds.iso", false, &nodes);
        assert_eq!(t.files.len(), 1);
        assert_eq!(t.files[0].key, plain_key);
    }

    #[test]
    fn download_url_is_cs_g_plaintext_redirector() {
        // Official web client default: `cs/g?x=&n=&fn=<urlencoded name>`
        // on the bt7 apipath (NOT the `a=g` ticket, which serves
        // ciphertext and must not be decrypted from this route).
        let t = decrypt_transfer("QqkN235n1J6K", "thunderbirds.iso", false, &fixture_nodes());
        let url = node_download_url("QqkN235n1J6K", &t.files[0]);
        assert_eq!(
            url,
            "https://bt7.api.mega.co.nz/cs/g?x=QqkN235n1J6K&n=WvpXVJaA&fn=thunderbirds.iso"
        );
        assert_eq!(urlencoding("a b+c/d?.iso"), "a%20b%2Bc%2Fd%3F.iso");
        assert_eq!(urlencoding("plain-name_1.~x"), "plain-name_1.~x");
    }

    #[test]
    fn info_parse() {
        let (name, pw) = parse_info(&serde_json::json!({"t": "dGh1bmRlcmJpcmRzLmlzbw", "size": [7403370496u64, 1, 1, 0, 0]})).unwrap();
        assert_eq!(name, "thunderbirds.iso");
        assert!(!pw);
        let (_, pw) = parse_info(&serde_json::json!({"t": "eA", "pw": "tok"})).unwrap();
        assert!(pw);
    }
}
