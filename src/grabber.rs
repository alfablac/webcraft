use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct GrabberProject {
    pub start_url: String,
    #[serde(default = "def_depth")]
    pub depth: usize,
    #[serde(default = "def_pages")]
    pub max_pages: usize,
    #[serde(default)]
    pub max_files: usize,
    #[serde(default)]
    pub max_total_bytes: u64,
    /// 0=same host,1=same registrable domain (see `registrable_domain`),2=external allowed
    #[serde(default)]
    pub scope: u8,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub mime_allow: Vec<String>,
    #[serde(default)]
    pub include_regex: String,
    #[serde(default)]
    pub exclude_regex: String,
    #[serde(default)]
    pub min_size: u64,
    #[serde(default)]
    pub max_size: u64,
    #[serde(default = "def_true")]
    pub respect_robots: bool,
    #[serde(default)]
    pub crawl_delay_ms: u64,
    #[serde(default)]
    pub mirror: bool,
    /// Levels to follow on hosts other than start host (IDM Grabber: levels on other sites).
    #[serde(default)]
    pub external_depth: usize,
    #[serde(default)]
    pub dont_explore_parent_dirs: bool,
    /// Semicolon-style include filters in GUI; stored split. Matches URL/path substring.
    #[serde(default)]
    pub only_paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Browser session cookie header ("name=value; ..."). Sent only to start
    /// host. Never persisted in exported project JSON by GUI (cleared there).
    #[serde(default)]
    pub cookies: String,
}
fn def_depth() -> usize {
    2
}
fn def_pages() -> usize {
    200
}
fn def_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FoundFile {
    pub url: String,
    pub size: Option<u64>,
    pub mime: String,
    /// Referrer: page on which this file was found (empty if unknown).
    #[serde(default)]
    pub page_url: String,
    pub selected: bool,
}

pub fn canonicalize(u: &Url) -> String {
    let mut c = u.clone();
    c.set_fragment(None);
    // strip default ports handled by Url; lowercase host via Url itself
    c.to_string()
}

fn host_of(u: &Url) -> String {
    u.host_str().unwrap_or("").to_ascii_lowercase()
}
/// Common two-label public suffixes (ponytail: tiny heuristic list, not full PSL;
/// upgrade path: `publicsuffix` crate if coverage matters).
const MULTI_LABEL_SUFFIXES: &[&str] = &[
    "ac.uk", "co.uk", "gov.uk", "org.uk", "com.au", "net.au", "org.au", "co.nz", "com.br",
    "com.cn", "com.tw", "com.hk", "co.jp", "ne.jp", "or.jp", "co.kr", "com.mx", "com.ar", "co.in",
    "com.sg",
];

/// Registrable domain: last 2 labels, or last 3 under a known multi-label suffix
/// (`example.co.uk`, not `co.uk`).
pub fn registrable_domain(host: &str) -> String {
    let labels: Vec<&str> = host
        .trim_end_matches('.')
        .split('.')
        .filter(|s| !s.is_empty())
        .collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    let last_two = labels[labels.len() - 2..].join(".").to_lowercase();
    let take = if MULTI_LABEL_SUFFIXES.contains(&last_two.as_str()) {
        3
    } else {
        2
    };
    labels[labels.len() - take..].join(".")
}

fn domain_of(u: &Url) -> String {
    registrable_domain(&host_of(u))
}

pub fn in_scope(start: &Url, cand: &Url, scope: u8) -> bool {
    match scope {
        0 => host_of(start) == host_of(cand),
        1 => domain_of(start) == domain_of(cand),
        _ => true,
    }
}

fn path_allowed(project: &GrabberProject, canon: &str) -> bool {
    if project
        .exclude_paths
        .iter()
        .any(|x| !x.trim().is_empty() && canon.contains(x.trim()))
    {
        return false;
    }
    let only: Vec<&str> = project
        .only_paths
        .iter()
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .collect();
    if !only.is_empty() && !only.iter().any(|x| canon.contains(x)) {
        return false;
    }
    true
}

fn under_start_dir(start: &Url, cand: &Url) -> bool {
    if host_of(start) != host_of(cand) {
        return true;
    }
    let sp = start.path();
    let dir = sp
        .rsplit_once('/')
        .map(|(d, _)| format!("{d}/"))
        .unwrap_or_else(|| "/".into());
    cand.path().starts_with(&dir) || cand.path() == sp
}

fn ext_of(url: &str) -> String {
    // Extension from last path segment only; dots in host/port/query are not extensions.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let seg = path.rsplit('/').next().unwrap_or("");
    seg.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// First `<base href>` wins per HTML spec; resolved against page URL. Relative links then resolve against it, not the page.
pub fn document_base(html: &str, page: &Url) -> Url {
    let lower = html.to_ascii_lowercase();
    if let Some(idx) = lower.find("<base") {
        let slice = &lower[idx..(idx + 600).min(lower.len())];
        if let Some(ai) = slice.find("href") {
            let after = &slice[ai + 4..];
            if let Some(eq) = after.find('=') {
                let rest = html[idx + ai + 4 + eq + 1..].trim_start();
                let val = if let Some(s) = rest.strip_prefix('"') {
                    s.split('"').next().unwrap_or("")
                } else if let Some(s) = rest.strip_prefix('\'') {
                    s.split('\'').next().unwrap_or("")
                } else {
                    rest.split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("")
                };
                if !val.is_empty() {
                    if let Ok(u) = page.join(val) {
                        return u;
                    }
                }
            }
        }
    }
    page.clone()
}

pub fn extract_links(html: &str, base: &Url) -> (Vec<Url>, Vec<Url>) {
    // returns (page_links, file_links) - heuristic: assets from src/href with extension treated as files via caller anyway; here split <a href> vs asset tags
    let base = &document_base(html, base);
    let mut pages = Vec::new();
    let mut files = Vec::new();
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    // simple tag scanner
    for (tag, attr) in [
        ("a", "href"),
        ("iframe", "src"),
        ("link", "href"),
        ("img", "src"),
        ("img", "srcset"),
        ("script", "src"),
        ("video", "src"),
        ("audio", "src"),
        ("source", "src"),
        ("source", "srcset"),
        ("embed", "src"),
    ] {
        let needle = format!("<{tag}");
        let mut pos = 0;
        while let Some(idx) = lower[pos..].find(&needle).map(|x| x + pos) {
            // find attr within next 500 chars
            let slice = &lower[idx..(idx + 600).min(lower.len())];
            if let Some(ai) = slice.find(&format!("{attr}=")) {
                let rest = &html[idx + ai + attr.len() + 1..];
                let rest_trim = rest.trim_start();
                let val = if let Some(stripped) = rest_trim.strip_prefix('"') {
                    stripped.split('"').next().unwrap_or("")
                } else if let Some(stripped) = rest_trim.strip_prefix('\'') {
                    stripped.split('\'').next().unwrap_or("")
                } else {
                    rest_trim
                        .split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("")
                };
                let vals: Vec<&str> = if attr == "srcset" {
                    val.split(',')
                        .filter_map(|e| e.split_whitespace().next())
                        .collect()
                } else {
                    vec![val]
                };
                for v in vals {
                    if !v.is_empty()
                        && !v.starts_with('#')
                        && !v.starts_with("javascript:")
                        && !v.starts_with("mailto:")
                        && !v.starts_with("data:")
                    {
                        if let Ok(u) = base.join(v) {
                            if u.scheme() == "http" || u.scheme() == "https" {
                                if tag == "a" || tag == "iframe" {
                                    pages.push(u);
                                } else {
                                    files.push(u);
                                }
                            }
                        }
                    }
                }
            }
            pos = idx + needle.len();
        }
        let _ = (bytes, &mut i);
    }
    // URLs assigned by scripts or written as plain text (IDM "Process JavaScript" discovery, static only).
    for needle in ["https://", "http://"] {
        let mut pos = 0;
        while let Some(idx) = lower[pos..].find(needle).map(|x| x + pos) {
            let rest = &html[idx..];
            let val = rest
                .split(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '\\')
                })
                .next()
                .unwrap_or("")
                .trim_end_matches(['.', ',', ';']);
            if let Ok(u) = base.join(val) {
                pages.push(u);
            }
            pos = idx + needle.len();
        }
    }
    // CSS url(...) references in style blocks / inline styles.
    {
        let mut pos = 0;
        while let Some(idx) = lower[pos..].find("url(").map(|x| x + pos) {
            let rest = html[idx + 4..].trim_start().trim_start_matches(['"', '\'']);
            let val = rest.split(['"', '\'', ')']).next().unwrap_or("").trim();
            if !val.is_empty() && !val.starts_with("data:") {
                if let Ok(u) = base.join(val) {
                    files.push(u);
                }
            }
            pos = idx + 4;
        }
    }
    (pages, files)
}

