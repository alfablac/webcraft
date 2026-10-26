use crate::engine;
use crate::hls;
use crate::model::*;
use crate::mpd::*;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
enum Manifest {
    Dash,
    Hls,
    Metalink,
    File,
}

fn is_metalink_url(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".meta4") || path.ends_with(".metalink")
}

/// Path portion (before `?`/`#`) ends with any suffix, case-insensitive.
/// Query tokens never count: `watch?f=.mpd` is not a manifest.
fn url_path_ends(url: &str, suffixes: &[&str]) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    suffixes.iter().any(|s| path.ends_with(s))
}

/// Manifest kind implied by URL alone: path ends `.mpd`/`.m3u8`, or query
/// names a packaged playlist (Reddit `.../x.mp4?m=DASHPlaylist.mpd`, whose
/// path ends `.mp4`). Substring-in-query (`.mpd` as a token value) is not
/// enough.
fn manifest_kind_from_url(url: &str) -> Option<Manifest> {
    let lower = url.to_ascii_lowercase();
    let query = lower
        .split_once('?')
        .map(|(_, q)| q.split('#').next().unwrap_or(q))
        .unwrap_or("");
    if url_path_ends(url, &[".mpd"]) || query.contains("dashplaylist") {
        Some(Manifest::Dash)
    } else if url_path_ends(url, &[".m3u8"]) || query.contains("hlsplaylist") {
        Some(Manifest::Hls)
    } else {
        None
    }
}

/// Segment-only URL (single `.ts`/`.m4s` part, no manifest) must not plain-
/// download into an orphan "complete" file. Only context that rescues it:
/// `x-page-url` pointing at a manifest itself. Returns rejection reason.
fn segment_only_reject(task: &DownloadTask) -> Option<String> {
    // Only .m4s is a bare fragment (no init, unplayable alone). Bare .ts is
    // a self-contained stream: allow through to probe.
    if !url_path_ends(&task.url, &[".m4s"]) {
        return None;
    }
    let has_manifest_ctx = task
        .headers
        .get("x-page-url")
        .map(|pu| manifest_kind_from_url(pu).is_some())
        .unwrap_or(false);
    if has_manifest_ctx {
        return None;
    }
    Some(format!(
        "segment-only URL has no manifest context (single .ts/.m4s part is not a complete file); download the parent manifest (.m3u8/.mpd) or page instead; url={}",
        task.url
    ))
}

/// `.nzb` URL/local path, `nzb://` wrapper, or explicit `x-nzb` header marker.
/// True when URL needs manifest/stream probe (DASH/HLS/Metalink) — prewarm gate.
pub fn is_manifest_url(url: &str) -> bool {
    manifest_kind_from_url(url).is_some()
}

pub fn is_nzb_url(task: &DownloadTask) -> bool {
    if task.headers.contains_key("x-nzb") {
        return true;
    }
    let url = task.url.trim();
    if url.starts_with("nzb://") {
        return true;
    }
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://"))
        && std::path::Path::new(url).is_file()
    {
        return url.to_ascii_lowercase().ends_with(".nzb");
    }
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase()
        .ends_with(".nzb")
}

pub fn is_nzb_url_str(url: &str) -> bool {
    is_nzb_url(&DownloadTask {
        url: url.trim().to_string(),
        ..Default::default()
    })
}

/// Download protocol, derived from task URL/headers. Label source for list UI:
/// mirrors dispatch order in `download_url_with_settings`.
/// ponytail: derived, not persisted — metalink primary-mirror rewrite and
/// Content-Type-only streams lose exact kind after run; persist field if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtoKind {
    Torrent,
    Ftp,
    Ftps,
    Sftp,
    Nntp,
    Metalink,
    Dash,
    Hls,
    YtDlp,
    #[default]
    Http,
    File,
}

impl ProtoKind {
    pub fn label(self) -> &'static str {
        match self {
            ProtoKind::Torrent => "TORRENT",
            ProtoKind::Ftp => "FTP",
            ProtoKind::Ftps => "FTPS",
            ProtoKind::Sftp => "SFTP",
            ProtoKind::Nntp => "NNTP",
            ProtoKind::Metalink => "METALINK",
            ProtoKind::Dash => "DASH",
            ProtoKind::Hls => "HLS",
            ProtoKind::YtDlp => "YT-DLP",
            ProtoKind::Http => "HTTP",
            ProtoKind::File => "FILE",
        }
    }
    pub fn full_name(self) -> &'static str {
        match self {
            ProtoKind::Torrent => "BitTorrent",
            ProtoKind::Ftp => "FTP",
            ProtoKind::Ftps => "FTPS",
            ProtoKind::Sftp => "SFTP/SSH",
            ProtoKind::Nntp => "NNTP (NZB)",
            ProtoKind::Metalink => "Metalink",
            ProtoKind::Dash => "DASH stream",
            ProtoKind::Hls => "HLS stream",
            ProtoKind::YtDlp => "Video page (yt-dlp)",
            ProtoKind::Http => "HTTP/HTTPS",
            ProtoKind::File => "Local file",
        }
    }
}

// Wire headers: engine::headers_for (strips pseudo-headers, drops
// invalid, adds Basic auth unless Authorization present). Reused below.

