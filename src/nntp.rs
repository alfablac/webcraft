// NNTP/NZB download backend (v1). NZB (HTTP URL or local path) -> segments
// fetched per file over N parallel NNTP connections (plain or TLS; N = task
// segments else global connections setting), yEnc-decoded, assembled in
// NZB order by single writer to sanitized NZB file names in out dir
// (dest_dir/<output_name> for multi-file NZB, else dest_dir).
// PAR2 repair in-process via crate::par2 after download when set includes
// .par2. Auto-unpack via crate::unpack when cfg.auto_unpack (archive deleted
// only on verified success). ponytail: no per-segment resume (pause restarts
// current file; completed files skipped via nzbstate sidecar). Add segment
// cache in .downloadcraft-{task.id} workdir when needed.
use crate::engine::Controls;
use crate::model::{DownloadTask, Settings, TaskStatus};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};

// ---------- NZB parsing ----------

#[derive(Clone, Debug, PartialEq)]
pub struct NzbSegment {
    pub id: String,
    pub bytes: u64,
    pub number: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NzbFile {
    pub name: String,
    pub poster: String,
    pub date: i64,
    pub groups: Vec<String>,
    pub segments: Vec<NzbSegment>,
    /// Sum of NZB segment `bytes` (on-wire yEnc size, slightly > decoded size).
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Nzb {
    pub files: Vec<NzbFile>,
    pub total_bytes: u64,
    /// `<head><meta type=..>text</meta>` pairs: key lowercased.
    pub meta: std::collections::BTreeMap<String, String>,
}

/// Filename from NZB subject: text inside first pair of quotes wins
/// (`poster "file.rar" yEnc (1/3)`), else whole subject.
fn name_from_subject(subject: &str) -> String {
    let name = match (subject.find('"'), subject.rfind('"')) {
        (Some(a), Some(b)) if b > a => &subject[a + 1..b],
        _ => subject.trim(),
    };
    crate::model::sanitize_filename(name)
}

pub fn parse_nzb(xml: &str) -> Result<Nzb, String> {
    // Real NZBs ship a newzBin DOCTYPE; roxmltree rejects DTD by default.
    let opts = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, opts).map_err(|e| e.to_string())?;
    if doc.root_element().tag_name().name() != "nzb" {
        return Err("not an NZB document".into());
    }
    let mut meta = std::collections::BTreeMap::new();
    for m in doc.descendants().filter(|n| n.has_tag_name("meta")) {
        if let Some(k) = m.attribute("type") {
            // Password values: trim only outer whitespace introduced by XML
            // formatting; `type` key trimmed+lowercased. Passwords with
            // meaningful interior spaces preserved.
            let text = m.text().unwrap_or("").trim().to_string();
            if !text.is_empty() {
                meta.insert(k.trim().to_ascii_lowercase(), text);
            }
        }
    }
    let mut files = Vec::new();
    for file in doc.descendants().filter(|n| n.has_tag_name("file")) {
        let subject = file.attribute("subject").unwrap_or("");
        let mut f = NzbFile {
            name: name_from_subject(subject),
            poster: file.attribute("poster").unwrap_or("").to_string(),
            date: file
                .attribute("date")
                .and_then(|d| d.parse().ok())
                .unwrap_or(0),
            groups: Vec::new(),
            segments: Vec::new(),
            total_bytes: 0,
        };
        for n in file.descendants() {
            if n.has_tag_name("group") {
                if let Some(t) = n.text().map(|t| t.trim()).filter(|t| !t.is_empty()) {
                    f.groups.push(t.to_string());
                }
            } else if n.has_tag_name("segment") {
                let Some(id) = n.text().map(|t| t.trim()).filter(|t| !t.is_empty()) else {
                    continue;
                };
                let number = n
                    .attribute("number")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(f.segments.len() as u32 + 1);
                let bytes = n
                    .attribute("bytes")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                f.segments.push(NzbSegment {
                    id: id.to_string(),
                    bytes,
                    number,
                });
            }
        }
        f.segments.sort_by_key(|s| s.number);
        f.total_bytes = f.segments.iter().map(|s| s.bytes).sum();
        files.push(f);
    }
    if files.is_empty() {
        return Err("NZB has no <file> entries".into());
    }
    let total_bytes = files.iter().map(|f| f.total_bytes).sum();
    Ok(Nzb {
        files,
        total_bytes,
        meta,
    })
}

/// Percent-decode %XX sequences (URL-derived names).
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `{{password}}` embedded in a name: SABnzbd convention for NZB filename
/// (`Release {{secret}}.nzb`) and archive stems (`Release {{secret}}.rar`).
/// Scans all pairs, first non-empty wins. Percent-decodes first (URL names).
pub fn password_from_braces(name: &str) -> Option<String> {
    let decoded = pct_decode(name);
    let mut rest = decoded.as_str();
    while let Some(a) = rest.find("{{") {
        let after = &rest[a + 2..];
        let Some(b) = after.find("}}") else { break };
        let pw = after[..b].trim();
        if !pw.is_empty() {
            return Some(pw.to_string());
        }
        rest = &after[b + 2..];
    }
    None
}

/// Remove `{{...}}` password segments from a display/output name, so the
/// secret never lands in folder names, persisted task state, or errors.
pub fn strip_brace_password(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(a) = rest.find("{{") {
        out.push_str(&rest[..a]);
        match rest[a + 2..].find("}}") {
            Some(b) => rest = &rest[a + 2 + b + 2..],
            None => {
                rest = &rest[a..];
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Archive password for this NZB (NZB-wide default). Priority:
/// `<meta type="password">`, then `{{pw}}` in NZB source/meta names, then
/// `{{pw}}` in archive file names. RAR passwords on Usenet live inside NZB.
pub fn nzb_password(nzb: &Nzb, source_url: &str) -> Option<String> {
    nzb_password_for(nzb, source_url, None)
}

/// `nzb_password` with per-archive override: `{{pw}}` in that archive's own
/// name wins over NZB-wide meta/source defaults.
pub fn nzb_password_for(nzb: &Nzb, source_url: &str, archive_name: Option<&str>) -> Option<String> {
    if let Some(name) = archive_name {
        if let Some(pw) = password_from_braces(name) {
            return Some(pw);
        }
    }
    if let Some(pw) = nzb.meta.get("password") {
        let pw = pw.trim();
        if !pw.is_empty() {
            return Some(pw.to_string());
        }
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Some(s) = source_stem(source_url) {
        candidates.push(s);
    }
    for k in ["title", "name"] {
        if let Some(v) = nzb.meta.get(k) {
            candidates.push(v.clone());
        }
    }
    for c in &candidates {
        if let Some(pw) = password_from_braces(c) {
            return Some(pw);
        }
    }
    nzb.files
        .iter()
        .filter(|f| crate::unpack::is_archive(&f.name))
        .find_map(|f| password_from_braces(&f.name))
}

/// Sanitized meta value usable as output name; None if sanitizes to
/// placeholder "download.bin".
fn meta_name(nzb: &Nzb, key: &str) -> Option<String> {
    let raw = nzb.meta.get(key)?.trim();
    if raw.is_empty() {
        return None;
    }
    let s = crate::model::sanitize_filename(raw);
    (!s.is_empty() && s != "download.bin").then_some(s)
}

/// Stem from NZB source URL: `filename_from_url` minus trailing `.nzb`
/// (else its extension). Skip-list: empty/"nzb-files"/"download" (covers
/// bare ".../download" giving "download.bin"; "https://x/get?id=1" gives
/// "get" — no path stem, skip as generic too).
fn source_stem(source_url: &str) -> Option<String> {
    let name = crate::model::filename_from_url(source_url);
    let lower = name.to_ascii_lowercase();
    let stem = if lower.ends_with(".nzb") {
        &name[..name.len() - 4]
    } else {
        crate::model::split_stem_ext(&name).0
    };
    let s = crate::model::sanitize_filename(stem);
    let sl = s.to_ascii_lowercase();
    if s.is_empty() || sl == "nzb-files" || sl == "download" || sl == "get" || s == "download.bin" {
        None
    } else {
        Some(s)
    }
}

/// Longest common byte prefix of file names, cut at last separator
/// boundary before it ("Release.Name.2024.part1/2.rar" -> "Release.Name.2024").
/// Requires >=4 chars after cut+strip.
fn common_prefix(files: &[NzbFile]) -> Option<String> {
    let first = files.first()?.name.as_bytes();
    let mut len = first.len();
    for f in &files[1..] {
        len = first
            .iter()
            .zip(f.name.as_bytes())
            .take(len)
            .take_while(|(a, b)| a == b)
            .count();
        if len == 0 {
            return None;
        }
    }
    let prefix = std::str::from_utf8(&first[..len]).ok()?;
    let boundary = prefix.rfind(['.', '_', '-', ' ', '[', '(']).unwrap_or(0);
    let cut = prefix[..boundary]
        .trim_end_matches(['.', '_', '-', ' '])
        .trim();
    if cut.chars().count() < 4 {
        return None;
    }
    let s = crate::model::sanitize_filename(cut);
    (!s.is_empty() && s != "download.bin").then_some(s)
}

/// Output name for an NZB: single file -> file name; multi -> folder name
/// from meta title/name, else NZB source stem, else file-name common
/// prefix, else first file stem, else "nzb-files". Always sanitized.
pub fn output_name(nzb: &Nzb, source_url: &str) -> String {
    // Strip {{password}} so secret never becomes folder name. Single file:
    // name must match file on disk verbatim (task.filename is join path).
    let clean = |s: &str| crate::model::sanitize_filename(&strip_brace_password(s));
    if nzb.files.len() == 1 {
        return nzb.files[0].name.clone();
    }
    if let Some(n) = meta_name(nzb, "title").or_else(|| meta_name(nzb, "name")) {
        return clean(&n);
    }
    if let Some(n) = source_stem(source_url) {
        return clean(&n);
    }
    if let Some(n) = common_prefix(&nzb.files) {
        return clean(&n);
    }
    nzb.files
        .first()
        .map(|f| clean(crate::model::split_stem_ext(&f.name).0))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nzb-files".into())
}

// ---------- CRC32 (IEEE, table-free bitwise) ----------

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

// ---------- yEnc decoding ----------

#[derive(Clone, Debug, PartialEq)]
pub struct YencPart {
    pub name: Option<String>,
    pub file_size: Option<u64>,
    pub part: Option<u32>,
    pub begin: Option<u64>,
    pub end: Option<u64>,
    pub data: Vec<u8>,
}

fn kv_u64(params: &str, key: &str) -> Option<u64> {
    params.split_whitespace().find_map(|p| {
        p.strip_prefix(key)
            .and_then(|v| v.strip_prefix('='))
            .and_then(|v| v.parse().ok())
    })
}

/// Decode one yEnc article body. Verifies `=yend` size and pcrc32/crc32 when
/// present. Bodies without `=ybegin` pass through raw (plain text postings).
pub fn yenc_decode(input: &[u8]) -> Result<YencPart, String> {
    // Raw bytes, NOT lossy UTF-8: encoded bytes >= 0x80 are data, UTF-8
    // replacement chars would corrupt them.
    let mut part = YencPart {
        name: None,
        file_size: None,
        part: None,
        begin: None,
        end: None,
        data: Vec::new(),
    };
    let mut in_data = false;
    let mut saw_yenc = false;
    for raw in input.split(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if let Some(rest) = line.strip_prefix(b"=ybegin".as_slice()) {
            let rest = String::from_utf8_lossy(rest);
            saw_yenc = true;
            in_data = true;
            part.part = kv_u64(&rest, "part").map(|v| v as u32);
            part.file_size = kv_u64(&rest, "size");
            if let Some(i) = rest.find("name=") {
                part.name = Some(rest[i + 5..].trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix(b"=ypart".as_slice()) {
            let rest = String::from_utf8_lossy(rest);
            part.begin = kv_u64(&rest, "begin");
            part.end = kv_u64(&rest, "end");
            in_data = true;
        } else if let Some(rest) = line.strip_prefix(b"=yend".as_slice()) {
            let rest = String::from_utf8_lossy(rest);
            in_data = false;
            if let Some(want) = kv_u64(&rest, "size") {
                if want as usize != part.data.len() {
                    return Err(format!(
                        "yEnc size mismatch: got {} want {want}",
                        part.data.len()
                    ));
                }
            }
            let crc_s = rest.split_whitespace().find_map(|p| {
                p.strip_prefix("pcrc32=")
                    .or_else(|| p.strip_prefix("crc32="))
            });
            if let Some(hex) = crc_s {
                let want =
                    u32::from_str_radix(hex, 16).map_err(|_| "bad yEnc crc32".to_string())?;
                let got = crc32(&part.data);
                if got != want {
                    return Err(format!(
                        "yEnc crc32 mismatch: got {got:08x} want {want:08x}"
                    ));
                }
            }
        } else if in_data {
            let mut i = 0;
            while i < line.len() {
                let mut c = line[i];
                if c == b'=' {
                    i += 1;
                    if i >= line.len() {
                        break;
                    }
                    c = line[i].wrapping_sub(64);
                }
                part.data.push(c.wrapping_sub(42));
                i += 1;
            }
        }
    }
    if !saw_yenc {
        part.data = input.to_vec();
    }
    Ok(part)
}

// ---------- NNTP client ----------

#[derive(Debug, Clone)]
pub struct NntpConfig {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub user: String,
    pub pass: String,
    /// NNTP connections for whole NZB (shared pool across files):
    /// task.segments (Add-dialog snapshot of nntp_connections) wins, else
    /// live nntp_connections setting, separate from HTTP max_connections.
    /// Clamped 1..=64.
    /// ponytail: per-task cap only; N tasks x N conns possible. Strict
    /// provider-account cap = global semaphore, add when needed.
    pub connections: u32,
    /// Auto-unpack rar/zip/7z in out dir after successful download
    /// (Settings.nzb_auto_unpack). Archive deleted only on verified success.
    pub auto_unpack: bool,
    /// All enabled servers with their share of `connections` (pool
    /// construction only; empty = fall back to the primary fields above).
    /// Workers are spawned per endpoint, so article jobs from the shared
    /// queue are pulled by connections on every server and a 430/failure
    /// on one server does not stop workers on the others.
    pub endpoints: Vec<(crate::model::NntpServer, u32)>,
}

impl NntpConfig {
    /// Global server settings; per-task username/password override when set
    /// (override applies to the primary server only; extras keep their own).
    pub fn from_settings(s: &Settings, task: &DownloadTask) -> Self {
        let total = if task.segments > 0 {
            task.segments
        } else {
            s.nntp_connections
        }
        .clamp(1, 64);
        let mut endpoints = s.nntp_server_endpoints(total);
        if let Some((primary, _)) = endpoints.first_mut() {
            if !task.username.is_empty() {
                primary.user = task.username.clone();
            }
            if !task.password.is_empty() {
                primary.pass = task.password.clone();
            }
        }
        Self {
            host: s.nntp_host.clone(),
            port: s.nntp_port,
            tls: s.nntp_tls,
            connections: total,
            auto_unpack: s.nzb_auto_unpack,
            user: if task.username.is_empty() {
                s.nntp_user.clone()
            } else {
                task.username.clone()
            },
            pass: if task.password.is_empty() {
                s.nntp_pass.clone()
            } else {
                task.password.clone()
            },
            endpoints,
        }
    }

    /// Per-endpoint single-server config for `Nntp::connect` (its own
    /// credentials; `connections` = this endpoint's share).
    fn for_endpoint(&self, srv: &crate::model::NntpServer, count: u32) -> Self {
        Self {
            host: srv.host.clone(),
            port: srv.port,
            tls: srv.tls,
            user: srv.user.clone(),
            pass: srv.pass.clone(),
            connections: count,
            auto_unpack: self.auto_unpack,
            endpoints: Vec::new(),
        }
    }
}

enum Wire {
    Plain(tokio::net::TcpStream),
    // Boxed: TlsStream dwarfs TcpStream (clippy large_enum_variant).
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl AsyncRead for Wire {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for Wire {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_write(cx, b),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_write(cx, b),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

struct Nntp {
    rd: BufReader<ReadHalf<Wire>>,
    wr: WriteHalf<Wire>,
    group: Option<String>,
}

impl Nntp {
    async fn connect(cfg: &NntpConfig) -> Result<Self, String> {
        if cfg.host.trim().is_empty() {
            return Err("NNTP server not configured (Options > Connection > NNTP)".into());
        }
        for v in [&cfg.host, &cfg.user, &cfg.pass] {
            if v.contains(['\r', '\n']) {
                return Err("invalid NNTP host/credential".into());
            }
        }
        let tcp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port)),
        )
        .await
        .map_err(|_| format!("NNTP connect {}:{} timed out", cfg.host, cfg.port))?
        .map_err(|e| format!("NNTP connect {}:{}: {e}", cfg.host, cfg.port))?;
        let wire = if cfg.tls {
            use tokio_rustls::rustls;
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from(cfg.host.clone())
                .map_err(|_| "invalid NNTP TLS host name".to_string())?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("NNTP TLS: {e}"))?;
            Wire::Tls(Box::new(tls))
        } else {
            Wire::Plain(tcp)
        };
        let (rd, wr) = tokio::io::split(wire);
        let mut c = Self {
            rd: BufReader::new(rd),
            wr,
            group: None,
        };
        let (code, text) = c.reply().await?;
        if code != 200 && code != 201 {
            return Err(format!("NNTP greeting -> {text}"));
        }
        if !cfg.user.is_empty() {
            let (code, text) = c.cmd(&format!("AUTHINFO USER {}", cfg.user)).await?;
            if code == 381 {
                c.expect(&format!("AUTHINFO PASS {}", cfg.pass), &[281])
                    .await?;
            } else if code != 281 {
                return Err(format!("NNTP AUTHINFO USER -> {text}"));
            }
        }
        Ok(c)
    }

    /// One CRLF line. Polls in 200ms slices so cancel/pause land mid-stall.
    async fn read_line(&mut self, ctrl: &Controls) -> Result<Vec<u8>, String> {
        let mut line = Vec::new();
        loop {
            crate::engine::wait_not_paused(ctrl).await?;
            let buf = match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.rd.fill_buf(),
            )
            .await
            {
                Err(_) => continue,
                Ok(r) => r.map_err(|e| e.to_string())?,
            };
            if buf.is_empty() {
                return if line.is_empty() {
                    Err("NNTP connection closed".into())
                } else {
                    Ok(line)
                };
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let chunk = buf[..=i].to_vec();
                    line.extend_from_slice(&chunk);
                    self.rd.consume(i + 1);
                    return Ok(line);
                }
                None => {
                    let n = buf.len();
                    line.extend_from_slice(buf);
                    self.rd.consume(n);
                    if line.len() > 8 * 1024 * 1024 {
                        return Err("NNTP line too long".into());
                    }
                }
            }
        }
    }

    async fn reply(&mut self) -> Result<(u16, String), String> {
        // Greeting precedes any task Controls polling; no ctrl available here.
        let mut line = Vec::new();
        loop {
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.rd.read_until(b'\n', &mut line),
            )
            .await
            .map_err(|_| "NNTP reply timeout".to_string())?
            .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("NNTP connection closed".into());
            }
            if line.ends_with(b"\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&line).trim_end().to_string();
        let code: u16 = text.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok((code, text))
    }

    async fn cmd(&mut self, c: &str) -> Result<(u16, String), String> {
        self.wr
            .write_all(format!("{c}\r\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.wr.flush().await.map_err(|e| e.to_string())?;
        self.reply().await
    }

    async fn expect(&mut self, c: &str, ok: &[u16]) -> Result<String, String> {
        let (code, text) = self.cmd(c).await?;
        if ok.contains(&code) {
            Ok(text)
        } else {
            Err(format!("NNTP {c} -> {text}"))
        }
    }

    async fn ensure_group(&mut self, group: &str) -> Result<(), String> {
        if group.contains(['\r', '\n']) {
            return Err("invalid NNTP group".into());
        }
        if self.group.as_deref() != Some(group) {
            self.expect(&format!("GROUP {group}"), &[211]).await?;
            self.group = Some(group.to_string());
        }
        Ok(())
    }

    /// BODY <msgid>: 222 + dot-terminated body. 430 = article gone;
    /// caller counts it missing (PAR2 repair may still save the file).
    async fn body(&mut self, msgid: &str, ctrl: &Controls) -> Result<Vec<u8>, String> {
        if msgid.contains(['\r', '\n']) {
            return Err("invalid NZB message-id".into());
        }
        let id = if msgid.starts_with('<') {
            msgid.to_string()
        } else {
            format!("<{msgid}>")
        };
        let (code, text) = self.cmd(&format!("BODY {id}")).await?;
        if code == 430 || code == 423 || code == 424 {
            // Stable marker parsed by `is_missing_article_error`.
            return Err(format!("NNTP article {id} missing ({code}): no such article"));
        }
        if code != 222 {
            return Err(format!("NNTP BODY {id} -> {text}"));
        }
        let mut out = Vec::new();
        loop {
            let raw = self.read_line(ctrl).await?;
            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line == b"." {
                return Ok(out);
            }
            // Dot-unstuffing: leading ".." -> "."
            let line = line
                .strip_prefix(b".")
                .filter(|_| line.starts_with(b".."))
                .unwrap_or(line);
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
}

async fn fetch_nzb_text(client: &reqwest::Client, task: &DownloadTask) -> Result<String, String> {
    let url = task.url.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        let mut req = client.get(url);
        if !task.referrer.is_empty() {
            req = req.header("Referer", &task.referrer);
        }
        if !task.cookies.is_empty() {
            req = req.header("Cookie", &task.cookies);
        }
        for (k, v) in &task.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("NZB fetch HTTP {} for {url}", resp.status()));
        }
        resp.text().await.map_err(|e| e.to_string())
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("NZB read {path}: {e}"))
    }
}

pub async fn probe_nzb(client: reqwest::Client, task: &DownloadTask) -> Result<Nzb, String> {
    let xml = fetch_nzb_text(&client, task).await?;
    parse_nzb(&xml)
}

// ---------- Missing-article accounting & retry policy (pure) ----------

/// Per-article attempts (initial try + retries) for transient errors
/// before an article is declared failed. Missing-article errors
/// (430-class) are definitive and never retried (see
/// `article_failure_action`), so this bounds only non-missing retries.
pub const ARTICLE_ATTEMPTS: u32 = 3;
/// Whole connect (TCP + TLS + greeting + auth) deadline.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// One article fetch (GROUP + BODY) deadline; a silent-but-open
/// connection must not wedge a worker forever.
pub const ARTICLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Coordinator no-progress deadline: workers guarantee one result
/// per job, so silence this long means a wedged pool — fail loudly.
pub const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);
/// Progressive NZB health check: once at least this many articles have
/// been checked, a missing share of >= 20% (1 in 5) aborts the whole
/// download early instead of grinding to the end (user policy: an NZB
/// with 20% of its articles gone is unusable).
pub const HEALTH_MIN_SAMPLE: u64 = 64;

/// Progressive NZB health check (pure). Returns `Some(user-facing
/// abort message)` when the NZB is already provably unusable:
/// `checked` (ok + missing outcomes so far) has reached
/// `HEALTH_MIN_SAMPLE`, `checked < total` (not at the end yet — the
/// final incomplete summary handles that case), and at least 20% of
/// the checked articles are missing (integer math: missing * 5 >=
/// checked, missing > 0). Message carries the observed percentage and
/// counts so it reads clearly in the task error.
pub fn health_abort(missing: u64, checked: u64, total: u64) -> Option<String> {
    if checked < HEALTH_MIN_SAMPLE || checked >= total || missing == 0 {
        return None;
    }
    if missing * 5 >= checked {
        let pct = missing * 100 / checked;
        Some(format!(
            "NZB unusable: {pct}% of articles missing ({missing}/{checked} checked so far) — aborting"
        ))
    } else {
        None
    }
}

/// Ok/missing article counts for one file (folded for the NZB total).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArticleTally {
    pub ok: u64,
    pub missing: u64,
}

impl ArticleTally {
    pub fn record_ok(&mut self) {
        self.ok += 1;
    }
    pub fn record_missing(&mut self) {
        self.missing += 1;
    }
    pub fn total(&self) -> u64 {
        self.ok + self.missing
    }
    pub fn is_complete(&self) -> bool {
        self.missing == 0
    }
}

/// True when an article-fetch error means "article not on server"
/// (NNTP 430, also 423/424 article-number variants / text form),
/// as opposed to a transient failure worth retrying. Matches the
/// stable markers emitted by `Nntp::body`, never bare digits that
/// could appear in a message-id or byte count.
pub fn is_missing_article_error(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    l.contains("no such article")
        || l.contains("(430)")
        || l.contains("-> 430")
        || l.contains("-> 423")
        || l.contains("-> 424")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArticleAction {
    Missing,
    Retry,
    Fail,
}

/// Article fetch failure policy: missing-article errors
/// (430/423/424) count Missing on the FIRST attempt — NNTP
/// availability is definitive, so a 430 means the article is gone and
/// repeated checks will not find it (deliberate user policy: "no need
/// to keep checking if the article appears, nntp is very direct
/// according availability"). Transient non-missing errors retry up to
/// `max_attempts`, then fail the download; cancel never retries.
fn article_failure_action(e: &str, _attempt: u32, max_attempts: u32) -> ArticleAction {
    if e == "cancelled" {
        return ArticleAction::Fail;
    }
    if is_missing_article_error(e) {
        return ArticleAction::Missing;
    }
    if _attempt >= max_attempts.max(1) {
        ArticleAction::Fail
    } else {
        ArticleAction::Retry
    }
}

/// Headline for an NZB that cannot complete: `"NZB incomplete:
/// X/Y articles missing (P%)"` (X missing of Y total, P = integer
/// missing percentage, 0 when total is 0).
pub fn incomplete_summary(missing: u64, total: u64) -> String {
    let pct = missing.checked_mul(100).and_then(|m| m.checked_div(total)).unwrap_or(0);
    format!("NZB incomplete: {missing}/{total} articles missing ({pct}%)")
}

/// Monotonic ms since first use (stall deadlines; immune to wall
/// clock jumps).
fn now_mono_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// True when `last_ms` is at least `window` in the past.
fn deadline_hit(last_ms: u64, window: std::time::Duration) -> bool {
    last_ms != 0
        && now_mono_ms().saturating_sub(last_ms) >= window.as_millis() as u64
}

/// Per-article outcome in the worker pool (module scope so the
/// fetch helper and the `download_nzb` coordinator share it).
#[derive(Debug)]
enum SegOut {
    Ok(Vec<u8>),
    Missing,
    Err(String),
}

/// Fetch one article with bounded retries (see `ARTICLE_ATTEMPTS`/
/// `ARTICLE_TIMEOUT`/`CONNECT_TIMEOUT`). Every failure drops the
/// connection (it may be desynced); missing-article errors
/// (430-class) count Missing immediately — no retries, NNTP
/// availability is definitive. `first_job` preserves the
/// pre-existing contract: the very first article's exhausted connect
/// failure aborts the whole download instead of failing one file.
/// Total latency is bounded by attempts x (connect + article
/// timeouts), and the shared `last_result_at` stall breaker ends the
/// pool.
#[allow(clippy::too_many_arguments)]
async fn fetch_article_with_retry(
    conn: &mut Option<Nntp>,
    cfg: &NntpConfig,
    group: &str,
    seg: &NzbSegment,
    ctrl: &Controls,
    last_result_at: &std::sync::atomic::AtomicU64,
    first_job: bool,
) -> SegOut {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if deadline_hit(
            last_result_at.load(Ordering::Relaxed),
            STALL_TIMEOUT,
        ) {
            return SegOut::Err(format!(
                "NNTP stalled: no articles completed in {}s",
                STALL_TIMEOUT.as_secs()
            ));
        }
        if conn.is_none() {
            match tokio::time::timeout(CONNECT_TIMEOUT, Nntp::connect(cfg)).await {
                Ok(Ok(c)) => *conn = Some(c),
                Ok(Err(e)) => {
                    if first_job && attempt >= ARTICLE_ATTEMPTS {
                        return SegOut::Err(e);
                    }
                    match article_failure_action(&e, attempt, ARTICLE_ATTEMPTS) {
                        ArticleAction::Missing => {
                            last_result_at.store(now_mono_ms(), Ordering::Relaxed);
                            return SegOut::Missing;
                        }
                        ArticleAction::Retry => continue,
                        ArticleAction::Fail => return SegOut::Err(e),
                    }
                }
                Err(_) => {
                    let e = format!("NNTP connect to {} timed out", cfg.host);
                    if attempt >= ARTICLE_ATTEMPTS {
                        return SegOut::Err(e);
                    }
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("conn just ensured");
        let fetched = tokio::time::timeout(ARTICLE_TIMEOUT, async {
            c.ensure_group(group).await?;
            c.body(&seg.id, ctrl).await
        })
        .await;
        match fetched {
            Ok(Ok(b)) => {
                last_result_at.store(now_mono_ms(), Ordering::Relaxed);
                return SegOut::Ok(b);
            }
            Ok(Err(e)) => {
                if e == "cancelled" {
                    return SegOut::Err(e);
                }
                *conn = None;
                match article_failure_action(&e, attempt, ARTICLE_ATTEMPTS) {
                    ArticleAction::Missing => {
                        last_result_at.store(now_mono_ms(), Ordering::Relaxed);
                        return SegOut::Missing;
                    }
                    ArticleAction::Retry => {}
                    ArticleAction::Fail => return SegOut::Err(e),
                }
            }
            Err(_) => {
                *conn = None;
                let e = format!(
                    "NNTP article <{}> timed out after {}s",
                    seg.id,
                    ARTICLE_TIMEOUT.as_secs()
                );
                match article_failure_action(&e, attempt, ARTICLE_ATTEMPTS) {
                    ArticleAction::Missing => {
                        last_result_at.store(now_mono_ms(), Ordering::Relaxed);
                        return SegOut::Missing;
                    }
                    ArticleAction::Retry => {}
                    ArticleAction::Fail => return SegOut::Err(e),
                }
            }
        }
    }
}

/// Download an NZB task: fetch/parse NZB, then per file fetch segments in
/// `number` order, yEnc-decode, append to out dir/<sanitized NZB name>
/// (out dir = dest_dir/<output_name> for multi-file, else dest_dir;
/// task.filename/category/dest_dir updated in place for multi-file).
pub async fn download_nzb(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
    cfg: &NntpConfig,
) -> Result<DownloadTask, String> {
    let xml = fetch_nzb_text(&client, &task).await?;
    let nzb = parse_nzb(&xml)?;
    let multi_pre = nzb.files.len() > 1;
    let out_name = output_name(&nzb, &task.url);
    task.filename = out_name.clone();
    task.category = crate::model::category_for(&out_name).to_string();
    // Multi-file: files live under dest_dir/<out_name> folder. task.dest_dir
    // stays parent (torrent root convention): app dest_dir.join(filename)
    // resolves folder for open/reveal/delete(remove_dir_all).
    let out_dir = if multi_pre {
        task.dest_dir.join(&out_name)
    } else {
        task.dest_dir.clone()
    };
    tokio::fs::create_dir_all(&out_dir)
        .await
        .map_err(|e| e.to_string())?;

    let total_segs: u64 = nzb.files.iter().map(|f| f.segments.len() as u64).sum();
    task.total = Some(nzb.total_bytes);
    ctrl.total.store(nzb.total_bytes, Ordering::Relaxed);
    // Multi-file: one Controls.parts entry per file (decoded bytes done,
    // NZB on-wire bytes total — done snaps to total on completion/skip,
    // mirroring global total/downloaded convention). Single file keeps
    // segment-count convention (parts_are_segments).
    let multi = nzb.files.len() > 1;
    ctrl.parts_are_segments.store(!multi, Ordering::Relaxed);
    *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = if multi {
        nzb.files.iter().map(|f| (0, f.total_bytes)).collect()
    } else {
        vec![(0, total_segs)]
    };
    if multi {
        *ctrl.part_files.lock().unwrap_or_else(|e| e.into_inner()) = nzb
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.name.clone()))
            .collect();
    }
    task.status = TaskStatus::Downloading;

    // ---- Whole-NZB shared connection pool ----
    // Upfront: sidecar-skip every file, then N connections (cfg.connections)
    // shared across ALL files' segments. Coordinator alone writes files in
    // per-file NZB order; files finalize independently (sidecar on complete,
    // 430-fail isolated). ponytail: fetched-but-unwritten segments buffer in
    // RAM (~2*N channel + pending); spool to part files if huge NZBs matter.
    struct FileSt {
        out: Option<tokio::fs::File>,
        dest: std::path::PathBuf,
        state_path: std::path::PathBuf,
        pending: std::collections::BTreeMap<usize, SegOut>,
        next_write: usize,
        written: u64,
        missing: u64,
        max_end: u64,
        pending_hole: bool,
        unrepairable_offset: bool,
        done: bool,
    }
    let mut downloaded: u64 = 0;
    let mut segs_done: u64 = 0;
    // Per-file article accounting (missing counted on the first
    // definitive 430-class answer, no retries; ok on written
    // segments); folded NZB-wide it drives both the progressive
    // health abort (`health_abort`) and the final "NZB incomplete:
    // X/Y articles missing (P%)" failure.
    let mut tallies: Vec<ArticleTally> = nzb.files.iter().map(|_| ArticleTally::default()).collect();
    let mut failed: Vec<(usize, String)> = Vec::new();
    let mut states: Vec<Option<FileSt>> = Vec::new();
    let mut groups: Vec<String> = Vec::new();
    let mut file_jobs: Vec<Vec<(usize, usize)>> = Vec::new();
    for (file_idx, file) in nzb.files.iter().enumerate() {
        let dest = out_dir.join(&file.name);
        let state_path = out_dir.join(format!("{}.nzbstate", file.name));
        let skip = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .filter(|v| {
                v.get("file_idx").and_then(|x| x.as_u64()) == Some(file_idx as u64)
                    && v.get("seg_idx").and_then(|x| x.as_u64()) == Some(file.segments.len() as u64)
            })
            .and_then(|v| v.get("bytes").and_then(|x| x.as_u64()))
            .filter(|&b| std::fs::metadata(&dest).map(|m| m.len()).ok() == Some(b));
        groups.push(
            file.groups
                .first()
                .ok_or_else(|| format!("NZB file '{}' has no <group>", file.name))?
                .clone(),
        );
        if let Some(b) = skip {
            downloaded += b;
            segs_done += file.segments.len() as u64;
            tallies[file_idx].ok = file.segments.len() as u64;
            if multi {
                if let Some(p) = ctrl
                    .parts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(file_idx)
                {
                    *p = (file.total_bytes, file.total_bytes);
                }
            } else if let Some(p) = ctrl
                .parts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_mut()
            {
                p.0 = segs_done;
            }
            states.push(Some(FileSt {
                out: None,
                dest,
                state_path,
                pending: Default::default(),
                next_write: file.segments.len(),
                written: b,
                missing: 0,
                max_end: 0,
                pending_hole: false,
                unrepairable_offset: false,
                done: true,
            }));
            file_jobs.push(Vec::new());
            continue;
        }
        let out = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| e.to_string())?;
        let empty = file.segments.is_empty();
        if empty {
            let _ = std::fs::write(
                &state_path,
                serde_json::json!({"file_idx": file_idx, "seg_idx": 0, "bytes": 0}).to_string(),
            );
        }
        states.push(Some(FileSt {
            out: Some(out),
            dest,
            state_path,
            pending: Default::default(),
            next_write: 0,
            written: 0,
            missing: 0,
            max_end: 0,
            pending_hole: false,
            unrepairable_offset: false,
            done: empty,
        }));
        file_jobs.push((0..file.segments.len()).map(|s| (file_idx, s)).collect());
    }
    ctrl.downloaded.store(downloaded, Ordering::Relaxed);
    task.downloaded = downloaded;
    let total_jobs: usize = file_jobs.iter().map(|j| j.len()).sum();
    if total_jobs > 0 {
        use std::sync::atomic::AtomicBool;
        let queue = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<(
            usize,
            usize,
        )>::new()));
        let pool_done = std::sync::Arc::new(AtomicBool::new(false));
        let dead = std::sync::Arc::new(
            (0..nzb.files.len())
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>(),
        );
        let mut active_dests: std::collections::HashSet<std::path::PathBuf> = Default::default();
        let mut queued_files: std::collections::HashSet<usize> = Default::default();
        {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            for (fi, jobs) in file_jobs.iter().enumerate() {
                let st = states[fi].as_ref().expect("state");
                if st.done || active_dests.contains(&st.dest) {
                    continue;
                }
                active_dests.insert(st.dest.clone());
                queued_files.insert(fi);
                q.extend(jobs.iter().copied());
            }
        }
        // Combined pool: one worker group per enabled server, sized by
        // its share of cfg.connections (model::divide_connections). All
        // groups pull from the same shared queue (work-stealing), so an
        // article missing/failing on one server leaves other servers'
        // workers unaffected. Fallback (directly built cfg): primary only.
        let endpoints: Vec<NntpConfig> = if cfg.endpoints.is_empty() {
            if cfg.host.trim().is_empty() {
                return Err(
                    "no Usenet server configured/enabled (NNTP not configured; Options > Connection > NNTP)".into(),
                );
            }
            vec![cfg.clone()]
        } else {
            cfg.endpoints
                .iter()
                .map(|(srv, c)| cfg.for_endpoint(srv, *c))
                .collect()
        };
        let nworkers: usize = endpoints
            .iter()
            .map(|e| (e.connections as usize).clamp(1, total_jobs))
            .sum::<usize>()
            .clamp(1, total_jobs);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(usize, usize, SegOut)>(nworkers * 2);
        let segs_all = std::sync::Arc::new(
            nzb.files
                .iter()
                .map(|f| f.segments.clone())
                .collect::<Vec<_>>(),
        );
        let groups_a = std::sync::Arc::new(groups.clone());
        // Pool progress clock (mono ms): workers stamp every article
        // outcome; the stall breaker in fetch_article_with_retry and
        // the coordinator no-progress deadlines read it.
        let last_result_at = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now_mono_ms()));
        let mut handles = Vec::new();
        for ecfg in endpoints {
            for _ in 0..(ecfg.connections as usize).clamp(1, total_jobs) {
            let (tx, queue, pool_done, dead, segs_all, groups_a, wctrl, wcfg, last_result_at) = (
                tx.clone(),
                queue.clone(),
                pool_done.clone(),
                dead.clone(),
                segs_all.clone(),
                groups_a.clone(),
                ctrl.clone(),
                ecfg.clone(),
                last_result_at.clone(),
            );
            handles.push(tokio::spawn(async move {
                let mut conn: Option<Nntp> = None;
                loop {
                    let job = {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.pop_front()
                    };
                    let Some((fi, si)) = job else {
                        if pool_done.load(Ordering::Relaxed) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        continue;
                    };
                    if dead[fi].load(Ordering::Relaxed) {
                        continue;
                    }
                    // Connection (with connect timeout) happens inside
                    // fetch_article_with_retry; initial-connect failure
                    // is reported as a per-file Err by the coordinator
                    // instead of silently returning here.
                    let seg = &segs_all[fi][si];
                    let out = fetch_article_with_retry(
                        &mut conn,
                        &wcfg,
                        &groups_a[fi],
                        seg,
                        &wctrl,
                        &last_result_at,
                        fi == 0 && si == 0,
                    )
                    .await;
                    let fatal = matches!(out, SegOut::Err(_));
                    if tx.send((fi, si, out)).await.is_err() || fatal {
                        return;
                    }
                }
                if let Some(mut c) = conn {
                    let _ = c.cmd("QUIT").await;
                }
            }));
            }
        }
        drop(tx);
        let mut fatal: Option<String> = None;
        // Coordinator no-progress clock: declared results also count
        // (ko-progress). Workers bound every job, so a full
        // STALL_TIMEOUT of silence = wedged pool -> fail loudly
        // instead of the old indefinite rx.recv() 0.0% stall.
        let mut coord_last_progress = now_mono_ms();
        loop {
            let remaining = states.iter().flatten().filter(|st| !st.done).count();
            if remaining == 0 {
                break;
            }
            // Pause semantics identical to workers' wait_not_paused:
            // keep waiting, never treat pause as a stall.
            if let Err(e) = crate::engine::wait_not_paused(&ctrl).await {
                pool_done.store(true, Ordering::Relaxed);
                for h in &handles {
                    h.abort();
                }
                if e == "cancelled" {
                    return paused(task, downloaded, e);
                }
                fatal = Some(e);
                break;
            }
            let recv = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                rx.recv(),
            )
            .await;
            let (fi, si, out) = match recv {
                Ok(Some(m)) => {
                    coord_last_progress = now_mono_ms();
                    m
                }
                Ok(None) => {
                    fatal = Some("segment workers stopped".into());
                    break;
                }
                Err(_) => {
                    if ctrl.paused.load(Ordering::Relaxed) {
                        continue;
                    }
                    if deadline_hit(coord_last_progress, STALL_TIMEOUT) {
                        fatal = Some(format!(
                            "NNTP stalled: no articles completed in {}s",
                            STALL_TIMEOUT.as_secs()
                        ));
                        break;
                    }
                    continue;
                }
            };
            if fi == usize::MAX {
                if let SegOut::Err(e) = out {
                    fatal = Some(e);
                }
                break;
            }
            // ---- Progressive NZB health check ----
            // Once >=64 sampled articles are >=20% missing the NZB is
            // unusable: stop the pool instead of grinding to the end.
            // This deliberately aborts before PAR2 repair is considered
            // (user policy — at that loss rate repair cannot save it).
            // NZB-wide aggregation: tallies for every file count
            // sidecar-skipped files as ok (set before the pool starts),
            // and already-written pending results are in tallies; any
            // just-buffered pending ones catch the check next message.
            {
                let agg_missing: u64 = tallies.iter().map(|t| t.missing).sum();
                let agg_checked: u64 = tallies.iter().map(|t| t.total()).sum();
                if let Some(msg) = health_abort(agg_missing, agg_checked, total_segs) {
                    crate::applog::append_default(crate::applog::LV_ERROR, "nntp", &msg);
                    pool_done.store(true, Ordering::Relaxed);
                    for h in &handles {
                        h.abort();
                    }
                    return Err(msg);
                }
            }
            {
                let file = &nzb.files[fi];
                let st = states[fi].as_mut().expect("state");
                if st.done {
                    continue;
                }
                st.pending.insert(si, out);
                while let Some(seg_out) = st.pending.remove(&st.next_write) {
                    let seg = &file.segments[st.next_write];
                    st.next_write += 1;
                    if let Err(e) = crate::engine::wait_not_paused(&ctrl).await {
                        pool_done.store(true, Ordering::Relaxed);
                        for h in &handles {
                            h.abort();
                        }
                        return paused(task, downloaded, e);
                    }
                    match seg_out {
                        SegOut::Missing => {
                            // Retain damaged file for PAR2 repair
                            // (crate::par2). Record hole, write nothing;
                            // workers keep fetching later segments. Next
                            // Ok segment zero-pads to its =ypart begin
                            // (physical bytes only: padding is not added
                            // to `downloaded`); without begin, append
                            // best-effort and flag offset (PAR2 rebuilds
                            // shifted/missing blocks, or verify fails).
                            st.missing += 1;
                            tallies[fi].record_missing();
                            // Partial results still advance the NZB.
                            coord_last_progress = now_mono_ms();
                            st.pending_hole = true;
                        }
                        SegOut::Err(e) => {
                            pool_done.store(true, Ordering::Relaxed);
                            for h in &handles {
                                h.abort();
                            }
                            if e == "cancelled" {
                                if let Some(o) = states[fi].as_mut().expect("state").out.take() {
                                    drop(o);
                                }
                                let dest = states[fi].as_ref().expect("state").dest.clone();
                                let _ = tokio::fs::remove_file(&dest).await;
                                return paused(task, downloaded, e);
                            }
                            return Err(format!("NZB file '{}': {e}", file.name));
                        }
                        SegOut::Ok(body) => {
                            let part = match yenc_decode(&body) {
                                Ok(p) => p,
                                Err(e) => {
                                    pool_done.store(true, Ordering::Relaxed);
                                    for h in &handles {
                                        h.abort();
                                    }
                                    return Err(format!(
                                        "NZB file '{}' segment {}: {e}",
                                        file.name, seg.number
                                    ));
                                }
                            };
                            if let Some(e) = part.end {
                                st.max_end = st.max_end.max(e);
                            }
                            if let Some(fs) = part.file_size {
                                st.max_end = st.max_end.max(fs);
                            }
                            // Zero-pad hole left by missing segment(s):
                            // physical bytes only, not added to `downloaded`.
                            if let Some(begin) = part.begin {
                                let want = begin.saturating_sub(1);
                                if want > st.written {
                                    let o = st.out.as_mut().expect("file open");
                                    let zeros = [0u8; 8192];
                                    let mut pad = want - st.written;
                                    while pad > 0 {
                                        let n = pad.min(zeros.len() as u64) as usize;
                                        o.write_all(&zeros[..n])
                                            .await
                                            .map_err(|e| e.to_string())?;
                                        pad -= n as u64;
                                    }
                                    st.written = want;
                                } else if want < st.written && st.missing == 0 {
                                    pool_done.store(true, Ordering::Relaxed);
                                    for h in &handles {
                                        h.abort();
                                    }
                                    return Err(format!(
                                        "NZB file '{}' segment {}: yEnc offset {begin} != {}",
                                        file.name,
                                        seg.number,
                                        st.written + 1
                                    ));
                                } else if want < st.written {
                                    // Shifted by unknown hole: append
                                    // best-effort; PAR2 decides.
                                    st.unrepairable_offset = true;
                                }
                                st.pending_hole = false;
                            } else if st.pending_hole {
                                st.unrepairable_offset = true;
                                st.pending_hole = false;
                            }
                            let o = st.out.as_mut().expect("file open");
                            o.write_all(&part.data).await.map_err(|e| e.to_string())?;
                            st.written += part.data.len() as u64;
                            downloaded += part.data.len() as u64;
                            segs_done += 1;
                            tallies[fi].record_ok();
                            coord_last_progress = now_mono_ms();
                            ctrl.downloaded.store(downloaded, Ordering::Relaxed);
                            if multi {
                                if let Some(p) = ctrl
                                    .parts
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .get_mut(fi)
                                {
                                    p.0 = st.written.min(p.1);
                                }
                            } else if let Some(p) = ctrl
                                .parts
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .first_mut()
                            {
                                p.0 = segs_done;
                            }
                            task.downloaded = downloaded;
                        }
                    }
                }
                if !st.done && st.next_write >= file.segments.len() && st.missing > 0 {
                    // Damaged file: PAR2 candidate. Pad trailing hole to
                    // known end, flush, retain file, no sidecar.
                    if st.max_end > st.written {
                        let o = st.out.as_mut().expect("file open");
                        let zeros = [0u8; 8192];
                        let mut pad = st.max_end - st.written;
                        while pad > 0 {
                            let n = pad.min(zeros.len() as u64) as usize;
                            o.write_all(&zeros[..n]).await.map_err(|e| e.to_string())?;
                            pad -= n as u64;
                        }
                        st.written = st.max_end;
                    }
                    if let Some(mut o) = st.out.take() {
                        o.flush().await.map_err(|e| e.to_string())?;
                        drop(o);
                    }
                    crate::applog::append_default(
                        crate::applog::LV_WARN,
                        "nntp",
                        &format!(
                            "NZB file '{}': {}/{} articles missing (430)",
                            file.name,
                            tallies[fi].missing,
                            nzb.files[fi].segments.len()
                        ),
                    );
                    failed.push((
                        fi,
                        format!(
                            "{} ({} articles missing, 430)",
                            file.name, tallies[fi].missing
                        ),
                    ));
                    st.done = true;
                }
                if !st.done && st.next_write >= file.segments.len() {
                    if st.written == 0 && !file.segments.is_empty() {
                        pool_done.store(true, Ordering::Relaxed);
                        for h in &handles {
                            h.abort();
                        }
                        return Err(format!("NZB file '{}' assembled empty", file.name));
                    }
                    if let Some(mut o) = st.out.take() {
                        o.flush().await.map_err(|e| e.to_string())?;
                        drop(o);
                    }
                    if multi {
                        if let Some(p) = ctrl
                            .parts
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_mut(fi)
                        {
                            *p = (file.total_bytes, file.total_bytes);
                        }
                    }
                    let _ = tokio::fs::write(
                        &st.state_path,
                        serde_json::json!({"file_idx": fi, "seg_idx": file.segments.len(), "bytes": st.written}).to_string(),
                    )
                    .await;
                    st.done = true;
                }
            }
            {
                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                for (nfi, jobs) in file_jobs.iter().enumerate() {
                    if queued_files.contains(&nfi) {
                        continue;
                    }
                    let st = states[nfi].as_ref().expect("state");
                    if st.done {
                        queued_files.insert(nfi);
                        continue;
                    }
                    let blocked = states
                        .iter()
                        .flatten()
                        .any(|s| !std::ptr::eq(s, st) && !s.done && s.dest == st.dest);
                    if blocked {
                        continue;
                    }
                    queued_files.insert(nfi);
                    q.extend(jobs.iter().copied());
                }
            }
        }
        pool_done.store(true, Ordering::Relaxed);
        for h in &handles {
            h.abort();
        }
        if let Some(e) = fatal {
            if e == "cancelled" {
                return paused(task, downloaded, e);
            }
            return Err(e);
        }
    }
    failed.sort_by_key(|(i, _)| *i);
    // PAR2 repair: set ships a completed .par2 (out_dir also = dest_dir
    // for single-file NZB), so try in-process repair of retained
    // damaged files before failing.
    let mut repair_note: Option<String> = None;
    if !failed.is_empty() {
        let par2_ready = nzb.files.iter().any(|f| {
            f.name.to_ascii_lowercase().ends_with(".par2") && out_dir.join(&f.name).exists()
        });
        if par2_ready {
            let before: Vec<(usize, u64)> = failed
                .iter()
                .map(|(i, _)| {
                    (
                        *i,
                        std::fs::metadata(out_dir.join(&nzb.files[*i].name))
                            .map(|m| m.len())
                            .unwrap_or(0),
                    )
                })
                .collect();
            let dir = out_dir.clone();
            let repair = tokio::task::spawn_blocking(move || crate::par2::repair_dir(&dir)).await;
            match repair {
                Ok(Ok(rep)) if rep.already_ok || rep.files_repaired > 0 => {
                    let mut still: Vec<(usize, String)> = Vec::new();
                    for (fi, msg) in failed.drain(..) {
                        let name = nzb.files[fi].name.clone();
                        let dest = out_dir.join(&name);
                        let Ok(meta) = std::fs::metadata(&dest) else {
                            still.push((fi, msg));
                            continue;
                        };
                        let after = meta.len();
                        let b = before
                            .iter()
                            .find(|(i, _)| *i == fi)
                            .map(|(_, v)| *v)
                            .unwrap_or(0);
                        downloaded = downloaded.saturating_sub(b).saturating_add(after);
                        ctrl.downloaded.store(downloaded, Ordering::Relaxed);
                        task.downloaded = downloaded;
                        if multi {
                            if let Some(p) = ctrl
                                .parts
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_mut(fi)
                            {
                                p.0 = p.1;
                            }
                        }
                        let segs = nzb.files[fi].segments.len();
                        let _ = tokio::fs::write(
                            out_dir.join(format!("{name}.nzbstate")),
                            format!("{{\"file_idx\":{fi},\"seg_idx\":{segs},\"bytes\":{after}}}"),
                        )
                        .await;
                    }
                    failed = still;
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => repair_note = Some(format!("PAR2 repair failed: {e}")),
                Err(e) => repair_note = Some(format!("PAR2 repair failed: {e}")),
            }
        }
    }
    // NZB-level missing totals across every file (recorded even for
    // files PAR2 could not save; skipped/complete files count ok).
    let total_missing: u64 = tallies.iter().map(|t| t.missing).sum();
    let total_articles: u64 = tallies.iter().map(|t| t.total()).sum();
    if !failed.is_empty() {
        let mut msg = if total_missing > 0 {
            format!(
                "{}; {} file(s) incomplete: {}",
                incomplete_summary(total_missing, total_articles),
                failed.len(),
                failed
                    .iter()
                    .map(|(_, s)| s.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        } else {
            format!(
                "NZB: {} file(s) failed: {}",
                failed.len(),
                failed
                    .iter()
                    .map(|(_, s)| s.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        if let Some(n) = repair_note {
            msg.push_str("; ");
            msg.push_str(&n);
        }
        crate::applog::append_default(crate::applog::LV_ERROR, "nntp", &msg);
        return Err(msg);
    }
    if total_missing > 0 {
        // Only reachable when PAR2 repaired every damaged file.
        crate::applog::append_default(
            crate::applog::LV_INFO,
            "nntp",
            &format!(
                "{}; recovered via PAR2 repair",
                incomplete_summary(total_missing, total_articles)
            ),
        );
    }
    // Auto-unpack after PAR2 repair, before Complete: extraction failure
    // returns Err (archive kept, app marks Error/retry, never Complete).
    if cfg.auto_unpack {
        let names: Vec<String> = nzb.files.iter().map(|f| f.name.clone()).collect();
        let dir = out_dir.clone();
        let password = nzb_password(&nzb, &task.url);
        let unpacked = tokio::task::spawn_blocking(move || {
            crate::unpack::unpack_dir(&dir, &names, password.as_deref())
        })
        .await
        .map_err(|e| format!("unpack task failed: {e}"))??;
        if unpacked > 0 {
            eprintln!(
                "nntp: unpacked {unpacked} archive(s) in {}",
                out_dir.display()
            );
        }
    }
    for file in &nzb.files {
        let _ = tokio::fs::remove_file(out_dir.join(format!("{}.nzbstate", file.name))).await;
    }
    task.total = Some(downloaded);
    ctrl.total.store(downloaded, Ordering::Relaxed);
    task.downloaded = downloaded;
    task.status = TaskStatus::Complete;
    Ok(task)
}

fn paused(mut task: DownloadTask, downloaded: u64, _e: String) -> Result<DownloadTask, String> {
    task.status = TaskStatus::Paused;
    task.downloaded = downloaded;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster@example.com" date="1700000000" subject="poster &quot;file1.bin&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group><group>alt.binaries.misc</group></groups>
    <segments>
      <segment bytes="200" number="2">seg2@news</segment>
      <segment bytes="100" number="1">seg1@news</segment>
    </segments>
  </file>
  <file poster="p2" date="1700000100" subject="plain name.rar">
    <groups><group>alt.binaries.test</group></groups>
    <segments><segment bytes="50" number="1">only@news</segment></segments>
  </file>
</nzb>"#;

    #[test]
    fn parse_nzb_sorts_segments_and_totals() {
        let nzb = parse_nzb(SAMPLE).unwrap();
        assert_eq!(nzb.files.len(), 2);
        assert_eq!(nzb.files[0].name, "file1.bin");
        assert_eq!(nzb.files[0].poster, "poster@example.com");
        assert_eq!(nzb.files[0].date, 1700000000);
        assert_eq!(
            nzb.files[0].groups,
            vec!["alt.binaries.test", "alt.binaries.misc"]
        );
        assert_eq!(
            nzb.files[0]
                .segments
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["seg1@news", "seg2@news"]
        );
        assert_eq!(nzb.files[0].segments[0].number, 1);
        assert_eq!(nzb.files[0].total_bytes, 300);
        assert_eq!(nzb.files[1].name, "plain name.rar");
        assert_eq!(nzb.files[1].total_bytes, 50);
        assert_eq!(nzb.total_bytes, 350);
    }

    #[test]
    fn parse_nzb_rejects_bad_docs() {
        assert!(parse_nzb("<MPD/>").is_err());
        assert!(parse_nzb("<nzb").is_err());
        assert!(parse_nzb("<nzb/>").is_err(), "no files");
    }

    fn nzb_for(names: &[&str], meta: &[(&str, &str)]) -> Nzb {
        Nzb {
            files: names
                .iter()
                .map(|n| NzbFile {
                    name: n.to_string(),
                    poster: String::new(),
                    date: 0,
                    groups: Vec::new(),
                    segments: Vec::new(),
                    total_bytes: 0,
                })
                .collect(),
            total_bytes: 0,
            meta: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn output_name_derivation() {
        // Single file: file name verbatim (source/meta ignored).
        assert_eq!(
            output_name(&nzb_for(&["solo.mkv"], &[]), "https://x/movie.nzb"),
            "solo.mkv"
        );
        // Meta title beats name, source stem, common prefix.
        assert_eq!(
            output_name(
                &nzb_for(
                    &["Release.Name.2024.part1.rar", "Release.Name.2024.part2.rar"],
                    &[("title", "My: Release?"), ("name", "Other")]
                ),
                "https://x/Other.nzb"
            ),
            "My_ Release_",
            "meta title wins, sanitized"
        );
        // Meta name (no title) beats source stem.
        assert_eq!(
            output_name(
                &nzb_for(
                    &["Release.Name.2024.part1.rar", "Release.Name.2024.part2.rar"],
                    &[("name", "From Meta")]
                ),
                "https://x/Other.nzb"
            ),
            "From Meta"
        );
        // Source stem: trailing .nzb stripped case-insens.
        assert_eq!(
            output_name(
                &nzb_for(
                    &["Release.Name.2024.part1.rar", "Release.Name.2024.part2.rar"],
                    &[]
                ),
                "https://x/Movie.Pack.NZB?dl=1"
            ),
            "Movie.Pack"
        );
        // Skip-listed stems fall through to common prefix.
        for url in [
            "https://x/get?id=1",
            "https://x/download",
            "https://x/nzb-files.nzb",
            "https://x/",
        ] {
            assert_eq!(
                output_name(
                    &nzb_for(
                        &["Release.Name.2024.part1.rar", "Release.Name.2024.part2.rar"],
                        &[]
                    ),
                    url
                ),
                "Release.Name.2024",
                "url={url} prefix fallback"
            );
        }
        // No usable prefix (<4 after boundary cut): first file stem.
        assert_eq!(
            output_name(&nzb_for(&["ab.rar", "cd.rar"], &[]), "https://x/get?id=1"),
            "ab"
        );
    }

    #[test]
    fn parse_nzb_meta_head() {
        let x = concat!(
            "<nzb><head><meta type=\"Title\">  My Release </meta>",
            "<meta type=\"name\">ignored</meta><meta>NoType</meta>",
            "<meta type=\"empty\">   </meta></head>",
            "<file subject=\"p &quot;a.rar&quot;\"><segments><segment bytes=\"10\" number=\"1\">a@b</segment></segments></file>",
            "<file subject=\"p &quot;b.rar&quot;\"><segments><segment bytes=\"5\" number=\"1\">c@b</segment></segments></file></nzb>"
        );
        let n = parse_nzb(x).unwrap();
        assert_eq!(n.meta.get("title").map(String::as_str), Some("My Release"));
        assert_eq!(n.meta.get("name").map(String::as_str), Some("ignored"));
        assert!(!n.meta.contains_key("empty"));
        assert_eq!(n.meta.len(), 2, "meta without type skipped: {:?}", n.meta);
        // Title meta (case-insens key) wins as output name.
        assert_eq!(output_name(&n, "https://x/get?id=1"), "My Release");
    }

    #[test]
    fn parse_nzb_meta_password_and_braces() {
        let x = concat!(
            "<nzb><head><meta type=\" password \">  s3cret pw </meta></head>",
            "<file subject=\"p &quot;a.rar&quot;\"><segments><segment bytes=\"10\" number=\"1\">a@b</segment></segments></file></nzb>"
        );
        let n = parse_nzb(x).unwrap();
        assert_eq!(
            nzb_password(&n, "https://x/r.nzb").as_deref(),
            Some("s3cret pw")
        );
        // No meta: SABnzbd {{pw}} in NZB filename.
        let plain = parse_nzb(
            "<nzb><file subject=\"p &quot;a.rar&quot;\"><segments><segment bytes=\"10\" number=\"1\">a@b</segment></segments></file></nzb>",
        )
        .unwrap();
        assert_eq!(
            nzb_password(&plain, "https://x/Release%20%7B%7Bfrom-url%7D%7D.nzb").as_deref(),
            Some("from-url")
        );
        assert_eq!(
            nzb_password(&plain, "/tmp/Release {{from-file}}.nzb").as_deref(),
            Some("from-file")
        );
        // File name braces fallback.
        let fnamed = parse_nzb(
            "<nzb><file subject=\"p &quot;Rel {{filepw}}.rar&quot;\"><segments><segment bytes=\"10\" number=\"1\">a@b</segment></segments></file></nzb>",
        )
        .unwrap();
        assert_eq!(
            nzb_password(&fnamed, "https://x/r.nzb").as_deref(),
            Some("filepw")
        );
        // Meta wins over braces.
        assert_eq!(
            nzb_password(&n, "/tmp/{{other}}.nzb").as_deref(),
            Some("s3cret pw")
        );
        // Encoded password content decoded.
        assert_eq!(
            nzb_password(&plain, "https://x/%7B%7Bmy%20pw%7D%7D.nzb").as_deref(),
            Some("my pw")
        );
        // {{pw}} never becomes folder/output name.
        let multi = parse_nzb(
            "<nzb><head><meta type=\"title\">Rel {{topsecret}}</meta></head><file subject=\"p &quot;a.rar&quot;\"><segments><segment bytes=\"1\" number=\"1\">a@b</segment></segments></file><file subject=\"p &quot;b.rar&quot;\"><segments><segment bytes=\"1\" number=\"1\">c@b</segment></segments></file></nzb>",
        )
        .unwrap();
        assert_eq!(output_name(&multi, "https://x/r.nzb"), "Rel");
        // Non-archive file braces must not win over archive file braces.
        let mixed = parse_nzb(
            "<nzb><file subject=\"p &quot;notes {{decoy}}.nfo&quot;\"><segments><segment bytes=\"1\" number=\"1\">a@b</segment></segments></file><file subject=\"p &quot;Rel {{realpw}}.rar&quot;\"><segments><segment bytes=\"1\" number=\"1\">c@b</segment></segments></file></nzb>",
        )
        .unwrap();
        assert_eq!(
            nzb_password(&mixed, "https://x/r.nzb").as_deref(),
            Some("realpw")
        );
    }

    #[test]
    fn missing_article_classification() {
        assert!(is_missing_article_error(
            "NNTP article <a@b> missing (430): no such article"
        ));
        assert!(is_missing_article_error(
            "NNTP article <a@b> missing (423): no such article"
        ));
        assert!(is_missing_article_error("NNTP BODY <a@b> -> 430 No Such Article"));
        assert!(is_missing_article_error("NNTP BODY <a@b> -> 424 Bad Article Number"));
        // Not missing: message-id digits, byte counts, transient errors.
        assert!(!is_missing_article_error("NNTP connect h:563: timed out"));
        assert!(!is_missing_article_error(
            "NNTP article <430bytes@x> timed out after 120s"
        ));
        assert!(!is_missing_article_error("yEnc size mismatch: got 430 want 100"));
        assert!(!is_missing_article_error("cancelled"));
        assert!(!is_missing_article_error(""));
    }

    #[test]
    fn article_first_430_counts_missing_no_retries() {
        let missing_err = "NNTP article <a@b> missing (430): no such article";
        // Definitive on the FIRST attempt — NNTP availability is direct,
        // no reason to re-check a 430/423/424 (user policy).
        assert_eq!(
            article_failure_action(missing_err, 1, ARTICLE_ATTEMPTS),
            ArticleAction::Missing
        );
        assert_eq!(
            article_failure_action(missing_err, 2, ARTICLE_ATTEMPTS),
            ArticleAction::Missing
        );
        assert_eq!(
            article_failure_action(missing_err, 3, ARTICLE_ATTEMPTS),
            ArticleAction::Missing
        );
        assert_eq!(
            article_failure_action(missing_err, 99, ARTICLE_ATTEMPTS),
            ArticleAction::Missing
        );
        // Non-missing: retry then hard fail (never silently missing).
        assert_eq!(
            article_failure_action("connection reset", 1, ARTICLE_ATTEMPTS),
            ArticleAction::Retry
        );
        assert_eq!(
            article_failure_action("connection reset", 3, ARTICLE_ATTEMPTS),
            ArticleAction::Fail
        );
        // Cancel never retries and never counts missing.
        assert_eq!(
            article_failure_action("cancelled", 1, ARTICLE_ATTEMPTS),
            ArticleAction::Fail
        );
        assert_eq!(
            article_failure_action("cancelled", 3, ARTICLE_ATTEMPTS),
            ArticleAction::Fail
        );
        // Degenerate max_attempts still terminates on attempt 1.
        assert_eq!(
            article_failure_action(missing_err, 1, 0),
            ArticleAction::Missing
        );
        assert_eq!(
            article_failure_action("boom", 1, 1),
            ArticleAction::Fail
        );
    }

    #[test]
    fn article_tally_and_incomplete_summary() {
        let mut t = ArticleTally::default();
        assert!(t.is_complete());
        assert_eq!(t.total(), 0);
        t.record_ok();
        t.record_ok();
        t.record_missing();
        assert_eq!(t.ok, 2);
        assert_eq!(t.missing, 1);
        assert_eq!(t.total(), 3);
        assert!(!t.is_complete());
        assert_eq!(
            incomplete_summary(1, 3),
            "NZB incomplete: 1/3 articles missing (33%)"
        );
        assert_eq!(
            incomplete_summary(0, 5544),
            "NZB incomplete: 0/5544 articles missing (0%)"
        );
        assert_eq!(
            incomplete_summary(1100, 5544),
            "NZB incomplete: 1100/5544 articles missing (19%)"
        );
        assert_eq!(
            incomplete_summary(0, 0),
            "NZB incomplete: 0/0 articles missing (0%)"
        );
        // Fold across files (skipped + downloaded + damaged).
        let tallies = [
            ArticleTally { ok: 68, missing: 0 },
            ArticleTally { ok: 60, missing: 8 },
        ];
        let missing: u64 = tallies.iter().map(|t| t.missing).sum();
        let total: u64 = tallies.iter().map(|t| t.total()).sum();
        assert_eq!(
            incomplete_summary(missing, total),
            "NZB incomplete: 8/136 articles missing (5%)"
        );
    }

    #[test]
    fn health_abort_threshold() {
        // Below the 64-article sample: keep going (small NZBs handled by
        // the final incomplete summary instead).
        assert_eq!(health_abort(13, 63, 500), None, "13/63 is 20%+ but unsampled");
        assert!(health_abort(100, 100, 101).is_some(), "100/100 missing, 1 left: abort");
        // No losses at all.
        assert_eq!(health_abort(0, 100, 1000), None);
        // Below 20%: keep going (19/100 = 19%).
        assert_eq!(health_abort(12, 64, 500), None, "12/64 = 18.75%");
        assert_eq!(health_abort(19, 100, 1000), None, "19%");
        // At exactly 20% (missing*5 == checked): abort.
        assert_eq!(
            health_abort(13, 64, 500).as_deref(),
            Some("NZB unusable: 20% of articles missing (13/64 checked so far) — aborting")
        );
        assert!(health_abort(20, 100, 1000).is_some(), "exactly 20%");
        // Sub-20 percent that rounds up via integer math still below:
        // 19/96 = 19.79% -> missing*5=95 < 96, no abort.
        assert_eq!(health_abort(19, 96, 1000), None);
        // At the very end (checked == total): no early abort; the final
        // "NZB incomplete" summary reports it (and PAR2 can still run).
        assert_eq!(health_abort(50, 100, 100), None, "already complete");
        assert_eq!(health_abort(200, 200, 200), None);
        // Message shape: percentage is integer missing*100/checked.
        assert_eq!(
            health_abort(130, 520, 5544).as_deref(),
            Some("NZB unusable: 25% of articles missing (130/520 checked so far) — aborting")
        );
    }

    #[test]
    fn stall_deadline_arithmetic() {
        // 0 = never stamped: no stall (pool just started).
        assert!(!deadline_hit(0, STALL_TIMEOUT));
        // Just stamped: no stall.
        assert!(!deadline_hit(now_mono_ms(), STALL_TIMEOUT));
        // Stamped beyond the window: stall.
        let old = now_mono_ms().saturating_sub(STALL_TIMEOUT.as_millis() as u64 + 60_000);
        if old > 0 {
            assert!(deadline_hit(old, STALL_TIMEOUT));
        }
        assert!(STALL_TIMEOUT > ARTICLE_TIMEOUT);
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// Minimal yEnc encoder for tests (single line body).
    fn encode(data: &[u8], name: &str) -> Vec<u8> {
        let mut out = format!(
            "=ybegin part=1 total=1 line=128 size={} name={name}\r\n",
            data.len()
        )
        .into_bytes();
        for &b in data {
            let c = b.wrapping_add(42);
            if matches!(c, 0 | 9 | 10 | 13 | 61) {
                out.push(b'=');
                out.push(c.wrapping_add(64));
            } else {
                out.push(c);
            }
        }
        out.extend_from_slice(
            format!(
                "\r\n=yend size={} part=1 pcrc32={:08x}\r\n",
                data.len(),
                crc32(data)
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn yenc_roundtrip_with_escapes_and_crc() {
        // Bytes chosen so +42 hits NUL, TAB, LF, CR, '=' critical chars.
        let data: Vec<u8> = (0u16..=255).map(|v| v as u8).collect();
        let part = yenc_decode(&encode(&data, "x.bin")).unwrap();
        assert_eq!(part.data, data);
        assert_eq!(part.name.as_deref(), Some("x.bin"));
    }

    #[test]
    fn yenc_crc_vector_and_bad_crc() {
        let part = yenc_decode(&encode(b"123456789", "v")).unwrap();
        assert_eq!(part.data, b"123456789");
        let mut bad = encode(b"123456789", "v");
        let pos = bad.windows(8).position(|w| w == b"cbf43926").unwrap();
        bad[pos] = b'0';
        assert!(yenc_decode(&bad).is_err(), "corrupt pcrc32 must fail");
    }

    #[test]
    fn yenc_plain_body_passthrough_and_parts() {
        assert_eq!(yenc_decode(b"hello plain").unwrap().data, b"hello plain");
        let mut body =
            b"=ybegin part=2 total=2 line=128 size=4 name=f.bin\r\n=ypart begin=5 end=8\r\n"
                .to_vec();
        // Encode DATA payload lines only (strip nested headers by hand).
        let enc = encode(b"DATA", "f.bin");
        let payload = &enc[enc.windows(2).position(|w| w == b"\r\n").unwrap() + 2..];
        let payload = &payload[..payload.windows(6).position(|w| w == b"\r\n=yen").unwrap()];
        body.extend_from_slice(payload);
        body.extend_from_slice(
            format!("\r\n=yend size=4 part=2 pcrc32={:08x}\r\n", crc32(b"DATA")).as_bytes(),
        );
        let part = yenc_decode(&body).unwrap();
        assert_eq!(part.data, b"DATA");
        assert_eq!(part.begin, Some(5));
        assert_eq!(part.end, Some(8));
    }
}