/// Exact wildcard match: `*` matches any sequence (incl. empty), all else literal.
fn robots_wildcard(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                c => c == t[j - 1] && dp[i - 1][j - 1],
            };
        }
    }
    dp[p.len()][t.len()]
}

fn robots_pattern_matches(pat: &str, path: &str) -> bool {
    // `$` at end anchors to end of path; otherwise pattern is a prefix pattern.
    if let Some(anchored) = pat.strip_suffix('$') {
        robots_wildcard(anchored, path)
    } else {
        robots_wildcard(&format!("{pat}*"), path)
    }
}

/// Robots.txt check for `User-agent: *` group only. `Allow`/`Disallow` both
/// supported; longest matching pattern wins, `Allow` wins ties. Patterns may
/// contain `*` wildcards and a trailing `$` end anchor.
pub fn robots_allows(robots_txt: &str, path: &str) -> bool {
    let mut in_star = false;
    let mut best: Option<(usize, bool)> = None; // (pattern len, allowed)
    for line in robots_txt.lines() {
        let l = line.trim();
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("user-agent:") {
            in_star = l[11..].trim() == "*";
        } else if in_star && (lower.starts_with("disallow:") || lower.starts_with("allow:")) {
            let is_allow = lower.starts_with("allow:");
            let p = l[if is_allow { 6 } else { 9 }..].trim();
            if p.is_empty() {
                continue;
            }
            if robots_pattern_matches(p, path) {
                let better = match best {
                    None => true,
                    Some((len, allowed)) => {
                        p.len() > len || (p.len() == len && is_allow && !allowed)
                    }
                };
                if better {
                    best = Some((p.len(), is_allow));
                }
            }
        }
    }
    best.map(|(_, allowed)| allowed).unwrap_or(true)
}

pub fn normalize_start_url(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    match Url::parse(raw) {
        Ok(u) if u.scheme() == "http" || u.scheme() == "https" => Ok(u),
        Ok(u) => Err(format!("unsupported start URL scheme {}", u.scheme())),
        Err(_) if !raw.contains("://") => {
            Url::parse(&format!("https://{raw}")).map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

const CRAWL_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

pub struct Crawler {
    pub project: GrabberProject,
    pub progress: Option<std::sync::Arc<std::sync::Mutex<CrawlProgress>>>,
}

/// True for SharePoint/OneDrive library pages. Static HTML there is JS shell;
/// file list only exists via REST API (needs session cookies).
fn is_sharepoint_page(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("");
    if !host.ends_with(".sharepoint.com") {
        return false;
    }
    let p = url.path().to_ascii_lowercase();
    let has_id = url.query_pairs().any(|(k, _)| k.eq_ignore_ascii_case("id"));
    (p.contains("onedrive.aspx")
        || p.ends_with("/allitems.aspx")
        || p.ends_with("/forms/allitems.aspx"))
        && has_id
}

/// Server-relative path from SP item: classic ServerRelativeUrl string, or
/// newer ServerRelativePath {DecodedUrl} object. Empty if neither.
fn sp_item_path(it: &serde_json::Value) -> &str {
    if let Some(s) = it.get("ServerRelativeUrl").and_then(|x| x.as_str()) {
        if !s.is_empty() {
            return s;
        }
    }
    it.get("ServerRelativePath")
        .and_then(|x| x.get("DecodedUrl"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
}

/// Percent-encode one URL path-ish value, keeping `/` and given extra chars.
fn pct_encode(s: &str, keep: &[u8]) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ if keep.contains(&b) => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// (web_url, server-relative folder) from .../_layouts/15/onedrive.aspx?id=%2F...
fn sharepoint_root(url: &Url) -> Result<(String, String), String> {
    let id = url
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("id"))
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
        .ok_or("SharePoint URL missing id= folder parameter")?;
    let path = url.path();
    let web_path = path
        .to_ascii_lowercase()
        .find("/_layouts/")
        .map(|i| &path[..i])
        .unwrap_or("");
    let web = format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        web_path
    );
    Ok((web, id))
}

/// One SP REST collection page: (items, next_url). Handles nometadata
/// {value:[...]} and verbose {d:{results:[...],__next}} shapes.
fn sp_page(v: &serde_json::Value) -> (Vec<serde_json::Value>, Option<String>) {
    if let Some(a) = v.get("value").and_then(|x| x.as_array()) {
        return (
            a.clone(),
            v.get("@odata.nextLink")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        );
    }
    if let Some(d) = v.get("d") {
        if let Some(a) = d.get("results").and_then(|x| x.as_array()) {
            return (
                a.clone(),
                d.get("__next").and_then(|x| x.as_str()).map(str::to_string),
            );
        }
        if d.is_object() {
            return (vec![d.clone()], None);
        }
    }
    (Vec::new(), None)
}

/// Percent-encode SP download SourceUrl value (keep `/` readable).
fn sp_download_url(web: &str, server_relative: &str) -> String {
    format!(
        "{web}/_layouts/15/download.aspx?SourceUrl={}",
        pct_encode(server_relative, &[])
    )
}

/// GetFolderByServerRelativeUrl endpoint for folder. OData literal: `'`
/// doubled, rest percent-encoded so `# ? % \` in names can't break URL.
fn sp_folder_base(web: &str, folder: &str) -> String {
    let lit = pct_encode(&folder.replace('\'', "''"), b"'");
    format!("{web}/_api/web/GetFolderByServerRelativeUrl('{lit}')")
}

/// GET one SP REST endpoint, following @odata.nextLink/__next paging.
async fn sp_get_all(
    client: &reqwest::Client,
    first_url: String,
    cookies: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::new();
    let mut next = Some(first_url);
    let mut guard = 0usize;
    while let Some(u) = next {
        guard += 1;
        if guard > 100 {
            break; // ponytail: 100 pages/collection cap, raise if huge libraries truncated
        }
        let mut req = client
            .get(&u)
            .header(reqwest::header::USER_AGENT, CRAWL_UA)
            .header(reqwest::header::ACCEPT, "application/json;odata=nometadata");
        if !cookies.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookies);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "SharePoint API HTTP {} for {u} — sign in via browser, use extension Scrap site (cookies expire), or paste fresh Cookies",
                resp.status()
            ));
        }
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        let (items, nx) = sp_page(&v);
        out.extend(items);
        next = nx;
    }
    Ok(out)
}