pub fn proto_kind(task: &DownloadTask) -> ProtoKind {
    let lower = task.url.trim().to_ascii_lowercase();
    if crate::torrent::is_torrent_task(task) {
        return ProtoKind::Torrent;
    }
    if lower.starts_with("ftps://") || lower.starts_with("ftpsi://") {
        return ProtoKind::Ftps;
    }
    if lower.starts_with("ftp://") {
        return ProtoKind::Ftp;
    }
    if lower.starts_with("sftp://") || lower.starts_with("ssh://") {
        return ProtoKind::Sftp;
    }
    if is_nzb_url(task) {
        return ProtoKind::Nntp;
    }
    if is_metalink_url(&task.url) {
        return ProtoKind::Metalink;
    }
    if manifest_kind_from_url(&task.url) == Some(Manifest::Dash) {
        return ProtoKind::Dash;
    }
    if manifest_kind_from_url(&task.url) == Some(Manifest::Hls) {
        return ProtoKind::Hls;
    }
    if task.headers.contains_key("x-ytdlp") || crate::ytdlp::is_video_page(&task.url) {
        return ProtoKind::YtDlp;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return ProtoKind::Http;
    }
    ProtoKind::File
}

/// Probe Content-Type only (headers, body dropped). On any error/non-success -> File.
/// ponytail: header-only probe misses manifests served as octet-stream/text; body-peek + reuse would need passing fetched text into download_dash/download_hls — add when such servers matter.
async fn probe_manifest(client: &reqwest::Client, task: &DownloadTask) -> Manifest {
    let req = client
        .get(&task.url)
        .headers(engine::headers_for_target(task, &task.url));
    let Ok(resp) = req.send().await else {
        return Manifest::File;
    };
    if !resp.status().is_success() {
        return Manifest::File;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ct = ct.split(';').next().unwrap_or("").trim();
    // IDM MIME map (REA round 6): video/vnd.mpeg.dash.mpd, application/vnd.apple.mpegurl.
    if ct.contains("application/metalink+xml") {
        return Manifest::Metalink;
    } else if ct.contains("dash") || ct.contains("mpd") {
        return Manifest::Dash;
    } else if ct.contains("mpegurl") || ct.contains("m3u") {
        return Manifest::Hls;
    }
    // Body sniff: manifests served as video/mp4/octet-stream/text (Reddit
    // packaged-media, signed CDNs). Peek first bytes; body dropped here,
    // download path re-fetches (signed URLs tolerate re-GET; probe already
    // consumed one anyway).
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut peek: Vec<u8> = Vec::new();
    while peek.len() < 1024 {
        match stream.next().await {
            Some(Ok(chunk)) => peek.extend_from_slice(&chunk),
            _ => break,
        }
    }
    classify_body(&peek).unwrap_or(Manifest::File)
}

/// Classify by body magic: HLS playlist, MPD/metalink XML, else None.
fn classify_body(bytes: &[u8]) -> Option<Manifest> {
    let t = String::from_utf8_lossy(bytes);
    let t = t.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("#EXTM3U") {
        return Some(Manifest::Hls);
    }
    if t.starts_with('<') {
        let head = &t[..t.len().min(512)].to_ascii_lowercase();
        if head.contains("<mpd") {
            return Some(Manifest::Dash);
        }
        if head.contains("<metalink") {
            return Some(Manifest::Metalink);
        }
    }
    None
}

/// Manifest-kind bytes (playlist/XML text) must never complete as media file.
fn looks_like_manifest_bytes(bytes: &[u8]) -> bool {
    classify_body(bytes).is_some()
}

/// Pub wrapper so engine segment fetch can reuse manifest guard.
pub fn looks_like_manifest_bytes_pub(bytes: &[u8]) -> bool {
    looks_like_manifest_bytes(bytes)
}

/// Normalize captured filename for media path: manifest-named tasks
/// (.m3u8/.mpd) produce merged media, never playlist file.
fn media_filename(filename: &str) -> String {
    let lower = filename.to_ascii_lowercase();
    for ext in [".m3u8", ".mpd", ".m3u"] {
        if lower.ends_with(ext) {
            return format!("{}.mp4", &filename[..filename.len() - ext.len()]);
        }
    }
    filename.to_string()
}

/// Resolve URL to download task(s): URL .mpd/.m3u8 hint first, else probe Content-Type, else plain HTTP.
pub async fn download_url(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    download_url_with_settings(client, task, ctrl, &Settings::default()).await
}

/// Same dispatch with live app `Settings` (NNTP server config lives there).
pub async fn download_url_with_settings(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<engine::Controls>,
    settings: &Settings,
) -> Result<DownloadTask, String> {
    let lower = task.url.to_ascii_lowercase();
    // transfer.it before the generic debrid gate: providers do not
    // support transfer.it, so downloads are direct-only via
    // `crate::transferit` (resolved natively, never debrid). Hard
    // errors here return Err so the SPA shell HTML never saves as a
    // fake file.
    if crate::transferit::is_task_url(&task.url) {
        return crate::transferit::download_transfer(client, task, ctrl, settings)
            .await
            .map_err(|(e, _hard)| e);
    }
    // Debrid before torrent/HTTP: magnets + hoster URLs. Resolved direct
    // URLs expire/IP-bound, so resolution runs fresh here on every start
    // (same client = same egress IP); persisted task keeps original URL
    // (download_via_debrid restores it). Soft debrid failure falls through
    // to local torrent / plain HTTP below; hard provider refusal returns
    // Err (see below) so hoster page HTML never saves as fake file.
    let choice = crate::model::debrid_choice_of(&task);
    if choice != "none"
        && ((crate::debrid::is_magnet(&task.url)
            && crate::debrid::any_for_magnets_choice(settings, choice))
            || (crate::torrent::is_torrent_task(&task)
                && !crate::debrid::is_magnet(&task.url)
                && crate::debrid::any_for_magnets_choice(settings, choice))
            || (!crate::torrent::is_torrent_task(&task)
                && (lower.starts_with("http://") || lower.starts_with("https://"))
                && !crate::debrid::providers_for(settings, choice).is_empty()))
    {
        // Hoster route only when URL is not plain file/manifest/video-page:
        // cheap gate = debrid hosts check is provider-side on unrestrict;
        // avoid hijacking normal HTTP by requiring hoster match attempt
        // only for non-manifest URLs. Failure falls through anyway, but
        // skip obvious manifests/video pages to save API calls.
        let skip = manifest_kind_from_url(&task.url).is_some()
            || crate::ytdlp::is_video_page(&task.url)
            // Google Drive: direct-only; debrid cannot unrestrict one-shot
            // usercontent GETs and the attempt hard-fails the task.
            || crate::debrid::is_google_drive_url(&task.url);
        if !skip {
            match crate::debrid::download_via_debrid(
                client.clone(),
                task.clone(),
                ctrl.clone(),
                settings,
            )
            .await
            {
                Ok(t) => {
                    crate::applog::append_default(
                        crate::applog::LV_INFO,
                        "debrid",
                        &format!("Debrid download finished: {}", t.filename),
                    );
                    return Ok(t);
                }
                Err((e, true)) => {
                    // Hard provider refusal (e.g. RD infringing_file,
                    // error 35): fail task with provider message.
                    // Falling back to direct HTTP would download the
                    // hoster HTML page as a fake completed file.
                    return Err(e);
                }
                Err((_, false)) => {
                    // Soft: unsupported/no provider — fall through to
                    // local torrent / HTTP. Never log URL/token.
                    crate::applog::append_default(
                        crate::applog::LV_DEBUG,
                        "debrid",
                        "Debrid soft failure; falling back to local download",
                    );
                }
            }
        }
    }
    // BitTorrent before HTTP probe: magnet has no HTTP form, .torrent probe would download metadata as file.
    if crate::torrent::is_torrent_task(&task) {
        return crate::torrent::download_torrent(client, task, ctrl, settings).await;
    }
    if lower.starts_with("ftp://") || lower.starts_with("ftps://") || lower.starts_with("ftpsi://")
    {
        return crate::ftp::download_ftp(task, ctrl).await;
    }
    if lower.starts_with("sftp://") || lower.starts_with("ssh://") {
        return crate::sftp::download_sftp(task, ctrl).await;
    }
    if is_nzb_url(&task) {
        let mut task = task;
        // nzb://<real-url> wrapper: strip scheme so fetch sees the NZB URL/path.
        if let Some(rest) = task.url.trim().strip_prefix("nzb://") {
            task.url = rest.to_string();
        }
        let cfg = crate::nntp::NntpConfig::from_settings(settings, &task);
        crate::applog::append_default(
            crate::applog::LV_INFO,
            "nzb",
            &format!("NZB download started: {}", task.filename),
        );
        return crate::nntp::download_nzb(client, task, ctrl, &cfg).await;
    }
    if is_metalink_url(&task.url) {
        return download_metalink(client, task, ctrl, settings.max_part_mb * 1024 * 1024).await;
    }
    // yt-dlp for manifests: sniffed signed manifests are where native pipeline
    // corrupts (playlist saved as media file). Picker choice (x-yt-*) fails
    // loud (selection never discarded); plain x-ytdlp toggle / manifest
    // intent: yt-dlp first, native fallback on error (not on cancel).
    let has_picker = task
        .headers
        .keys()
        .any(|k| k.to_ascii_lowercase().starts_with("x-yt-"));
    let manifest_intent = task
        .headers
        .get("x-page-intent")
        .map(|s| s == "manifest")
        .unwrap_or(false);
    if task.headers.contains_key("x-ytdlp") && has_picker {
        return crate::ytdlp::download_with_ytdlp(task, ctrl).await;
    }
    if task.headers.contains_key("x-ytdlp") || manifest_intent {
        match crate::ytdlp::download_with_ytdlp(task.clone(), ctrl.clone()).await {
            Ok(t) => return Ok(t),
            Err(e) => {
                if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(e);
                }
                // Page intent: yt-dlp is the only handler (native would save
                // the article HTML as a fake media file — the Globo "no
                // streams" path). Surface the yt-dlp error; a preserved
                // sniffed stream (x-page-stream) retries natively below.
                let page_intent = task
                    .headers
                    .get("x-page-intent")
                    .map(|s| s == "page")
                    .unwrap_or(false);
                if page_intent {
                    match task.headers.get("x-page-stream").cloned() {
                        Some(stream)
                            if (stream.starts_with("http://")
                                || stream.starts_with("https://"))
                                && !crate::ytdlp::is_video_page(&stream) =>
                        {
                            let mut fallback = task.clone();
                            fallback.url = stream;
                            fallback.headers.remove("x-ytdlp");
                            fallback.headers.remove("x-page-stream");
                            fallback.headers.remove("x-page-intent");
                            return Box::pin(download_url_with_settings(
                                client, fallback, ctrl, settings,
                            ))
                            .await;
                        }
                        _ => return Err(format!("yt-dlp failed: {e}")),
                    }
                }
                // else: fall through to native dispatch
            }
        }
        if task.headers.contains_key("x-ytdlp") && !manifest_intent {
            // x-ytdlp on non-manifest URL with yt-dlp failed: native would
            // likely also fail, but plain-file fallback is safe. Continue.
        }
    }
    if manifest_kind_from_url(&task.url) == Some(Manifest::Dash) {
        return download_dash(client, task, ctrl, false).await;
    }
    if manifest_kind_from_url(&task.url) == Some(Manifest::Hls) {
        return download_hls(client, task, ctrl).await;
    }
    // Segment-only probe/file fallback: never plain-download one part. Runs
    // before probe so Content-Type sniffing cannot rescue it either.
    if let Some(reason) = segment_only_reject(&task) {
        return Err(reason);
    }
    // Video-page URLs (YouTube etc): yt-dlp extracts formats; native probe would
    // download HTML page. Fall through to native flow on any yt-dlp failure.
    if crate::ytdlp::is_video_page(&task.url) {
        // No native fallback: probe would download page HTML as fake file.
        return crate::ytdlp::download_with_ytdlp(task, ctrl)
            .await
            .map_err(|e| format!("yt-dlp failed: {e}"));
    }
    // Direct archive/document URLs skip the manifest-sniff GET: it burns a
    // full request on one-shot/non-resumable hosts (Drive takeout) and the
    // body peek is dropped. Engine probe handles them directly.
    let path_only = lower.split(['?', '#']).next().unwrap_or(&lower);
    const DIRECT_FILE_EXTS: &[&str] = &[
        ".zip", ".7z", ".rar", ".gz", ".bz2", ".xz", ".zst", ".tar", ".tgz", ".iso", ".img",
        ".apk", ".exe", ".msi", ".dmg", ".deb", ".rpm", ".pdf",
    ];
    if DIRECT_FILE_EXTS.iter().any(|e| path_only.ends_with(e)) {
        return engine::download_http_with_prewarm(
            client,
            task.clone(),
            ctrl,
            engine::prewarm_take(&task.url),
            settings.max_part_mb * 1024 * 1024,
        )
        .await;
    }
    match probe_manifest(&client, &task).await {
        Manifest::Metalink => {
            download_metalink(client, task, ctrl, settings.max_part_mb * 1024 * 1024).await
        }
        Manifest::Dash => download_dash(client, task, ctrl, false).await,
        Manifest::Hls => download_hls(client, task, ctrl).await,
        // Prewarm hit (Add/capture dialog probe, runtime-only registry):
        // skips engine probe. Plain downloads re-GET body anyway.
        Manifest::File => {
            engine::download_http_with_prewarm(
                client,
                task.clone(),
                ctrl,
                engine::prewarm_take(&task.url),
                settings.max_part_mb * 1024 * 1024,
            )
            .await
        }
    }
}

