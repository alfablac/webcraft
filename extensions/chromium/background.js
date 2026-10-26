// webcraft Chromium MV3 background. Native host: com.webcraft.host
try { importScripts("rank.js"); } catch (e) {}
const HOST = "com.webcraft.host";
// Native messaging: single in-flight queue. Host originally replied without
// an id, so sends are serialized and the first reply settles the in-flight
// send. New host echoes `id`; a mismatched id is ignored. 4s timeout rejects.
let nativePort = null;
let nativeNextId = 1;
let nativeInFlight = null;
const nativeQueue = [];
function nativeSend(msg) {
  return new Promise((resolve, reject) => {
    nativeQueue.push({ msg: { ...msg, id: nativeNextId++ }, resolve, reject, timer: null });
    pumpNative();
  });
}
function settleNative(item, err, resp) {
  if (nativeInFlight !== item) return;
  clearTimeout(item.timer);
  nativeInFlight = null;
  if (err) item.reject(err); else item.resolve(resp);
  pumpNative();
}
function pumpNative() {
  if (nativeInFlight || !nativeQueue.length) return;
  const item = nativeQueue.shift();
  nativeInFlight = item;
  item.timer = setTimeout(() => settleNative(item, new Error("native host timeout")), 4000);
  try {
    if (!nativePort) {
      nativePort = chrome.runtime.connectNative(HOST);
      nativePort.onMessage.addListener((m) => {
        const cur = nativeInFlight;
        if (!cur) return;
        if (m && m.id != null && m.id !== cur.msg.id) return;
        settleNative(cur, null, m);
      });
      nativePort.onDisconnect.addListener(() => {
        nativePort = null;
        const cur = nativeInFlight;
        nativeInFlight = null;
        if (cur) { clearTimeout(cur.timer); cur.reject(new Error("native host disconnected")); }
        while (nativeQueue.length) nativeQueue.shift().reject(new Error("native host disconnected"));
      });
    }
    nativePort.postMessage(item.msg);
  } catch (e) {
    // Fallback: one-shot sendNativeMessage
    try {
      chrome.runtime.sendNativeMessage(HOST, item.msg, (resp) => {
        if (chrome.runtime.lastError) settleNative(item, new Error(chrome.runtime.lastError.message));
        else settleNative(item, null, resp || { type: "ok" });
      });
    } catch (e2) { settleNative(item, e2); }
  }
}
// --- Media sniffing (observing webRequest only; no webRequestBlocking) ---
const CANDIDATE_TTL_MS = 30 * 60 * 1000;
const CANDIDATE_MAX = 200;
const candidates = new Map(); // url -> {url, type, tabId, frameId, redirectFrom, time, size}
// IDM model: every in-flight request keyed by requestId. Transient (not
// persisted); on completion/error record dies, candidate (URL index) stays.
// storage.session persists derived candidates only, so SW restart loses
// requestId correlation but keeps toolbar-scan list.
const requestRecords = new Map(); // requestId -> {url, redirectFrom, hops, tabId, frameId, time, type, size, status}
const RECORD_TTL_MS = 10 * 60 * 1000;
const RECORD_MAX = 500;
const MEDIA_PATH = /\.(mpd|m3u8|mp4|webm|ts|m4s)($|[?#])/i;
const MEDIA_TYPES = new Set(["media", "xmlhttprequest", "other", "object"]);
function isMediaIsh(details) {
  // Record only media-ish requests; full <all_urls> recording bloats TTL map
  // with page/API traffic element matcher never needs. Known registry-CDN
  // hosts record too (MSE fetch arrives as other/fetch off MEDIA_TYPES).
  if (isKnownCdnHost(details.url)) return true;
  if (details.type && MEDIA_TYPES.has(details.type)) return true;
  try { return MEDIA_PATH.test(new URL(details.url || "").pathname); } catch (e) { return MEDIA_PATH.test(String(details.url || "")); }
}
function sweepRecords(now = Date.now()) {
  for (const [k, r] of requestRecords) if (r.time && now - r.time > RECORD_TTL_MS) requestRecords.delete(k);
  while (requestRecords.size > RECORD_MAX) requestRecords.delete(requestRecords.keys().next().value); // ponytail: FIFO cap
}
// Browser request headers per URL: DENYLIST capture (most downloads are
// gated on header values — UA, Referer, Origin, Authorization, custom
// X- tokens...). Stack-owned / hop-by-hop / conditional / content names are
// never forwarded: the engine's send-time denylist is authoritative, and
// host flattening re-filters; this list only saves wire traffic. Raw Cookie
// never forwarded — cookies.getAll covers it and the engine Cookie field
// stays canonical. Captured on onBeforeSendHeaders with "extraHeaders"
// (Chromium-only extraInfoSpec): without it, Accept-Language/Referer/
// Cookie/Origin are hidden. Values over 4096 are DROPPED (never sliced —
// a truncated token is worse than none); duplicates join with ", ".
// Same TTL discipline as requestRecords; persisted to storage.session
// (wcUrlHeaders) so MV3 worker restarts keep the capture.
const HEADER_DENY = new Set(["cookie", "cookie2", "host", "content-length", "connection", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-connection", "accept-encoding", "range", "if-range", "if-modified-since", "if-none-match", "if-unmodified-since", "if-match", "cache-control", "pragma", "content-type", "priority", "set-cookie", "www-authenticate"]);
const HEADER_MAX_PER_URL = 24;
const urlHeaders = new Map(); // url -> {headers, time}
function headerKeyIndex(url) {
  // Dual key: exact URL plus origin+path, so a redirect-final or
  // query-drifted send still finds the browser's headers.
  try { const u = new URL(url); return u.origin + u.pathname; } catch (e) { return url; }
}
function persistUrlHeaders() {
  try {
    const o = {};
    let n = 0;
    for (const [u, e] of urlHeaders) {
      if (n >= 200) break;
      // Authorization is never persisted: one-shot tokens must not
      // outlive the worker in storage.
      const h = {};
      for (const [k, v] of Object.entries(e.headers)) if (k !== "authorization") h[k] = v;
      if (Object.keys(h).length) { o[u] = { headers: h, time: e.time }; n++; }
    }
    chrome.storage.session.set({ wcUrlHeaders: o });
  } catch (e) {}
}
async function loadUrlHeaders() {
  try {
    const s = await chrome.storage.session.get(["wcUrlHeaders"]);
    if (s && s.wcUrlHeaders) for (const [u, e] of Object.entries(s.wcUrlHeaders)) if (e && e.headers) urlHeaders.set(u, e);
  } catch (e) {}
}
function captureReqHeaders(details) {
  try {
    if (!details || !details.url || !Array.isArray(details.requestHeaders)) return;
    const out = {};
    for (const h of details.requestHeaders) {
      const n = String(h.name || "").toLowerCase();
      if (!n || n.length > 64 || /[\r\n=:]/.test(n) || HEADER_DENY.has(n)) continue;
      const v = String(h.value || "");
      if (!v || v.length > 4096 || /[\r\n]/.test(v)) continue;
      out[n] = out[n] ? out[n] + ", " + v : v;
      if (Object.keys(out).length > HEADER_MAX_PER_URL) delete out[n];
    }
    if (!Object.keys(out).length) return;
    const now = Date.now();
    for (const [u, e] of urlHeaders) if (now - e.time > RECORD_TTL_MS) urlHeaders.delete(u);
    while (urlHeaders.size >= RECORD_MAX) urlHeaders.delete(urlHeaders.keys().next().value); // ponytail: FIFO cap
    urlHeaders.set(details.url, { headers: out, time: now });
    const idx = headerKeyIndex(details.url);
    if (idx !== details.url) urlHeaders.set(idx, { headers: out, time: now });
    persistUrlHeaders();
  } catch (e) {}
}
function recordRequest(details) {
  try {
    captureReqHeaders(details); // before media filter: downloads are not media-ish
    if (!details || !details.requestId || !(details.tabId >= 0)) return;
    if (!isMediaIsh(details)) return;
    sweepRecords();
    const prev = requestRecords.get(details.requestId);
    if (prev) {
      // Redirect: same requestId re-fires with new url. hops keeps every hop
      // (origin first, cap 8); redirectFrom stays first origin for back-compat.
      if (details.url && details.url !== prev.url) {
        if (!prev.redirectFrom) prev.redirectFrom = prev.url;
        if (!Array.isArray(prev.hops)) prev.hops = prev.redirectFrom ? [prev.redirectFrom, prev.url] : [prev.url];
        if (!prev.hops.includes(details.url) && prev.hops.length < 8) prev.hops.push(details.url);
        prev.url = details.url;
      }
    } else {
      requestRecords.set(details.requestId, { url: details.url || "", redirectFrom: "", hops: details.url ? [details.url] : [], tabId: details.tabId, frameId: details.frameId != null ? details.frameId : null, time: details.timeStamp || Date.now(), type: "", size: 0, status: 0 });
    }
  } catch (e) {}
}
// MV3 service worker dies idle: persist candidates in storage.session
// (per tab) or played-then-clicked-later finds empty list.
function persistCandidates() {
  try {
    const o = {};
    for (const [u, c] of candidates) o[u] = c;
    chrome.storage.session.set({ wcCandidates: o });
  } catch (e) {}
}
let candidatesLoaded = false;
async function loadCandidates() {
  if (candidatesLoaded) return;
  candidatesLoaded = true;
  try {
    const s = await chrome.storage.session.get(["wcCandidates"]);
    // ponytail: entries lacking numeric status predate status tracking (incl. pre-206 filter); drop, TTL re-sniff rediscovers live ones.
    if (s && s.wcCandidates) for (const [u, c] of Object.entries(s.wcCandidates)) if (c && typeof c.status === "number") candidates.set(u, c);
    pruneCandidates();
  } catch (e) {}
}
const SNIFF_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl", "application/x-mpegurl", "video/vnd.mpeg.dash.mpd", "video/mpegurl", "video/x-mpegurl", "application/mpegurl"]);
const META_CDN_RE = /(^|\.)(cdninstagram\.com|fbcdn\.net)$/i;
const META_PAGE_RE = /(^|\.)(instagram\.com|facebook\.com|threads\.net|threads\.com)$/i;
function isMetaCdnUrl(u) { try { return META_CDN_RE.test(new URL(u).hostname); } catch (e) { return /cdninstagram\.com|fbcdn\.net/i.test(String(u || "")); } }
function isMetaDoc(details) { try { const d = details && (details.documentUrl || details.initiator || details.originUrl || ""); if (!d) return false; return META_PAGE_RE.test(new URL(d).hostname); } catch (e) { return false; } }
// Known registry-CDN hostname (hostMatchesScope same-host pairs): media CDN
// pair of a listed site. isMediaIsh keeps these recorded even when the
// request type falls outside MEDIA_TYPES (MSE fetch arrives as other/fetch).
function isKnownCdnHost(u) {
  try {
    const h = new URL(u || "").hostname.toLowerCase().replace(/^(www\.|m\.)/, "");
    return typeof hostMatchesScope === "function" && ["instagram.com", "facebook.com", "youtube.com", "x.com", "reddit.com", "tiktok.com", "vimeo.com", "twitch.tv", "dailymotion.com", "bilibili.com", "globo.com"].some((ph) => ph !== h && hostMatchesScope(ph, h));
  } catch (e) { return false; }
}
function isSniff(details, type) {
  const url = details.url || "";
  let path = "";
  try { path = new URL(url).pathname.toLowerCase(); } catch (e) { path = String(url).split(/[?#]/)[0].toLowerCase(); }
  // pathname only: query-string ext false-rejects signed Meta URLs
  // (?x=.js / ?token=xx.css). Page assets rejected by real path suffix.
  if (/\.(json|js|mjs|css|html?|map)$/i.test(path)) return false; // page assets
  const ft = String(details.type || "");
  const igCdn = isMetaCdnUrl(url);
  const igDoc = isMetaDoc(details);
  const knownCdn = isKnownCdnHost(url);
  // Meta progressive (extension-less, video/mp4, type=fetch/xhr/other) and
  // Meta DASH (extension-less .mpd via fetch, application/dash+xml or
  // octet-stream): accept Meta CDN regardless of request type. Request-type
  // allowlist was root cause: MSE fetch arrives as xmlhttprequest/fetch, not media.
  if (igCdn && (type.startsWith("video/") || type.startsWith("audio/") || SNIFF_TYPES.has(type))) return true;
  if (igCdn && (type === "application/octet-stream" || !type) && (igDoc || ft === "media" || ft === "xmlhttprequest" || ft === "fetch" || ft === "other" || ft === "object" || ft === "")) return true;
  if (igCdn && igDoc && (path.endsWith(".mpd") || path.endsWith(".m3u8"))) return true;
  // Known registry-CDN hosts (googlevideo/twimg/...): same sniff allowances
  // as Meta for sniff types, and octet-stream media (no .mpd branch: that
  // needs a listed page doc; other CDN manifests still land via the .mpd /
  // SNIFF_TYPES branches below).
  if (knownCdn && SNIFF_TYPES.has(type)) return true;
  if (knownCdn && (type === "application/octet-stream" || !type) && (ft === "media" || ft === "xmlhttprequest" || ft === "fetch" || ft === "other" || ft === "object" || ft === "")) return true;
  if (path.endsWith(".mpd") || path.endsWith(".m3u8")) return true;
  // Extension fallback: DASH/HLS/full mp4 served without usable Content-Type.
  if (!type || type === "application/octet-stream") {
    if (path.endsWith(".mp4")) return true;
    return type === "application/octet-stream" && details.type === "media";
  }
  if (SNIFF_TYPES.has(type)) return true;
  if (type.startsWith("video/") || type.startsWith("audio/")) {
    // Segments pollute panel (X twimg / Reddit DASH floods cap); only
    // manifests/full media belong. Meta CDN exempted by branches above.
    try { const seg = new URL(url).pathname.split("/").pop() || ""; if (/^(seg|segment|chunk|frag)[-_]?\d|\.m4s$|[-_]\d+\.(ts|m4s)$/i.test(seg)) return false; } catch (e) {}
    return true;
  }
  return false;
}
function isPlaylistUrl(u) { try { const p = new URL(u).pathname.toLowerCase(); return p.endsWith(".mpd") || p.endsWith(".m3u8"); } catch (e) { return /\.(mpd|m3u8)($|[?#])/i.test(String(u || "")); } }
function pruneCandidates(now = Date.now()) {
  let changed = false;
  for (const [url, c] of candidates) if (now - c.time > (isPlaylistUrl(url) ? 30 * 60 * 1000 : CANDIDATE_TTL_MS)) { candidates.delete(url); changed = true; }
  // Cap: evict non-playlists (oldest first) before playlists; playlists alone over cap fall back to FIFO.
  if (candidates.size > CANDIDATE_MAX) { for (const url of [...candidates.keys()]) { if (candidates.size <= CANDIDATE_MAX) break; if (!isPlaylistUrl(url)) { candidates.delete(url); changed = true; } } }
  while (candidates.size > CANDIDATE_MAX) { candidates.delete(candidates.keys().next().value); changed = true; }
  if (changed) persistCandidates();
}
if (chrome.webRequest) {
  try {
    const F = { urls: ["<all_urls>"] };
    const kill = (d) => { try { requestRecords.delete(d.requestId); } catch (e) {} };
    if (chrome.webRequest.onBeforeRequest) chrome.webRequest.onBeforeRequest.addListener(recordRequest, F);
    if (chrome.webRequest.onBeforeSendHeaders) chrome.webRequest.onBeforeSendHeaders.addListener(recordRequest, F, ["requestHeaders", "extraHeaders"]);
    loadUrlHeaders();
    if (chrome.webRequest.onCompleted) chrome.webRequest.onCompleted.addListener(kill, F);
    if (chrome.webRequest.onErrorOccurred) chrome.webRequest.onErrorOccurred.addListener(kill, F);
  } catch (e) {}
}
function headerOf(headers, name) { const h = (headers || []).find((x) => x.name.toLowerCase() === name); return h ? h.value || "" : ""; }
function totalSizeFromHeaders(headers, statusCode) {
  // 206 Content-Length is range length, not total; Content-Range total wins.
  const h = headers || [];
  if (statusCode === 206) {
    const cr = h.find((x) => x.name.toLowerCase() === "content-range");
    const m = cr && /\/(\d+)\s*$/.exec(cr.value || "");
    if (m) { const n = parseInt(m[1], 10); if (n > 0) return n; } // total '*' falls through
  }
  const lh = h.find((x) => x.name.toLowerCase() === "content-length");
  return lh ? parseInt(lh.value, 10) || 0 : 0;
}
function filenameFromDisp(disp, url) {
  const m = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(disp || "");
  if (m) { try { return decodeURIComponent(m[1]); } catch (e) { return m[1]; } }
  return filenameFromUrl(url);
}
// Download probes from response headers (onCreated has no headers and
// item.totalBytes is often 0 at creation). Persisted like candidates:
// SW death between headers and onCreated loses probe -> size 0 verdict.
const downloadProbes = new Map(); // url -> {size, mime, filename, referrer, time}
const PROBE_TTL_MS = 5 * 60 * 1000;
const PROBE_MAX = 100;
function persistProbes() {
  try {
    const o = {};
    for (const [u, p] of downloadProbes) o[u] = p;
    chrome.storage.session.set({ wcProbes: o });
  } catch (e) {}
}
function pruneProbes(now = Date.now()) {
  let changed = false;
  for (const [url, p] of downloadProbes) if (now - p.time > PROBE_TTL_MS) { downloadProbes.delete(url); changed = true; }
  while (downloadProbes.size > PROBE_MAX) { downloadProbes.delete(downloadProbes.keys().next().value); changed = true; }
  if (changed) persistProbes();
}
let probesLoaded = false;
async function loadProbes() {
  if (probesLoaded) return;
  probesLoaded = true;
  try {
    const s = await chrome.storage.session.get(["wcProbes"]);
    if (s && s.wcProbes) for (const [u, p] of Object.entries(s.wcProbes)) if (p) downloadProbes.set(u, p);
    pruneProbes();
  } catch (e) {}
}
if (chrome.webRequest && chrome.webRequest.onHeadersReceived) {
  chrome.webRequest.onHeadersReceived.addListener(async (details) => {
    recordRequest(details);
    const rec2 = requestRecords.get(details.requestId);
    if (rec2) { rec2.status = details.statusCode || 0; rec2.size = totalSizeFromHeaders(details.responseHeaders, details.statusCode) || rec2.size || 0; const ch = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-type"); if (ch && ch.value) rec2.type = ch.value.split(";")[0].trim().toLowerCase(); }
    // Probe before isSniff early return: any 200/206 response may become a download.
    try {
      if (details && [200, 206].includes(details.statusCode)) {
        const headers = details.responseHeaders || [];
        downloadProbes.set(details.url, {
          size: totalSizeFromHeaders(headers, details.statusCode),
          mime: headerOf(headers, "content-type").split(";")[0].trim().toLowerCase(),
          filename: filenameFromDisp(headerOf(headers, "content-disposition"), details.url),
          referrer: details.originUrl || details.documentUrl || "",
          time: Date.now(),
        });
        pruneProbes();
        persistProbes();
      }
    } catch (e) {}
    await loadCandidates();
    if (details.statusCode && ![200, 206].includes(details.statusCode)) return; // 404/redirect bodies poison panel
    if (!(details.tabId >= 0)) return; // non-tab leaks into every tab list
    const header = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-type");
    const type = header && header.value ? header.value.split(";")[0].trim().toLowerCase() : "";
    if (!isSniff(details, type)) return;
    const rec = requestRecords.get(details.requestId);
    candidates.set(details.url, { url: details.url, type, status: details.statusCode || 0, tabId: details.tabId, frameId: details.frameId != null ? details.frameId : null, redirectFrom: (rec && rec.redirectFrom) || "", hops: (rec && rec.hops) || [], time: (rec && rec.time) || Date.now(), size: totalSizeFromHeaders(details.responseHeaders, details.statusCode) });
    persistCandidates();
    pruneCandidates();
  }, { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "media", "object", "xmlhttprequest", "other"] }, ["responseHeaders"]);
}
function basename(name) {
  return name ? String(name).split(/[\\/]/).pop() || "" : "";
}
async function sendDownload(url, tab, extra = {}) {
  let cookies = "";
  let cookiesJson = "";
  try {
    // Cookies match page host (X/YouTube/Reddit session), not CDN host.
    // Page intent: pageUrl promotes to yt-dlp target app-side. Stream intent:
    // cookieUrl scopes cookies only, no promotion field sent.
    // storeId from the tab so Firefox-container / Chrome-incognito
    // sessions resolve their own cookie store, not the default one.
    const cookieQuery = { url: extra.pageUrl || extra.cookieUrl || url };
    if (tab && tab.cookieStoreId) cookieQuery.storeId = tab.cookieStoreId;
    const cs = await chrome.cookies.getAll(cookieQuery);
    cookies = cs.map(c => `${c.name}=${c.value}`).join("; ");
    // Structured form for yt-dlp cookie jar (httpOnly/domain/expiry lost in string form).
    cookiesJson = JSON.stringify(cs.map(c => ({ name: c.name, value: c.value, domain: c.domain, path: c.path, expirationDate: c.expirationDate, secure: c.secure, httpOnly: c.httpOnly, hostOnly: c.hostOnly })));
  } catch (e) {}
  // Referrer = video page (permalink), not feed tab URL (x.com/home).
  const referrer = extra.referrer != null ? extra.referrer : (extra.pageUrl || extra.cookieUrl || (tab ? tab.url : ""));
  const msg = { type: "download", url, referrer, referer: referrer, cookies, cookiesJson, filename: extra.filename ? basename(extra.filename) : filenameFromUrl(url) };
  // Last-seen browser headers for this URL (exact or origin+path index);
  // UA + Accept-Language synthesis so the engine never has to fall back
  // to the webcraft UA on never-requested URLs.
  try {
    const got = urlHeaders.get(url) || urlHeaders.get(headerKeyIndex(url));
    const headers = { ...(got && got.headers ? got.headers : {}) };
    if (!headers["user-agent"] && typeof navigator !== "undefined" && navigator.userAgent) headers["user-agent"] = navigator.userAgent;
    if (!headers["accept-language"] && typeof navigator !== "undefined" && Array.isArray(navigator.languages) && navigator.languages.length) headers["accept-language"] = navigator.languages.join(",");
    if (Object.keys(headers).length) msg.headers = headers;
  } catch (e) {}
  if (extra.pageUrl) msg.pageUrl = extra.pageUrl;
  if (extra.pageTitle) msg.pageTitle = extra.pageTitle;
  if (extra.intent) msg.intent = extra.intent;
  if (extra.mime) msg.mime = extra.mime;
  // Captured (intercepted) downloads pass host-side capture filter (extension
  // allowlist + min size from capture-filter.json). Explicit sends (context
  // menu, video panel, link grab) omit `captured` and always go: user intent.
  if (extra.captured) msg.captured = true;
  if (extra.size > 0) msg.size = extra.size;
  return nativeSend(msg);
}
function filenameFromUrl(u) {
  try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; }
}
const MEDIA_EXT = /\.(mp4|webm|mkv|mp3|mp4|zip|rar|7z|gz|pdf|exe|msi|dmg|deb|iso|tar|mpd|m3u8)(\?|#|$)/i;
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: "downloadcraft-link", title: "Download with webcraft", contexts: ["link"] });
  chrome.contextMenus.create({ id: "downloadcraft-media", title: "Download media with webcraft", contexts: ["video", "audio", "image"] });
  chrome.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with webcraft", contexts: ["page"] });
  chrome.contextMenus.create({ id: "downloadcraft-grab", title: "Download site with webcraft", contexts: ["page"] });
  chrome.contextMenus.create({ id: "downloadcraft-scrap", title: "Scrap site with webcraft", contexts: ["page"] });
});
chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) sendDownload(info.linkUrl, tab);
  // blob:/data: srcUrl dies in native host; open panel (page yt-dlp row works).
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl && /^(blob|data):/.test(info.srcUrl)) { if (tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }); }
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) sendDownload(info.srcUrl, tab);
  // Stale tab (no content script): fall back to grab-site instead of silent dead click.
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" }).catch(() => { if (tab.url) sendDownload("webcraft://grab?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url, cookieUrl: tab.url }); });
  // "Download site" -> Add URL/File dialog prefilled with the page URL
  // (webcraft://adddial, NOT add?url= which is the IDM capture-confirm modal).
  // cookieUrl=tab.url: cookies must scope to real page, not webcraft:// pseudo-URL.
  else if (info.menuItemId === "downloadcraft-grab" && tab && tab.url) sendDownload("webcraft://adddial?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url, cookieUrl: tab.url });
  // "Scrap site" -> Site Grabber (auto-explore, cookies ride along).
  else if (info.menuItemId === "downloadcraft-scrap" && tab && tab.url) sendDownload("webcraft://grab?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url, cookieUrl: tab.url });
});
// Captured downloads go DIRECTLY to webcraft (its modal is the config box,
// like IDM). No browser ask box. Filter = webcraft Settings, decided by
// native host (captureCheck) BEFORE cancel: filtered -> browser keeps file.
const bypass = new Map(); // url -> timestamp, one-shot for resumed native downloads
const BYPASS_TTL_MS = 30 * 1000;
function takeBypass(url) {
  const t = bypass.get(url);
  if (t == null) return false;
  bypass.delete(url);
  return Date.now() - t < BYPASS_TTL_MS;
}
function resumeBrowser(url, filename) {
  bypass.set(url, Date.now());
  try { const p = chrome.downloads.download({ url, filename: filename || undefined }); if (p && p.catch) p.catch(() => {}); } catch (e) {}
}
// Never intercept page-asset downloads (json/js/css) even with download attr.
function isAssetDownload(item) {
  const m = String((item && item.mime) || "").toLowerCase();
  if (/json|javascript|ecmascript|css/.test(m)) return true;
  const f = String((item && item.filename) || (item && item.url) || "").split(/[?#]/)[0];
  return /\.(json|js|mjs|css)$/i.test(f);
}
async function captureCheck(entry) {
  try {
    const r = await nativeSend({ type: "captureCheck", url: entry.url, filename: entry.filename, mime: entry.mime, size: entry.size });
    return !!(r && r.allow === true); // fail-closed: garbage/down -> browser keeps file
  } catch (e) { return false; }
}
// Cancel-first capture (IDM Chrome pattern): onCreated cancels sync, before
// any await/native call, so no body progress races verdict. probe (headers
// map, persisted in chrome.storage.session) supplies size/mime/filename;
// captureCheck verdict: valid -> send webcraft; invalid/host-down ->
// resumeBrowser() restarts in browser (Firefox semantics in spirit, MV3
// limits accepted: bytes leaked until sync cancel lands; if file already
// complete before cancel, browser won, no send).
chrome.downloads.onCreated.addListener((item) => {
  try {
    if (isAssetDownload(item)) return;
    if (!item || item.id == null) return;
    const url0 = item.finalUrl || item.url || "";
    if (!url0 || url0.startsWith("blob:") || url0.startsWith("data:")) return;
    if (takeBypass(url0) || (item.url && takeBypass(item.url))) return;
  } catch (e) { return; }
  // CANCEL FIRST: sync, after sync guards only, before any await/native call.
  try { const p = chrome.downloads.cancel(item.id); if (p && p.catch) p.catch(() => {}); } catch (e) {} // completed item rejects: fire-and-forget
  const url = item.finalUrl || item.url || "";
  const id = item.id;
  (async () => {
    await loadProbes();
    const probe = downloadProbes.get(url) || downloadProbes.get(item.url);
    const size = probe && probe.size > 0 ? probe.size : (item.totalBytes > 0 ? item.totalBytes : 0);
    const entry = {
      url,
      filename: basename(item.filename) || (probe && probe.filename) || filenameFromUrl(url),
      mime: item.mime || (probe && probe.mime) || "",
      size,
      referrer: item.referrer || (probe && probe.referrer) || "",
    };
    try {
      const items = await chrome.downloads.search({ id }).catch(() => null);
      if (!items || !items.length) return; // deleted between cancel and check: nothing to resume
      if (items[0].state === "complete") return; // browser won: already complete before cancel landed. interrupted = our cancel (proceed)
    } catch (e) {}
    try {
      if (!(await captureCheck(entry))) { resumeBrowser(entry.url, entry.filename); return; } // invalid/host-down: restart in browser
      bypass.set(entry.url, Date.now()); // BEFORE send: re-entrant onCreated hits bypass
      const resp = await sendDownload(entry.url, null, { referrer: entry.referrer, filename: entry.filename, mime: entry.mime, size: entry.size, captured: true });
      if (!resp || resp.skipped || resp.type === "error") resumeBrowser(entry.url, entry.filename); // skipped/error: item already cancelled, re-download
    } catch (e) { resumeBrowser(entry.url, entry.filename); } // host died mid-send: file must not vanish
    pollResumeOnce().catch(() => {}); // poll on onCreated too (guarded skip-when-busy inside)
  })().catch(() => {});
});

// Resume poll: Ask box "resume in browser" (and host restart) queues URL in
// native host; extension polls and restarts it in the browser. Old host reply
// (unknown-type/error) has no items -> no-op. Comment: poll skip-when-busy
// mandatory (queue flood) - nativeSend would enqueue behind captures/sends
// forever, so if capture in-flight/queued, skip this tick entirely.
async function pollResumeOnce() {
  if (nativeInFlight || nativeQueue.length) return; // skip-when-busy mandatory (queue flood)
  try {
    const r = await nativeSend({ type: "pollResume" });
    const items = r && Array.isArray(r.items) ? r.items : [];
    for (const it of items) {
      if (!it || !it.url || !/^https?:/i.test(it.url)) continue;
      if (!(Date.now() - (it.ts || 0) < 10 * 60 * 1000)) continue; // fresh only (<10min)
      resumeBrowser(it.url, it.filename);
    }
  } catch (e) {} // host down/old host: silently no-op
}
try {
  chrome.alarms.create("wcPollResume", { periodInMinutes: 0.5 }); // 30s MV3 floor; setInterval unreliable in SW
  if (chrome.alarms.onAlarm) chrome.alarms.onAlarm.addListener((a) => { if (a && a.name === "wcPollResume") { pollResumeOnce().catch(() => {}); refreshBtnStyle().catch(() => {}); } });
} catch (e) {}
try { pollResumeOnce().catch(() => {}); } catch (e) {} // poll on startup; onCreated handles also wake host

// Button appearance: host owns style {icon,w,h,radius}; old/down host replies
// invalid/missing -> keep last good cache (storage.session survives SW death).
let wcBtnStyle = null;
let wcBtnStyleLoaded = false;
let wcBtnStyleLastTry = 0;
function validBtnStyle(r) {
  if (!r || typeof r !== "object") return null;
  if (typeof r.icon !== "string" || !r.icon) return null;
  if (typeof r.w !== "number" || !Number.isFinite(r.w) || !(r.w > 0)) return null;
  if (typeof r.h !== "number" || !Number.isFinite(r.h) || !(r.h > 0)) return null;
  if (typeof r.radius !== "string") return null;
  return { icon: r.icon, w: r.w, h: r.h, radius: r.radius };
}
async function loadBtnStyle() {
  if (wcBtnStyleLoaded) return;
  wcBtnStyleLoaded = true;
  try {
    const s = await chrome.storage.session.get(["wcBtnStyle"]);
    const v = s && validBtnStyle(s.wcBtnStyle);
    if (v) wcBtnStyle = v;
  } catch (e) {}
}
function broadcastBtnStyle() {
  try {
    chrome.tabs.query({}, (tabs) => {
      for (const t of tabs || []) {
        if (!t || t.id == null) continue;
        try { const p = chrome.tabs.sendMessage(t.id, { type: "downloadcraft-button-style", style: wcBtnStyle }); if (p && p.catch) p.catch(() => {}); } catch (e) {}
      }
    });
  } catch (e) {}
}
async function refreshBtnStyle(force) {
  if (!force && Date.now() - wcBtnStyleLastTry < 25000) return wcBtnStyle; // ~30s poll cadence
  wcBtnStyleLastTry = Date.now();
  await loadBtnStyle();
  let v = null;
  try { v = validBtnStyle(await nativeSend({ type: "getButtonStyle" })); } catch (e) {}
  if (!v) return wcBtnStyle; // host down/old: keep cache, no broadcast
  const changed = JSON.stringify(v) !== JSON.stringify(wcBtnStyle);
  wcBtnStyle = v;
  try { chrome.storage.session.set({ wcBtnStyle: v }); } catch (e) {}
  if (changed) broadcastBtnStyle();
  return wcBtnStyle;
}
refreshBtnStyle(true).catch(() => {}); // startup refresh (restores session cache first)

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  // Legacy stale content scripts may still answer removed ask box: ignore.
  if (msg && msg.type === "downloadcraft-confirm-result") return;
  if (msg && msg.type === "downloadcraft-get-button-style") {
    if (wcBtnStyle) sendResponse({ style: wcBtnStyle });
    else refreshBtnStyle(true).then((s) => sendResponse({ style: s }), () => sendResponse({ style: wcBtnStyle }));
    return true; // async sendResponse
  }
  if (msg && msg.type === "getCandidates") {
    loadCandidates().then(() => {
      pruneCandidates();
      sweepRecords();
      const tabId = sender.tab && sender.tab.id;
      let pool;
      if (msg.element) {
        // Element-scoped (IDM): same tab+frame only. Prefer live records;
        // candidates carry same fields (frameId/redirectFrom/hops) and survive
        // SW/record cleanup, so match over union deduped by url.
        const byUrl = new Map();
        [...requestRecords.values(), ...candidates.values()].forEach((r) => { if (r && r.url) byUrl.set(r.url, r); });
        const senderFrame = sender.frameId != null ? sender.frameId : null;
        const el = { tabId: sender.tab ? sender.tab.id : tabId, frameId: senderFrame, srcs: msg.element.srcs || [], srcKey: msg.element.srcKey || "", postId: msg.element.postId || "", streamId: msg.element.streamId || "", since: msg.element.since || 0, now: Date.now() };
        pool = typeof matchRecordsForElement === "function" ? matchRecordsForElement([...byUrl.values()], el, Date.now()) : [];
        // Element-frame match empty: retry frame-agnostic (frameId=null passes
        // tolerant filter) before returning empty. noTiming: retry widens
        // frame only, never tier — else cross-frame timing hands this video
        // another video's streams when srcs/postId/streamId matched nothing.
        if (!pool.length && senderFrame != null && typeof matchRecordsForElement === "function") pool = matchRecordsForElement([...byUrl.values()], { ...el, frameId: null, noTiming: true }, Date.now());
          // Host-scoped fallback (background-only): CDN URLs never carry the
          // page id on Meta/YouTube/X/Reddit/TikTok/..., MSE src is blob:,
          // timing misses preloads. pageHost = sender.tab.url hostname
          // (trusted bg, not msg); hostMatchesScope maps it to its CDN pair.
          // noTiming likewise: hostExtra is window-bound itself; plain timing
          // must not widen into other frames on feeds.
          if (!pool.length && typeof matchRecordsForElement === "function") {
            let ph = "";
            try { ph = new URL((sender.tab && sender.tab.url) || "").hostname.toLowerCase(); } catch (e) {}
            if (ph) pool = matchRecordsForElement([...byUrl.values()], { ...el, frameId: null, noTiming: true, pageHost: ph }, Date.now(), [...candidates.values()]);
          }
      } else {
        pool = [...candidates.values()].filter((c) => tabId == null || c.tabId === tabId);
      }
      const list = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)(pool, msg.element ? el : null);
      sendResponse({ type: "candidates", candidates: list });
    });
    return true;
  }
  if (msg && msg.type === "downloadcraft-download") { sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer, pageTitle: msg.pageTitle }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
  if (msg && msg.type === "downloadcraft-grab-site" && sender.tab && sender.tab.url) { sendDownload("webcraft://grab?url=" + encodeURIComponent(sender.tab.url), sender.tab, { referrer: sender.tab.url, cookieUrl: sender.tab.url }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
  // intent=stream: picked manifest/file is target; pageUrl must NOT promote
  // to yt-dlp page (else manifest choice lost). Only cookies scope passes.
  if (msg && msg.type === "downloadcraft-video-panel") { sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer || msg.pageUrl, pageTitle: msg.pageTitle, pageUrl: msg.intent === "page" ? msg.pageUrl : undefined, cookieUrl: msg.pageUrl, intent: msg.intent }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
});
// Toolbar click -> content scan (unchanged); host-scoped fallback lives in getCandidates above.
chrome.action.onClicked.addListener((tab) => { if (tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }).catch(() => {}); });
// SPA pushState nav (url-only change) must NOT wipe candidates: IG preloads
// media before user opens reel; only real loading nav to a new URL purges.
const tabLastUrl = new Map();
if (chrome.tabs && chrome.tabs.onUpdated) chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status !== "loading" || !info.url) return;
  if (tabLastUrl.get(tabId) === info.url) return;
  tabLastUrl.set(tabId, info.url);
  let n = false;
  for (const [u, c] of [...candidates]) if (c.tabId === tabId) { candidates.delete(u); n = true; }
  for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k);
  if (n) persistCandidates();
});
if (chrome.tabs && chrome.tabs.onRemoved) chrome.tabs.onRemoved.addListener((tabId) => { for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k); for (const [u, c] of [...candidates]) if (c.tabId === tabId) candidates.delete(u); persistCandidates(); });
// ponytail: urlHeaders intentionally not persisted/tab-cleaned — 10-min TTL
// bounds it; SW restart only loses headers, UA fallback covers the gap.