/// Live crawl state, updated per page/file so GUI shows results as depth grows (IDM Grabber behavior).
#[derive(Default, Clone)]
pub struct CrawlProgress {
    pub pages_done: usize,
    pub pages_queued: usize,
    pub depth: usize,
    pub files: Vec<FoundFile>,
    pub current_url: String,
    pub error: Option<String>,
}

impl Crawler {
    fn publish(
        &self,
        pages: &[String],
        queue_len: usize,
        depth: usize,
        files: &[FoundFile],
        current: &str,
    ) {
        if let Some(p) = &self.progress {
            let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
            g.pages_done = pages.len();
            g.pages_queued = queue_len;
            g.depth = depth;
            g.files = files.to_vec();
            g.current_url = current.to_string();
        }
    }
    /// SharePoint/OneDrive folder listing via REST. Static crawl sees only
    /// JS shell + login redirect, so branch before it. Cookies required for
    /// private tenants; public share links may work without.
    async fn crawl_sharepoint(
        &self,
        client: &reqwest::Client,
        start: &Url,
    ) -> Result<(Vec<String>, Vec<FoundFile>), String> {
        let (web, root) = sharepoint_root(start)?;
        let page_url = start.to_string();
        let mut files: Vec<FoundFile> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut folders: VecDeque<String> = VecDeque::new();
        folders.push_back(root);
        let mut pages = 0usize;
        let mut errors: Vec<String> = Vec::new();
        // ponytail: 500-folder cap; raise when deep libraries need it.
        while let Some(folder) = folders.pop_front() {
            pages += 1;
            if pages > 500 {
                break;
            }
            let base = sp_folder_base(&web, &folder);
            for kind in ["Files", "Folders"] {
                // One bad subfolder must not abort whole crawl / wipe results.
                let items = match sp_get_all(
                    client,
                    format!("{base}/{kind}"),
                    self.project.cookies.trim(),
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(format!("{folder}/{kind}: {e}"));
                        continue;
                    }
                };
                for it in items {
                    let p = sp_item_path(&it);
                    if p.is_empty() {
                        continue;
                    }
                    if kind == "Folders" {
                        // Skip only SP system folder; `_`-prefixed user folders legit.
                        if !p
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .eq_ignore_ascii_case("forms")
                        {
                            folders.push_back(p.to_string());
                        }
                        continue;
                    }
                    if !seen.insert(p.to_ascii_lowercase()) {
                        continue;
                    }
                    if !self.project.extensions.is_empty() {
                        let name = p.rsplit('/').next().unwrap_or("");
                        let ext = name
                            .rsplit_once('.')
                            .map(|(_, e)| e.to_ascii_lowercase())
                            .unwrap_or_default();
                        if !self
                            .project
                            .extensions
                            .iter()
                            .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
                        {
                            continue;
                        }
                    }
                    files.push(FoundFile {
                        url: sp_download_url(&web, p),
                        size: it.get("Length").and_then(|x| {
                            x.as_u64()
                                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
                        }),
                        mime: String::new(),
                        page_url: page_url.clone(),
                        selected: true,
                    });
                }
            }
            self.publish(
                std::slice::from_ref(&page_url),
                folders.len(),
                0,
                &files,
                &folder,
            );
        }
        if files.is_empty() {
            let detail = if errors.is_empty() {
                "empty folder or wrong id= path".to_string()
            } else {
                errors.join(" · ")
            };
            return Err(format!("SharePoint listed 0 files ({detail}). For private folders: open folder signed-in in browser, then extension Scrap site with webcraft (sends session cookies)."));
        }
        Ok((vec![page_url], files))
    }

    pub async fn crawl(
        &self,
        client: &reqwest::Client,
    ) -> Result<(Vec<String>, Vec<FoundFile>), String> {
        let start = normalize_start_url(&self.project.start_url)?;
        if is_sharepoint_page(&start) {
            return self.crawl_sharepoint(client, &start).await;
        }
        let mut robots = String::new();
        if self.project.respect_robots {
            let robots_url = start.join("/robots.txt").map_err(|e| e.to_string())?;
            let mut rb = client
                .get(robots_url)
                .header(reqwest::header::USER_AGENT, CRAWL_UA);
            if !self.project.cookies.trim().is_empty() {
                rb = rb.header(reqwest::header::COOKIE, self.project.cookies.trim());
            }
            if let Ok(r) = rb.send().await {
                if r.status().is_success() {
                    robots = r.text().await.unwrap_or_default();
                }
            }
        }
        let include_re = compile_opt(&self.project.include_regex);
        let exclude_re = simple_re(&self.project.exclude_regex);
        let _ = include_re;
        let mut visited: HashSet<String> = HashSet::new();
        // (url, depth, external_depth, is_start, referring page, leaf: fetch but never expand links)
        let mut queue: VecDeque<(Url, usize, usize, bool, String, bool)> = VecDeque::new();
        queue.push_back((start.clone(), 0, 0, true, String::new(), false));
        visited.insert(canonicalize(&start));
        let mut pages: Vec<String> = Vec::new();
        let mut files: Vec<FoundFile> = Vec::new();
        let mut seen_files: BTreeSet<String> = BTreeSet::new();
        let mut total_bytes: u64 = 0;
        while let Some((url, depth, ext_depth, is_start, referrer, leaf)) = queue.pop_front() {
            if pages.len() >= self.project.max_pages {
                break;
            }
            if self.project.respect_robots && !robots_allows(&robots, url.path()) {
                continue;
            }
            if self.project.crawl_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.project.crawl_delay_ms,
                ))
                .await;
            }
            // Cookies only to start host: scope=Any must not leak session.
            let mut page_req = client
                .get(url.clone())
                .header(reqwest::header::USER_AGENT, CRAWL_UA)
                .header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,*/*",
                );
            if !self.project.cookies.trim().is_empty() && url.host_str() == start.host_str() {
                page_req = page_req.header(reqwest::header::COOKIE, self.project.cookies.trim());
            }
            let resp =
                match tokio::time::timeout(std::time::Duration::from_secs(30), page_req.send())
                    .await
                {
                    Err(_) => {
                        if is_start {
                            return Err("start page request timed out".into());
                        }
                        continue;
                    }
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        if is_start {
                            return Err(format!("start page request failed: {e}"));
                        }
                        continue;
                    }
                };
            if !resp.status().is_success() {
                if is_start {
                    return Err(format!("start page returned HTTP {}", resp.status()));
                }
                continue;
            }
            let url = resp.url().clone();
            visited.insert(canonicalize(&url));
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html")
                .to_string();
            let size = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok());
            let attachment = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("attachment")
                });
            let is_html = ct.to_ascii_lowercase().contains("html") && !attachment;
            if !is_html {
                // A short URL (including the start URL) can redirect straight to a file.
                // Inspect headers only; dropping the response avoids consuming its body.
                let canon = canonicalize(&url);
                let ext = ext_of(&canon);
                if !ext.is_empty()
                    && !self.project.extensions.is_empty()
                    && !self
                        .project
                        .extensions
                        .iter()
                        .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
                {
                    continue;
                }
                if !path_allowed(&self.project, &canon) {
                    continue;
                }
                if exclude_re.as_ref().is_some_and(|re| re.is_match(&canon)) {
                    continue;
                }
                if !self.project.include_regex.is_empty() {
                    if let Some(re) = &include_re {
                        if !re.is_match(&canon) {
                            continue;
                        }
                    }
                }
                if seen_files.contains(&canon) {
                    continue;
                }
                if self.project.min_size > 0 {
                    match size {
                        Some(z) if z >= self.project.min_size => {}
                        _ => continue,
                    }
                }
                if self.project.max_size > 0 {
                    if let Some(z) = size {
                        if z > self.project.max_size {
                            continue;
                        }
                    }
                }
                if !self.project.mime_allow.is_empty()
                    && !self.project.mime_allow.iter().any(|m| ct.contains(m))
                {
                    continue;
                }
                if self.project.max_files > 0 && files.len() >= self.project.max_files {
                    continue;
                }
                if let Some(z) = size {
                    total_bytes += z;
                    if self.project.max_total_bytes > 0
                        && total_bytes > self.project.max_total_bytes
                    {
                        continue;
                    }
                }
                seen_files.insert(canon.clone());
                files.push(FoundFile {
                    url: canon,
                    size,
                    mime: ct.clone(),
                    page_url: referrer,
                    selected: true,
                });
                self.publish(&pages, queue.len(), depth, &files, url.as_str());
                continue;
            }
            pages.push(canonicalize(&url));
            self.publish(&pages, queue.len(), depth, &files, url.as_str());
            if leaf {
                continue;
            }
            let html = resp.text().await.unwrap_or_default();
            // page text exclude/include via substring check on html if regex is plain word (regex compiled via simple contains fallback)
            let (links, assets) = links_with_doc_base(&html, &url);
            let mut candidates: Vec<Url> = Vec::new();
            candidates.extend(links.iter().cloned());
            candidates.extend(assets.iter().cloned());
            let mut pending_files: Vec<Url> = Vec::new();
            for cand in candidates {
                let canon = canonicalize(&cand);
                let ext = ext_of(&canon);
                let is_page_like = ext.is_empty()
                    || ["html", "htm", "php", "asp", "aspx", "jsp", "cgi"].contains(&ext.as_str())
                    || cand.path().ends_with('/');
                if !path_allowed(&self.project, &canon) {
                    continue;
                }
                if let Some(re) = &exclude_re {
                    if re.is_match(&canon) {
                        continue;
                    }
                }
                if is_page_like {
                    if !in_scope(&start, &cand, self.project.scope) {
                        continue;
                    }
                    // IDM: "levels on other sites" counts hops past the start domain, not host.
                    let same_site = host_of(&start).eq_ignore_ascii_case(&host_of(&cand))
                        || registrable_domain(&host_of(&start))
                            == registrable_domain(&host_of(&cand));
                    let (nd, ne) = if same_site {
                        (depth + 1, ext_depth)
                    } else {
                        (depth, ext_depth + 1)
                    };
                    let blocked_parent =
                        self.project.dont_explore_parent_dirs && !under_start_dir(&start, &cand);
                    if (same_site && depth < self.project.depth
                        || !same_site && ext_depth < self.project.external_depth)
                        && !visited.contains(&canon)
                    {
                        // Parent-dir guard: fetch linked page once (may be a file endpoint) but never expand its links.
                        visited.insert(canon);
                        queue.push_back((cand, nd, ne, false, url.to_string(), blocked_parent));
                    }
                } else {
                    if !in_scope(&start, &cand, self.project.scope) {
                        continue;
                    }
                    if !self.project.extensions.is_empty()
                        && !self
                            .project
                            .extensions
                            .iter()
                            .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
                    {
                        continue;
                    }
                    if !self.project.include_regex.is_empty() {
                        if let Some(re) = &include_re {
                            if !re.is_match(&canon) {
                                continue;
                            }
                        }
                    }
                    if seen_files.contains(&canon) {
                        continue;
                    }
                    seen_files.insert(canon.clone());
                    pending_files.push(cand);
                }
            }
            // Bounded parallel metadata probes: one slow asset cannot stall page discovery.
            for chunk in pending_files.chunks(8) {
                let mut set = tokio::task::JoinSet::new();
                for (i, cand) in chunk.iter().cloned().enumerate() {
                    let client = client.clone();
                    let cookies = if cand.host_str() == start.host_str() {
                        self.project.cookies.trim().to_string()
                    } else {
                        String::new()
                    };
                    set.spawn(async move {
                        let info = head_info(&client, &cand, &cookies).await;
                        (i, cand, info)
                    });
                }
                let mut got: Vec<(usize, Url, (Option<u64>, String))> = Vec::new();
                while let Some(r) = set.join_next().await {
                    if let Ok(v) = r {
                        got.push(v);
                    }
                }
                got.sort_by_key(|(i, _, _)| *i);
                for (_, cand, (size, mime)) in got {
                    if self.project.min_size > 0 {
                        match size {
                            Some(z) if z >= self.project.min_size => {}
                            _ => continue,
                        }
                    }
                    if self.project.max_size > 0 {
                        if let Some(z) = size {
                            if z > self.project.max_size {
                                continue;
                            }
                        }
                    }
                    if !self.project.mime_allow.is_empty()
                        && !mime.is_empty()
                        && !self.project.mime_allow.iter().any(|m| mime.contains(m))
                    {
                        continue;
                    }
                    if self.project.max_files > 0 && files.len() >= self.project.max_files {
                        continue;
                    }
                    if let Some(z) = size {
                        total_bytes += z;
                        if self.project.max_total_bytes > 0
                            && total_bytes > self.project.max_total_bytes
                        {
                            continue;
                        }
                    }
                    files.push(FoundFile {
                        url: canonicalize(&cand),
                        size,
                        mime,
                        page_url: url.to_string(),
                        selected: true,
                    });
                    self.publish(&pages, queue.len(), depth, &files, url.as_str());
                }
            }
        }
        Ok((pages, files))
    }
}

async fn head_info(client: &reqwest::Client, url: &Url, cookies: &str) -> (Option<u64>, String) {
    let mut req = client
        .head(url.clone())
        .header(reqwest::header::USER_AGENT, CRAWL_UA);
    if !cookies.is_empty() {
        req = req.header(reqwest::header::COOKIE, cookies);
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), req.send())
        .await
        .map_err(|_| "timeout".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
    {
        Ok(r) => {
            let size = r
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok());
            let mime = r
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (size, mime)
        }
        Err(_) => (None, String::new()),
    }
}

// Tiny regex via `regex` not in deps: use substring-or-pattern matching implemented as literal contains OR basic wildcard.
// ponytail: full regex via std only is unavailable; treat pattern as literal substring if it has no regex meta, else use simple wildcard matching.
pub struct SimpleRe(String);
impl SimpleRe {
    fn is_match(&self, s: &str) -> bool {
        wildcard_match(&self.0, s) || s.contains(&self.0)
    }
}
fn simple_re(p: &str) -> Option<SimpleRe> {
    if p.is_empty() {
        None
    } else {
        Some(SimpleRe(p.to_string()))
    }
}
fn compile_opt(p: &str) -> Option<SimpleRe> {
    simple_re(p)
}
pub fn wildcard_match(pat: &str, text: &str) -> bool {
    // supports * and ? and substring for plain patterns; also strips ^ $ anchors common in filters
    let pat = pat.trim_start_matches('^').trim_end_matches('$');
    if !pat.contains('*') && !pat.contains('?') && !pat.contains('.') {
        return text.contains(pat);
    }
    // convert escaped dot
    let pat = pat.replace("\\.", "\u{1}");
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // classic DP wildcard where '.' in pattern without escape means any char? Keep '.' literal except \. case: treat '.' as any single char only if pattern looks regex-ish (contains .* )
    let regexish = pat.contains(".*");
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                '\u{1}' => t[j - 1] == '.' && dp[i - 1][j - 1],
                '.' if regexish => dp[i - 1][j - 1],
                c => c == t[j - 1] && dp[i - 1][j - 1],
            };
        }
    }
    // substring semantics for exclude filters
    dp[p.len()][t.len()] || text.contains(&pat.replace('\u{1}', "."))
}

/// Local path for mirror: preserve folder structure.
pub fn mirror_rel_path(_root: &Url, u: &Url) -> String {
    let host = host_of(u);
    let mut p = format!("{host}{}", u.path());
    if p.ends_with('/') {
        p.push_str("index.html");
    } else if !p.contains('.') {
        p.push_str("/index.html");
    }
    // Query variants of same path collide on disk: tag filename with short query hash.
    if let Some(q) = u.query().filter(|q| !q.is_empty()) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        q.hash(&mut h);
        let hex = format!("{:016x}", h.finish());
        let seg_start = p.rfind('/').map(|i| i + 1).unwrap_or(0);
        if let Some(d) = p[seg_start..].rfind('.') {
            p.insert_str(seg_start + d, &format!("-{}", &hex[..8]));
        } else {
            p.push_str(&format!("-{}", &hex[..8]));
        }
    }
    // sanitize per segment
    p.split('/')
        .map(crate::model::sanitize_filename)
        .collect::<Vec<_>>()
        .join("/")
}

/// Relative path from page file `from_rel` (e.g. `host/a/index.html`) to target
/// `to_rel` (e.g. `host/pic.png`) -> `../pic.png`. Pure string math, `/` separators.
pub fn relative_link(from_rel: &str, to_rel: &str) -> String {
    let from_dir: Vec<&str> = from_rel.split('/').collect();
    let from_dir = &from_dir[..from_dir.len().saturating_sub(1)];
    let to: Vec<&str> = to_rel.split('/').collect();
    let mut common = 0;
    while common < from_dir.len()
        && common < to.len().saturating_sub(1)
        && from_dir[common] == to[common]
    {
        common += 1;
    }
    let mut out = String::new();
    for _ in common..from_dir.len() {
        out.push_str("../");
    }
    out.push_str(&to[common..].join("/"));
    if out.is_empty() {
        out = to.last().copied().unwrap_or("").to_string();
    }
    out
}

/// Mirror: crawl then download pages+files preserving structure, rewrite absolute URLs to relative local paths.
pub async fn mirror_site(
    client: &reqwest::Client,
    project: &GrabberProject,
    out_dir: &std::path::Path,
) -> Result<Vec<String>, String> {
    let crawler = Crawler {
        project: project.clone(),
        progress: None,
    };
    let (pages, files) = crawler.crawl(client).await?;
    let start = normalize_start_url(&project.start_url)?;
    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut downloaded: Vec<String> = Vec::new();
    let mut jobs: Vec<String> = pages.clone();
    for f in &files {
        jobs.push(f.url.clone());
    }
    for u in &jobs {
        let Ok(parsed) = Url::parse(u) else { continue };
        let rel = mirror_rel_path(&start, &parsed);
        mapping.insert(u.clone(), rel.clone());
    }
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    for u in &jobs {
        let Ok(parsed) = Url::parse(u) else { continue };
        let rel = mirror_rel_path(&start, &parsed);
        let dest = out_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let resp = client
            .get(parsed.clone())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            continue;
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if ct.contains("html") {
            let html = String::from_utf8_lossy(&bytes).into_owned();
            // Per-page mapping: values relative to this page, so file:// browsing works at any depth.
            let page_map: HashMap<String, String> = mapping
                .iter()
                .map(|(u, r)| (u.clone(), relative_link(&rel, r)))
                .collect();
            let out = rewrite_links(&html, &parsed, &page_map, out_dir, &rel);
            std::fs::write(&dest, out).map_err(|e| e.to_string())?;
        } else {
            std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
        }
        downloaded.push(dest.display().to_string());
    }
    Ok(downloaded)
}

/// Resolve links against the document's `<base href>` when present (falls back to page URL), matching browser semantics.
pub fn links_with_doc_base(html: &str, page_url: &Url) -> (Vec<Url>, Vec<Url>) {
    extract_links(html, &document_base(html, page_url))
}

pub fn rewrite_links(
    html: &str,
    page_url: &Url,
    mapping: &HashMap<String, String>,
    mirror_root: &std::path::Path,
    page_rel: &str,
) -> String {
    // Source <base href> must not survive: mirrored output carries its own base; relative hrefs were resolved against source base for mapping.
    let doc_base = document_base(html, page_url);
    let lower0 = html.to_ascii_lowercase();
    let html_owned;
    let html = if let Some(i) = lower0.find("<base") {
        let mut s = html.to_string();
        if let Some(j) = s[i..].find('>') {
            s.replace_range(i..=i + j, "");
        }
        html_owned = s;
        &html_owned
    } else {
        html
    };
    let _ = (mirror_root, page_rel);
    // Resolve href/src values (relative, root-relative, absolute) against the
    // page URL first, then swap in the mapped mirror path.
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    'scan: while !rest.is_empty() {
        // Find earliest href/src/srcset attr, case-insensitive, spaces around '=' allowed.
        // (needle search on lowercased copy; offsets align for ASCII HTML markup.)
        let lower = rest.to_ascii_lowercase();
        let mut hit: Option<(usize, usize, Option<char>, bool)> = None; // value start,len, quote, is_srcset
        for attr in ["href", "srcset", "src"] {
            let mut pos = 0;
            while let Some(idx) = lower[pos..].find(attr).map(|x| x + pos) {
                let after = idx + attr.len();
                let tail = lower[after..].trim_start();
                if tail.starts_with('=') {
                    let eq_end = after + (lower[after..].len() - tail.len()) + 1;
                    let valpart = lower[eq_end..].trim_start();
                    let vstart = eq_end + (lower[eq_end..].len() - valpart.len());
                    if valpart.starts_with('"') || valpart.starts_with('\'') {
                        let quote = valpart.chars().next();
                        if hit.is_none_or(|(s, _, _, _)| vstart + 1 < s) {
                            hit = Some((vstart + 1, attr.len(), quote, attr == "srcset"));
                        }
                    } else if !valpart.is_empty() && hit.is_none_or(|(s, _, _, _)| vstart < s) {
                        hit = Some((vstart, attr.len(), None, attr == "srcset"));
                    }
                    break;
                }
                pos = after;
            }
        }
        let Some((vstart, _, quote, is_srcset)) = hit else {
            break 'scan;
        };
        let (vend, vend_skip) = match quote {
            Some(q) => match rest[vstart..].find(q) {
                Some(i) => (i + vstart, 1),
                None => break 'scan,
            },
            None => (
                rest[vstart..]
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .map(|i| i + vstart)
                    .unwrap_or(rest.len()),
                0,
            ),
        };
        let val = &rest[vstart..vend];
        let lookup = |u: &str| -> Option<String> {
            if u.is_empty()
                || u.starts_with(['#', '?'])
                || ["javascript:", "mailto:", "data:", "tel:"]
                    .iter()
                    .any(|s| u.to_ascii_lowercase().starts_with(s))
            {
                return None;
            }
            // Browser resolves href/src against <base href> if present, else page URL.
            doc_base.join(u).ok().and_then(|resolved| {
                mapping
                    .get(&resolved.to_string())
                    .or_else(|| mapping.get(&canonicalize(&resolved)))
                    .cloned()
            })
        };
        let replacement = if is_srcset {
            let mut changed = false;
            let newv = val
                .split(',')
                .map(|entry| {
                    let e = entry.trim();
                    let mut it = e.splitn(2, char::is_whitespace);
                    let u = it.next().unwrap_or("");
                    match lookup(u) {
                        Some(r) => {
                            changed = true;
                            match it.next() {
                                Some(d) => format!("{r} {d}"),
                                None => r,
                            }
                        }
                        None => e.to_string(),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            if changed {
                Some(newv)
            } else {
                None
            }
        } else {
            lookup(val)
        };
        match replacement {
            Some(rel) => {
                out.push_str(&rest[..vstart]);
                out.push_str(&rel);
                rest = &rest[vend..];
            }
            None => {
                out.push_str(&rest[..vend]);
                rest = &rest[vend..];
            }
        }
        let _ = vend_skip;
    }
    out.push_str(rest);
    // Absolute URLs outside attributes (plain text, meta content, etc.).
    for (url, rel) in mapping {
        out = out.replace(url, rel);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn short_url_redirects_to_file_without_reading_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let response = match path {
                    "/kppc" => "HTTP/1.1 302 Found\r\nLocation: /opaque\r\nContent-Length: 0\r\n\r\n",
                    "/opaque" => "HTTP/1.1 302 Found\r\nLocation: /payload.exe\r\nContent-Length: 0\r\n\r\n",
                    "/payload.exe" => "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 12\r\n\r\n",
                    "/attachment" => "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Disposition: Attachment; filename=report.html\r\nContent-Length: 12\r\n\r\n",
                    _ => panic!("unexpected request: {request}"),
                };
                // Deliberately omit the advertised body: metadata discovery must not read it.
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = reqwest::Client::new();
        let project = GrabberProject {
            start_url: format!("{base}/kppc"),
            max_pages: 10,
            ..Default::default()
        };
        let crawl = |project| {
            let client = client.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    Crawler {
                        project,
                        progress: None,
                    }
                    .crawl(&client),
                )
                .await
                .expect("crawl stalled reading a file body")
                .unwrap()
            }
        };
        let (pages, files) = crawl(project.clone()).await;
        assert!(pages.is_empty());
        assert_eq!(
            files,
            vec![FoundFile {
                url: format!("{base}/payload.exe"),
                size: Some(12),
                mime: "application/octet-stream".into(),
                page_url: String::new(),
                selected: true,
            }]
        );
        for filtered in [
            GrabberProject {
                extensions: vec!["pdf".into()],
                ..project.clone()
            },
            GrabberProject {
                exclude_regex: "payload".into(),
                ..project.clone()
            },
            GrabberProject {
                include_regex: "report".into(),
                ..project.clone()
            },
            GrabberProject {
                exclude_paths: vec!["payload".into()],
                ..project.clone()
            },
            GrabberProject {
                mime_allow: vec!["application/pdf".into()],
                ..project.clone()
            },
            GrabberProject {
                max_size: 10,
                ..project.clone()
            },
            GrabberProject {
                min_size: 20,
                ..project.clone()
            },
            GrabberProject {
                max_total_bytes: 10,
                ..project.clone()
            },
        ] {
            assert!(crawl(filtered).await.1.is_empty());
        }
        let (pages, files) = crawl(GrabberProject {
            start_url: format!("{base}/attachment"),
            ..project
        })
        .await;
        assert!(pages.is_empty());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url, format!("{base}/attachment"));
        server.abort();
    }

    #[test]
    fn sharepoint_detection_and_rest_shapes() {
        let u = Url::parse("https://stievano-my.sharepoint.com/personal/yatogam1_stievano_onmicrosoft_com/_layouts/15/onedrive.aspx?id=%2Fpersonal%2Fyatogam1%5Fstievano%5Fonmicrosoft%5Fcom%2FDocuments%2FAnimes%2FDragon%20Ball%20Super&ga=1").unwrap();
        assert!(is_sharepoint_page(&u));
        let (web, folder) = sharepoint_root(&u).unwrap();
        assert_eq!(
            web,
            "https://stievano-my.sharepoint.com/personal/yatogam1_stievano_onmicrosoft_com"
        );
        assert_eq!(
            folder,
            "/personal/yatogam1_stievano_onmicrosoft_com/Documents/Animes/Dragon Ball Super"
        );
        assert_eq!(
            sp_download_url(&web, "/personal/x/Documents/Dragon Ball/e 1.mkv"),
            "https://stievano-my.sharepoint.com/personal/yatogam1_stievano_onmicrosoft_com/_layouts/15/download.aspx?SourceUrl=/personal/x/Documents/Dragon%20Ball/e%201.mkv"
        );
        let (items, next) = sp_page(
            &serde_json::json!({"value":[{"Name":"a.mkv"}],"@odata.nextLink":"https://x/next"}),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(next.as_deref(), Some("https://x/next"));
        let (items, next) =
            sp_page(&serde_json::json!({"d":{"results":[{"Name":"b"}],"__next":"https://x/n2"}}));
        assert_eq!(items.len(), 1);
        assert_eq!(next.as_deref(), Some("https://x/n2"));
        assert!(!is_sharepoint_page(
            &Url::parse("https://example.com/_layouts/15/onedrive.aspx?id=/x").unwrap()
        ));
        // Widened: modern library view also routes to REST listing.
        assert!(is_sharepoint_page(&Url::parse("https://t.sharepoint.com/sites/x/Docs/Forms/AllItems.aspx?id=%2Fsites%2Fx%2FDocs%2Fa").unwrap()));
        // OData literal: apostrophe doubled, space/#/% encoded, cannot break URL.
        let b = sp_folder_base("https://t.sharepoint.com/p", "/p/Documents/A #1/it's 100%");
        assert_eq!(b, "https://t.sharepoint.com/p/_api/web/GetFolderByServerRelativeUrl('/p/Documents/A%20%231/it''s%20100%25')");
        // Newer item shape fallback.
        assert_eq!(
            sp_item_path(&serde_json::json!({"ServerRelativePath":{"DecodedUrl":"/p/Docs/a.mkv"}})),
            "/p/Docs/a.mkv"
        );
        assert_eq!(
            sp_item_path(
                &serde_json::json!({"ServerRelativeUrl":"/p/Docs/b.mkv","ServerRelativePath":{"DecodedUrl":"/ignored"}})
            ),
            "/p/Docs/b.mkv"
        );
    }

    #[test]
    fn document_base_href_resolution() {
        let page = Url::parse("https://ex.com/dir/p.html").unwrap();
        let html = r#"<head><base href="https://cdn.ex.com/static/"></head><img src="a.png"><a href="b.html">b</a>"#;
        let (pages, files) = links_with_doc_base(html, &page);
        assert!(files
            .iter()
            .any(|u| u.as_str() == "https://cdn.ex.com/static/a.png"));
        assert!(pages
            .iter()
            .any(|u| u.as_str() == "https://cdn.ex.com/static/b.html"));
        let out = rewrite_links(
            r#"<head><base href="/root/"></head><a href="z.html">z</a>"#,
            &page,
            &HashMap::new(),
            std::path::Path::new("/tmp"),
            "index.html",
        );
        assert!(!out.to_ascii_lowercase().contains("<base"), "{out}");
    }

    #[test]
    fn extract_and_scope() {
        let base = Url::parse("https://example.com/a/index.html").unwrap();
        let (pages, files) = extract_links(
            r#"<a href="/b/">b</a><img src="pic.png"><a href="https://other.com/x.zip">x</a>"#,
            &base,
        );
        assert!(pages.iter().any(|u| u.path() == "/b/"));
        assert!(files.iter().any(|u| u.path().ends_with("pic.png")));
        assert!(!in_scope(
            &base,
            &Url::parse("https://other.com/").unwrap(),
            0
        ));
        assert!(!in_scope(
            &base,
            &Url::parse("https://sub.example.com/").unwrap(),
            0
        ));
        assert!(in_scope(
            &base,
            &Url::parse("https://sub.example.com/").unwrap(),
            1
        ));
    }
    #[test]
    fn robots() {
        assert!(!robots_allows(
            "User-agent: *\nDisallow: /private/",
            "/private/x"
        ));
        assert!(robots_allows(
            "User-agent: *\nDisallow: /private/",
            "/public"
        ));
    }
    #[test]
    fn robots_allow_and_wildcards() {
        let txt = "User-agent: *\nDisallow: /private/\nAllow: /private/public.html\nDisallow: /*.pdf$\nDisallow: /tmp/*\n";
        assert!(robots_allows(txt, "/private/public.html")); // longest match wins
        assert!(!robots_allows(txt, "/private/other"));
        assert!(!robots_allows(txt, "/a/b.pdf")); // * + $ anchor
        assert!(robots_allows(txt, "/a/b.pdfx")); // $ prevents suffix match
        assert!(!robots_allows(txt, "/tmp/a/b"));
        assert!(robots_allows("User-agent: bot\nDisallow: /\n", "/anything")); // only * group applies
        assert_eq!(registrable_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("a.b.example.com"), "example.com");
        assert_eq!(registrable_domain("sub.shop.com.au"), "shop.com.au");
        let base = Url::parse("https://example.co.uk/").unwrap();
        assert!(in_scope(
            &base,
            &Url::parse("https://cdn.example.co.uk/").unwrap(),
            1
        ));
        assert!(!in_scope(
            &base,
            &Url::parse("https://other.co.uk/").unwrap(),
            1
        ));
        assert!(FoundFile::default().page_url.is_empty());
    }
    #[test]
    fn mirror_query_and_relative_rewrite() {
        let root = Url::parse("https://example.com/").unwrap();
        let plain = mirror_rel_path(
            &root,
            &Url::parse("https://example.com/a/page.html").unwrap(),
        );
        let q1 = mirror_rel_path(
            &root,
            &Url::parse("https://example.com/a/page.html?x=1").unwrap(),
        );
        let q2 = mirror_rel_path(
            &root,
            &Url::parse("https://example.com/a/page.html?x=2").unwrap(),
        );
        assert_eq!(plain, "example.com/a/page.html");
        assert_ne!(q1, plain);
        assert_ne!(q1, q2);
        assert!(
            q1.starts_with("example.com/a/page-") && q1.ends_with(".html"),
            "{q1}"
        );
        assert_eq!(
            q1,
            mirror_rel_path(
                &root,
                &Url::parse("https://example.com/a/page.html?x=1").unwrap()
            )
        ); // deterministic
        let page = Url::parse("https://example.com/a/c/index.html").unwrap();
        let mut map = HashMap::new();
        map.insert(
            "https://example.com/a/b/page.html".to_string(),
            "a/b/page.html".to_string(),
        );
        let out = rewrite_links(
            r#"<a href="../b/page.html">r</a><a href="/a/b/page.html">abs</a><a href="https://example.com/a/b/page.html">full</a>"#,
            &page,
            &map,
            std::path::Path::new("/tmp"),
            "a/c/index.html",
        );
        assert_eq!(out.matches("a/b/page.html").count(), 3, "{out}");
    }
    #[test]
    fn relative_link_depths() {
        assert_eq!(
            relative_link("example.com/index.html", "example.com/pic.png"),
            "pic.png"
        );
        assert_eq!(
            relative_link("example.com/a/index.html", "example.com/pic.png"),
            "../pic.png"
        );
        assert_eq!(
            relative_link("example.com/a/b/index.html", "example.com/a/c/p.html"),
            "../c/p.html"
        );
        assert_eq!(
            relative_link("example.com/a/index.html", "example.com/a/sub/p.html"),
            "sub/p.html"
        );
    }
    #[test]
    fn rewrite_resolves_against_document_base() {
        // <base href> shifts relative resolution; mapping keyed by resolved absolute URL.
        let page = Url::parse("https://ex.com/dir/p.html").unwrap();
        let mut map = HashMap::new();
        map.insert(
            "https://cdn.ex.com/static/a.png".to_string(),
            "../a.png".to_string(),
        );
        let out = rewrite_links(
            r#"<head><base href="https://cdn.ex.com/static/"></head><img src="a.png">"#,
            &page,
            &map,
            std::path::Path::new("/tmp"),
            "dir/p.html",
        );
        assert!(out.contains(r#"src="../a.png""#), "{out}");
        assert!(!out.to_ascii_lowercase().contains("<base"), "{out}");
    }
    #[test]
    fn rewrite_handles_case_spaces_srcset() {
        let page = Url::parse("https://ex.com/dir/p.html").unwrap();
        let mut map = HashMap::new();
        map.insert(
            "https://cdn.ex.com/static/a.png".to_string(),
            "../a.png".to_string(),
        );
        map.insert(
            "https://cdn.ex.com/static/b.png".to_string(),
            "../b.png".to_string(),
        );
        let out = rewrite_links(
            r#"<head><base href="https://cdn.ex.com/static/"></head><IMG SRC = "a.png"><img srcset='a.png 1x, b.png 2x'>"#,
            &page,
            &map,
            std::path::Path::new("/tmp"),
            "dir/p.html",
        );
        assert!(out.contains(r#"SRC = "../a.png""#), "{out}");
        assert!(out.contains("../a.png 1x, ../b.png 2x"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("<base"), "{out}");
    }
    #[test]
    fn normalize_and_new_link_tags() {
        assert_eq!(
            normalize_start_url("example.com/a").unwrap().as_str(),
            "https://example.com/a"
        );
        assert!(normalize_start_url("ftp://example.com/a").is_err());
        let base = Url::parse("https://example.com/a/index.html").unwrap();
        let (pages, files) = extract_links(
            r#"<iframe src="frame.html"></iframe><embed src="book.pdf"><img srcset="small.png 1x, big.png 2x">"#,
            &base,
        );
        assert!(pages.iter().any(|u| u.path().ends_with("frame.html")));
        assert!(files.iter().any(|u| u.path().ends_with("book.pdf")));
        assert!(files.iter().any(|u| u.path().ends_with("small.png")));
        assert!(files.iter().any(|u| u.path().ends_with("big.png")));
    }
    #[test]
    fn script_css_and_depth_rules() {
        let base = Url::parse("https://example.com/dir/index.html").unwrap();
        let (_p, files) = extract_links(
            r#"<style>.a{background:url(bg.png)}</style><script>let u="https://cdn.example.com/file.zip";</script>"#,
            &base,
        );
        assert!(files.iter().any(|u| u.path().ends_with("bg.png")));
        assert!(_p
            .iter()
            .any(|u| u.as_str() == "https://cdn.example.com/file.zip"));
        assert!(under_start_dir(
            &base,
            &Url::parse("https://example.com/dir/a/b.html").unwrap()
        ));
        assert!(!under_start_dir(
            &base,
            &Url::parse("https://example.com/other.html").unwrap()
        ));
    }
    #[test]
    fn canon_dedupe() {
        assert_eq!(
            canonicalize(&Url::parse("https://EXAMPLE.com/a#frag").unwrap()),
            canonicalize(&Url::parse("https://example.com/a").unwrap())
        );
    }
}