#[cfg(test)]
mod manifest_classify_tests {
    use super::*;

    #[test]
    fn body_sniff_classifies() {
        assert_eq!(
            classify_body(b"#EXTM3U\n#EXT-X-VERSION:3\n"),
            Some(Manifest::Hls)
        );
        assert_eq!(
            classify_body(b"<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\">"),
            Some(Manifest::Dash)
        );
        assert_eq!(classify_body(b"\x00\x00\x00\x18ftypisom...."), None);
        assert_eq!(media_filename("movie.m3u8"), "movie.mp4");
        assert_eq!(media_filename("movie.MPD"), "movie.mp4");
        assert_eq!(media_filename("movie.mp4"), "movie.mp4");
    }
}

/// Fetch + parse .meta4/.metalink, take first <file>: primary url[0], rest
/// are mirrors (existing engine probe failover), sha256 feeds finish_check.
/// ponytail: first file only; multi-file .metalink would need task fan-out.
async fn download_metalink(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<engine::Controls>,
    max_part: u64,
) -> Result<DownloadTask, String> {
    // No silent fallback: bad fetch/parse/empty-url-list must error the task.
    let xml = fetch_text(&client, &task, &task.url)
        .await
        .map_err(|e| format!("metalink fetch failed: {e}"))?;
    let files = crate::metalink::parse_metalink(&xml)?;
    let f = files
        .into_iter()
        .next()
        .ok_or("metalink has no <file> entries")?;
    let mut urls = f.urls.into_iter();
    let Some(primary) = urls.next() else {
        return Err(format!("metalink file '{}' has no <url>", f.name));
    };
    task.url = primary;
    task.mirrors = urls.collect();
    if !f.name.is_empty() {
        task.filename = sanitize_filename(&f.name);
    }
    // Hash wins; keep caller-supplied sha256 when manifest has none.
    if f.sha256.is_some() {
        task.sha256 = f.sha256;
    }
    engine::download_http_with_prewarm(
        client,
        task.clone(),
        ctrl,
        engine::prewarm_take(&task.url),
        max_part,
    )
    .await
}

async fn fetch_text(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
) -> Result<String, String> {
    let req = client
        .get(url)
        .headers(engine::headers_for_target(task, url));
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

/// DASH download. If once=true, single snapshot pass (used for dynamic refusal fallback test).
pub async fn download_dash(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<engine::Controls>,
    _once: bool,
) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let xml = fetch_text(&client, &task, &task.url).await?;
    task.filename = media_filename(&task.filename);
    // URL pattern lied: body is HLS or plain media (Reddit packaged-media
    // variant MP4 at manifest-pattern URL). Route by content, not suffix.
    {
        let head = xml.trim_start_matches('\u{feff}').trim_start();
        if head.starts_with("#EXTM3U") {
            return Box::pin(download_hls(client, task, ctrl)).await;
        }
        if !head.starts_with('<') {
            return crate::engine::download_http(client, task, ctrl).await;
        }
    }
    if is_protected(&xml) {
        task.status = TaskStatus::Protected;
        task.error = "Protected content (DRM) is not supported".into();
        return Ok(task);
    }
    let mut parsed = parse_mpd(&xml, &base)
        .map_err(|e| format!("DASH manifest parse failed ({e}); url={}", task.url))?;
    if parsed.mpd_type == "dynamic" {
        // Live DASH: poll until stop condition (cancel) or END via repeated fetch collecting new segments.
        // ponytail: IDM live behavior unverified without live fixture against IDM; clone polls minimumUpdatePeriod and accumulates unique segments until cancelled.
        let mut seen: engine::MediaTracks = Vec::new();
        let poll = parsed.minimum_update_period_secs.unwrap_or(2.0).max(1.0);
        // first snapshot
        append_reps(&parsed, &mut seen, &task);
        loop {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(poll)).await;
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let xml2 = fetch_text(&client, &task, &task.url).await?;
            if is_protected(&xml2) {
                task.status = TaskStatus::Protected;
                task.error = "Protected content (DRM) is not supported".into();
                return Ok(task);
            }
            parsed = parse_mpd(&xml2, &base)?;
            let before: usize = seen.iter().map(|(_, s)| s.len()).sum();
            append_reps(&parsed, &mut seen, &task);
            let after: usize = seen.iter().map(|(_, s)| s.len()).sum();
            if after == before {
                // No new segments for one poll cycle: treat as ended snapshot and assemble what we have.
                break;
            }
        }
        if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            let mut t = task;
            t.status = TaskStatus::Paused;
            return Ok(t);
        }
        if track_is_empty(&seen) {
            return Err("live MPD produced no segments".into());
        }
        return engine::download_media(client, task, ctrl, seen).await;
    }
    let mut tracks = Vec::new();
    append_reps(&parsed, &mut tracks, &task);
    if tracks.iter().all(|(_, s)| s.is_empty()) {
        return Err("MPD produced no segment URLs".into());
    }
    engine::download_media(client, task, ctrl, tracks).await
}

fn track_is_empty(t: &engine::MediaTracks) -> bool {
    t.iter().all(|(_, s)| s.is_empty())
}

fn append_reps(parsed: &Mpd, tracks: &mut engine::MediaTracks, task: &DownloadTask) {
    // Select representation: task.headers x-rep-id override else highest bandwidth video + highest audio + first subtitle per spec default.
    let want_rep = task.headers.get("x-rep-id").cloned();
    // Group by content kind, merged in Period order: parse_mpd already concatenates periods as separate sets in order;
    // for multi-period same-kind sets, append segments in order into same track.
    for set in &parsed.sets {
        let kind = if set.mime_type.starts_with("video") || set.content_type == "video" {
            "video"
        } else if set.mime_type.starts_with("audio") || set.content_type == "audio" {
            "audio"
        } else if set.mime_type.contains("text") || set.content_type == "text" {
            "text"
        } else if set.mime_type.contains('/') {
            set.mime_type.split('/').next().unwrap_or("media")
        } else {
            "media"
        };
        let reps = &set.representations;
        if reps.is_empty() {
            continue;
        }
        let chosen = if let Some(w) = &want_rep {
            reps.iter().find(|r| &r.id == w).unwrap_or(&reps[0])
        } else {
            reps.iter().max_by_key(|r| r.bandwidth).unwrap_or(&reps[0])
        };
        // SegmentBase: single file holds init+index+media. Emit (None, file) — engine
        // fetching init_url too would download whole file twice (mpd.rs keeps init_url=None there).
        let segs: Vec<(Option<String>, String)> = if chosen.segment_base {
            chosen
                .segment_urls
                .iter()
                .map(|u| (None, u.clone()))
                .collect()
        } else {
            chosen
                .segment_urls
                .iter()
                .map(|u| (chosen.init_url.clone(), u.clone()))
                .collect()
        };
        if let Some(existing) = tracks.iter_mut().find(|(k, _)| k == kind) {
            for s in segs {
                if !existing.1.contains(&s) {
                    existing.1.push(s);
                }
            }
        } else {
            tracks.push((kind.to_string(), segs));
        }
    }
}

pub async fn download_hls(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let text = fetch_text(&client, &task, &task.url).await?;
    if let Some(m) = hls::key_method(&text) {
        if m != "AES-128" && m != "NONE" {
            task.status = TaskStatus::Protected;
            task.error = format!("Encrypted HLS METHOD={m} is not supported");
            return Ok(task);
        }
    }
    if text.contains("#EXT-X-STREAM-INF") {
        let master = hls::parse_master(&text, &base);
        if master.variants.is_empty() {
            return Err("HLS master has no variants".into());
        }
        // variant selection: x-variant-index header or highest bandwidth
        let idx: usize = task
            .headers
            .get("x-variant-index")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                master
                    .variants
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, v)| v.bandwidth)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let variant = &master.variants[idx.min(master.variants.len() - 1)];
        let mut t2 = task.clone();
        t2.url = variant.uri.clone();
        // AUDIO rendition in same group; DEFAULT=YES preferred.
        let audio_uri = master
            .renditions
            .iter()
            .find(|r| {
                r.kind == "AUDIO"
                    && r.default
                    && !variant.audio_group.is_empty()
                    && r.group_id == variant.audio_group
            })
            .and_then(|r| r.uri.clone())
            .or_else(|| {
                master
                    .renditions
                    .iter()
                    .find(|r| {
                        r.kind == "AUDIO"
                            && (variant.audio_group.is_empty() || r.group_id == variant.audio_group)
                    })
                    .and_then(|r| r.uri.clone())
            });
        let video_task = download_hls_media(client.clone(), t2.clone(), ctrl.clone()).await?;
        let Some(au) = audio_uri.filter(|u| u != &variant.uri) else {
            return Ok(video_task);
        };
        // Video failed/protected: keep its status, skip audio.
        if video_task.status != TaskStatus::Complete {
            return Ok(video_task);
        }
        // Audio to hidden temp file, then mux into final dest. Mirror DASH: mux fail -> Error, keep video.
        // Mux/audio temps live in workdir, never dest root (.mux-*.ts litter).
        let work = task
            .dest_dir
            .join(format!(".downloadcraft-hls-mux-{}", task.id));
        let _ = tokio::fs::create_dir_all(&work).await;
        let mut at = task.clone();
        at.dest_dir = work.clone();
        at.url = au.clone();
        at.filename = format!(".{}-audio", task.filename);
        match download_hls_media(client.clone(), at, ctrl.clone()).await {
            Err(e) => {
                let mut out = video_task;
                out.status = TaskStatus::Error;
                out.error = format!("audio download failed: {e}");
                return Ok(out);
            }
            Ok(at2) if at2.status != TaskStatus::Complete => {
                let mut out = video_task;
                out.status = TaskStatus::Error;
                out.error = format!("audio download failed: {}", at2.error);
                return Ok(out);
            }
            Ok(_) => {}
        }
        let audio_file = work.join(format!(".{}-audio", task.filename));
        let video_file = task.dest_dir.join(&task.filename);
        // ffmpeg cannot mux in-place (input == output) and picks muxer by extension:
        // temp name must keep real extension or it cannot choose output format.
        let mux_out = work.join(format!(".mux-{}", task.filename));
        let result = engine::mux_tracks(&[video_file.clone(), audio_file.clone()], &mux_out)
            .await
            .and_then(|()| std::fs::rename(&mux_out, &video_file).map_err(|e| e.to_string()));
        let _ = std::fs::remove_file(&mux_out);
        let _ = std::fs::remove_file(&audio_file);
        let _ = tokio::fs::remove_dir_all(&work).await;
        match result {
            Ok(()) => return Ok(video_task),
            Err(e) => {
                let mut out = video_task;
                out.status = TaskStatus::Error;
                out.error = format!("audio mux failed: {e}");
                return Ok(out);
            }
        }
    }
    download_hls_media(client, task, ctrl).await
}

async fn fetch_key(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
) -> Result<[u8; 16], String> {
    let req = client
        .get(url)
        .headers(engine::headers_for_target(task, url));
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for key {url}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    <[u8; 16]>::try_from(bytes.as_ref())
        .map_err(|_| format!("AES-128 key must be 16 bytes, got {}", bytes.len()))
}

async fn download_hls_media(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let text = fetch_text(&client, &task, &task.url).await?;
    let mut task = task;
    task.filename = media_filename(&task.filename);
    {
        let head = text.trim_start_matches('\u{feff}').trim_start();
        if head.starts_with('<') && head.to_ascii_lowercase().contains("<mpd") {
            return Box::pin(download_dash(client, task, ctrl, false)).await;
        }
        if !head.starts_with("#EXTM3U") && !head.starts_with('<') {
            return crate::engine::download_http(client, task, ctrl).await;
        }
        if !head.starts_with("#EXTM3U") {
            return Err(format!(
                "Expected HLS playlist, got non-playlist body; url={}",
                task.url
            ));
        }
    }
    if let Some(m) = hls::key_method(&text) {
        if m != "AES-128" && m != "NONE" {
            let mut t = task.clone();
            t.status = TaskStatus::Protected;
            t.error = format!("Encrypted HLS METHOD={m} is not supported");
            return Ok(t);
        }
    }
    let media = hls::resolve_implicit_ranges(hls::parse_media(&text, &base));
    if media.segments.is_empty() {
        return Err("HLS media playlist has no segments".into());
    }
    // Live HLS: if no ENDLIST, poll until ENDLIST appears or cancelled
    let mut media = media;
    if !media.end_list {
        let mut seen = media.segments.clone();
        let mut seen_keys = media.key_infos.clone();
        let mut seen_ranges = media.byte_ranges.clone();
        loop {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(
                media.target_duration.max(1.0) / 2.0,
            ))
            .await;
            let t2 = fetch_text(&client, &task, &task.url).await?;
            let m2 = hls::resolve_implicit_ranges(hls::parse_media(&t2, &base));
            for (i, s) in m2.segments.iter().enumerate() {
                if !seen.contains(s) {
                    seen.push(s.clone());
                    seen_keys.push(m2.key_infos.get(i).cloned().unwrap_or(None));
                    seen_ranges.push(m2.byte_ranges.get(i).copied().flatten());
                }
            }
            if m2.end_list {
                media = m2;
                break;
            }
        }
        if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            let mut t = task.clone();
            t.status = TaskStatus::Paused;
            return Ok(t);
        }
        if !media.end_list {
            media.segments = seen;
            media.key_infos = seen_keys;
            media.byte_ranges = seen_ranges;
        }
    }
    // Encrypted playlists: any non-AES-128 method refused; AES-128 handled below.
    // EXT-X-MAP init segment is clear (never decrypted).
    let encrypted = media.key_infos.iter().any(|k| k.is_some());
    if encrypted {
        for (i, ki) in media.key_infos.iter().enumerate() {
            if let Some(ki) = ki {
                if ki.method != "AES-128" {
                    let mut t = task.clone();
                    t.status = TaskStatus::Protected;
                    t.error = format!("Encrypted HLS METHOD={} is not supported", ki.method);
                    return Ok(t);
                }
                if ki.uri.is_none() {
                    return Err(format!("EXT-X-KEY AES-128 segment {i} missing URI"));
                }
            }
        }
    }
    // BYTERANGE or encrypted segments need per-segment fetch to temp then concat
    // (plain path streams raw bytes via download_media; decrypt needs whole segment).
    let has_ranges =
        media.byte_ranges.iter().any(|r| r.is_some()) || media.map_byte_range.is_some();
    if has_ranges || encrypted {
        let work = task
            .dest_dir
            .join(format!(".downloadcraft-hls-{}", task.id));
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        ctrl.parts_are_segments
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![(0, media.segments.len() as u64)];
        let mut parts: Vec<std::path::PathBuf> = Vec::new();
        if let Some(init) = &media.init_uri {
            let p = work.join("init.bin");
            let n =
                engine::fetch_bytes_counted(&client, &task, init, media.map_byte_range, &p).await?;
            ctrl.downloaded
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            parts.push(p);
        }
        // Key cache by URI: keys may rotate mid-playlist.
        let mut key_cache: std::collections::HashMap<String, [u8; 16]> =
            std::collections::HashMap::new();
        for (i, seg) in media.segments.iter().enumerate() {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let mut t = task.clone();
                t.status = TaskStatus::Paused;
                return Ok(t);
            }
            let p = work.join(format!("{i}.seg"));
            let n = engine::fetch_bytes_counted(
                &client,
                &task,
                seg,
                media.byte_ranges.get(i).copied().flatten(),
                &p,
            )
            .await?;
            ctrl.downloaded
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            if let Some(part) = ctrl
                .parts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_mut()
            {
                part.0 = (i + 1) as u64;
            }
            if let Some(Some(ki)) = media.key_infos.get(i) {
                let key_uri = ki.uri.as_deref().unwrap();
                let key = match key_cache.get(key_uri) {
                    Some(k) => *k,
                    None => {
                        let k = fetch_key(&client, &task, key_uri).await?;
                        key_cache.insert(key_uri.to_string(), k);
                        k
                    }
                };
                let iv = ki
                    .iv
                    .unwrap_or_else(|| hls::iv_for_sequence(media.media_sequence + i as u64));
                let raw = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
                let plain = hls::decrypt_segment(&key, &iv, &raw)?;
                tokio::fs::write(&p, &plain)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            parts.push(p);
        }
        let dest = task.dest_dir.join(&task.filename);
        let mut total_bytes: u64 = 0;
        {
            let mut out = tokio::fs::File::create(&dest)
                .await
                .map_err(|e| e.to_string())?;
            use tokio::io::AsyncWriteExt;
            for p in &parts {
                let b = tokio::fs::read(p).await.map_err(|e| e.to_string())?;
                total_bytes += b.len() as u64;
                out.write_all(&b).await.map_err(|e| e.to_string())?;
            }
            out.flush().await.map_err(|e| e.to_string())?;
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
        // HLS concat is raw TS mostly: validate, remux mp4 or rename .ts.
        let mut t = task;
        let final_dest = match engine::validate_output(&dest, &t.filename) {
            Ok(_) => dest.clone(),
            Err(engine::ValidateFail::TsInMp4) => {
                let fixed = engine::ensure_mp4_or_ts(&dest).await?;
                if let Some(name) = fixed.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    t.filename = name;
                }
                fixed
            }
            Err(e @ (engine::ValidateFail::Empty | engine::ValidateFail::Playlist)) => {
                return Err(engine::quarantine_bad(&dest, &e))
            }
            // Other magic mismatch (e.g. decrypted non-TS payload in .ts):
            // tolerate — segments already fetched/decrypted, magic is advisory here.
            Err(_) => dest.clone(),
        };
        let total_bytes = std::fs::metadata(&final_dest)
            .map(|m| m.len())
            .unwrap_or(total_bytes);
        t.status = TaskStatus::Complete;
        t.downloaded = total_bytes;
        t.total = Some(total_bytes);
        ctrl.downloaded
            .store(total_bytes, std::sync::atomic::Ordering::Relaxed);
        return Ok(t);
    }
    let segs: Vec<(Option<String>, String)> = media
        .segments
        .iter()
        .map(|u| (media.init_uri.clone(), u.clone()))
        .collect();
    engine::download_media(client, task, ctrl, vec![("video".into(), segs)]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DownloadTask;

    fn kind_of(url: &str) -> ProtoKind {
        proto_kind(&DownloadTask {
            url: url.to_string(),
            ..Default::default()
        })
    }

    #[test]
    fn proto_kind_labels() {
        assert_eq!(
            kind_of("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"),
            ProtoKind::Torrent
        );
        assert_eq!(kind_of("https://x.org/a.torrent"), ProtoKind::Torrent);
        assert_eq!(kind_of("ftp://x.org/a.zip"), ProtoKind::Ftp);
        assert_eq!(kind_of("ftps://x.org/a.zip"), ProtoKind::Ftps);
        assert_eq!(kind_of("ftpsi://x.org/a.zip"), ProtoKind::Ftps);
        assert_eq!(kind_of("sftp://x.org/a.zip"), ProtoKind::Sftp);
        assert_eq!(kind_of("ssh://x.org/a.zip"), ProtoKind::Sftp);
        assert_eq!(kind_of("nzb://x.org/a.nzb"), ProtoKind::Nntp);
        assert_eq!(kind_of("https://x.org/a.meta4"), ProtoKind::Metalink);
        assert_eq!(kind_of("https://x.org/m.mpd?token=1"), ProtoKind::Dash);
        assert_eq!(kind_of("https://x.org/m.m3u8"), ProtoKind::Hls);
        assert_eq!(kind_of("https://x.org/a.zip"), ProtoKind::Http);
        assert_eq!(kind_of("file.zip"), ProtoKind::File);
        assert_eq!(ProtoKind::Torrent.label(), "TORRENT");
    }

    #[test]
    fn manifest_kind_path_not_query_token() {
        // Query token value `.mpd`/`.m3u8` alone must not route as manifest.
        assert_eq!(kind_of("https://x.org/watch?f=.mpd"), ProtoKind::Http);
        assert_eq!(kind_of("https://x.org/watch?f=.m3u8"), ProtoKind::Http);
        assert_eq!(manifest_kind_from_url("https://x.org/watch?f=.mpd"), None);
        // Real manifest path (query noise ok) still routes.
        assert_eq!(
            manifest_kind_from_url("https://x.org/m.mpd?token=1"),
            Some(Manifest::Dash)
        );
        assert_eq!(
            manifest_kind_from_url("https://x.org/m.m3u8#frag"),
            Some(Manifest::Hls)
        );
        assert!(url_path_ends("https://x.org/M.MPD?token=1", &[".mpd"]));
        assert!(!url_path_ends("https://x.org/watch?f=.mpd", &[".mpd"]));
        // Reddit packaged: path .mp4, query m=DASHPlaylist.mpd.
        let reddit = "https://v.redd.it/abc123/m2-vp9-res_854p.mp4?m=DASHPlaylist.mpd";
        assert_eq!(manifest_kind_from_url(reddit), Some(Manifest::Dash));
        assert_eq!(kind_of(reddit), ProtoKind::Dash);
        assert_eq!(
            manifest_kind_from_url("https://x.org/v.mp4?m=HLSPlaylist.m3u8"),
            Some(Manifest::Hls)
        );
    }

    #[test]
    fn segment_only_reject_rules() {
        let bare = DownloadTask {
            url: "https://cdn.example.com/v/seg-00001.m4s".into(),
            ..Default::default()
        };
        assert!(segment_only_reject(&bare).is_some());
        let bare_ts = DownloadTask {
            url: "https://cdn.example.com/v/seg1.ts?tok=1".into(),
            filename: "seg1.ts".into(),
            ..Default::default()
        };
        // Bare .ts now allowed: self-contained, goes through probe.
        assert!(segment_only_reject(&bare_ts).is_none());
        let mut page_ctx = bare_ts.clone();
        page_ctx
            .headers
            .insert("x-page-url".into(), "https://x.org/watch?v=abc123".into());
        assert!(segment_only_reject(&page_ctx).is_none());
        // Page URL is the parent manifest: allowed (manifest context).
        let mut man_ctx = bare_ts.clone();
        man_ctx.headers.insert(
            "x-page-url".into(),
            "https://cdn.example.com/v/pl.m3u8".into(),
        );
        assert!(segment_only_reject(&man_ctx).is_none());
        let mut dash_ctx = bare.clone();
        dash_ctx.headers.insert(
            "x-page-url".into(),
            "https://cdn.example.com/v/pl.mpd".into(),
        );
        assert!(segment_only_reject(&dash_ctx).is_none());
        // Non-segment URL never rejected.
        let file = DownloadTask {
            url: "https://x.org/a.mp4".into(),
            ..Default::default()
        };
        assert!(segment_only_reject(&file).is_none());
    }
}
