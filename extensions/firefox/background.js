// webcraft Firefox background. Native host: com.webcraft.host
// IDM-parity capture (from shipped IDM Firefox ext evidence): blocking
// webRequest.onHeadersReceived decides takeover by status, Content-Disposition,
// Content-Type (MIME->ext map), URL extension; media requests feed video panel.
const HOST = "com.webcraft.host";
// Native messaging: single in-flight queue (see Chromium background.js).
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
      nativePort = browser.runtime.connectNative(HOST);
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
    browser.runtime.sendNativeMessage(HOST, item.msg).then(
      (resp) => settleNative(item, null, resp || { type: "ok" }),
      (err) => settleNative(item, err),
    );
  }
}
function notify(title, message) {
  try { browser.notifications.create({ type: "basic", title, message: String(message || "").slice(0, 240) }); } catch (e) {}
}
async function safeSend(url, tab, extra = {}) {
  try { return await sendDownload(url, tab, extra); }
  catch (e) { if (!extra.captured) notify("webcraft: download not sent", "Native host unreachable. Start webcraft desktop app once, then retry. (" + (e && e.message || e) + ")"); throw e; }
}
// Ask-first capture removed: captures go straight to webcraft modal.
// ponytail: URL-only bypass for 60s; a same-URL re-click inside window goes
// to browser. Ceiling: cannot key by download id, id unknown at webRequest time.
const bypassUrls = new Map(); // url -> time
function markBypass(url) {
  const now = Date.now();
  bypassUrls.set(url, now);
  for (const [u, t] of bypassUrls) if (now - t > 60000) bypassUrls.delete(u);
}
function isBypassed(url) {
  const t = bypassUrls.get(url);
  if (!t) return false;
  if (Date.now() - t > 60000) { bypassUrls.delete(url); return false; }
  return true;
}
async function browserDownload(p) {
  markBypass(p.url);
  const opts = { url: p.url };
  if (p.filename) opts.filename = basename(p.filename);
  try { await browser.downloads.download(opts); }
  catch (e) { if (!opts.filename) throw e; markBypass(p.url); await browser.downloads.download({ url: p.url }); }
}

// --- Capture tables (IDM: Extensions registry list + MIME->ext map subset) ---

const PANEL_EXTS = new Set("aac avi f4v flv m4a m4v mkv mov mp3 mp4 mpeg mpg ogg ogv rm ts wav webm wma wmv 3gp".split(" "));
const STREAM_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl", "application/x-mpegurl", "video/vnd.mpeg.dash.mpd", "video/mpegurl", "video/x-mpegurl", "application/mpegurl"]);
const CANDIDATE_TTL_MS = 30 * 60 * 1000;
const CANDIDATE_MAX = 200;
const candidates = new Map(); // url -> {url, type, tabId, frameId, redirectFrom, time, size}
// IDM model: every in-flight request keyed by requestId (transient);
// on completion/error record dies, candidate (URL index) stays.
const requestRecords = new Map(); // requestId -> {url, redirectFrom, hops, tabId, frameId, time, type, size, status}
const RECORD_TTL_MS = 10 * 60 * 1000;
const RECORD_MAX = 500;
const MEDIA_PATH = /\.(mpd|m3u8|mp4|webm|ts|m4s)($|[?#])/i;
const MEDIA_TYPES = new Set(["media", "xmlhttprequest", "fetch", "other", "object"]);
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
    return typeof DCRank !== "undefined" && DCRank && typeof DCRank.hostMatchesScope === "function" && ["instagram.com", "facebook.com", "youtube.com", "x.com", "reddit.com", "tiktok.com", "vimeo.com", "twitch.tv", "dailymotion.com", "bilibili.com", "globo.com"].some((ph) => ph !== h && DCRank.hostMatchesScope(ph, h));
  } catch (e) { return false; }
}
function isMediaIsh(details) {
  // Record only media-ish requests; full <all_urls> recording bloats TTL map
  // with page/API traffic element matcher never needs.
  if (isMetaCdnUrl(details.url)) return true;
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
// X- tokens...). Stack-owned / hop-by-hop / conditional / content names
// are never forwarded: the engine's send-time denylist is authoritative,
// and host flattening re-filters. Raw Cookie never forwarded —
// cookies.getAll covers it and the engine Cookie field stays canonical.
// Captured on onBeforeSendHeaders ("requestHeaders" only — Firefox has
// no "extraHeaders" extraInfoSpec; never add it here). Values over 4096
// are DROPPED (never sliced — a truncated token is worse than none);
// duplicates join with ", ". Same TTL discipline as requestRecords.
// Kept in sync with Chromium (except the listener spec).
const HEADER_DENY = new Set(["cookie", "cookie2", "host", "content-length", "connection", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-connection", "accept-encoding", "range", "if-range", "if-modified-since", "if-none-match", "if-unmodified-since", "if-match", "cache-control", "pragma", "content-type", "priority", "set-cookie", "www-authenticate"]);
const HEADER_MAX_PER_URL = 24;
const urlHeaders = new Map(); // url -> {headers, time}
function headerKeyIndex(url) {
  // Dual key: exact URL plus origin+path, so a redirect-final or
  // query-drifted send still finds the browser's headers.
  try { const u = new URL(url); return u.origin + u.pathname; } catch (e) { return url; }
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
function isPlaylistUrl(u) { try { const p = new URL(u).pathname.toLowerCase(); return p.endsWith(".mpd") || p.endsWith(".m3u8"); } catch (e) { return /\.(mpd|m3u8)($|[?#])/i.test(String(u || "")); } }
function pruneCandidates(now = Date.now()) {
  for (const [url, c] of candidates) if (now - c.time > (isPlaylistUrl(url) ? 30 * 60 * 1000 : CANDIDATE_TTL_MS)) candidates.delete(url);
  // Cap: evict non-playlists (oldest first) before playlists; playlists alone over cap fall back to FIFO.
  if (candidates.size > CANDIDATE_MAX) { for (const url of [...candidates.keys()]) { if (candidates.size <= CANDIDATE_MAX) break; if (!isPlaylistUrl(url)) candidates.delete(url); } }
  while (candidates.size > CANDIDATE_MAX) candidates.delete(candidates.keys().next().value);
}
function headerOf(headers, name) { const h = (headers || []).find((x) => x.name.toLowerCase() === name); return h ? h.value || "" : ""; }
function totalSizeFromHeaders(headers, statusCode) {
  // 206 Content-Length is range length, not total; Content-Range total wins.
  if (statusCode === 206) {
    const m = /\/(\d+)\s*$/.exec(headerOf(headers, "content-range") || "");
    if (m) { const n = parseInt(m[1], 10); if (n > 0) return n; } // total '*' falls through
  }
  return parseInt(headerOf(headers, "content-length"), 10) || 0;
}
function extOfUrl(u) { try { const seg = new URL(u).pathname.split("/").pop() || ""; const i = seg.lastIndexOf("."); return i > 0 ? seg.slice(i + 1).toLowerCase() : ""; } catch (e) { return ""; } }
function filenameFromDisp(disp, url) {
  const m = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(disp || "");
  if (m) { try { return decodeURIComponent(m[1]); } catch (e) { return m[1]; } }
  return filenameFromUrl(url);
}
function filenameFromUrl(u) { try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; } }
function basename(name) { return name ? String(name).split(/[\\/]/).pop() || "" : ""; }
// IDM takeover rules: status ok, attachment, binary/media type, or known file extension.
function captureDecision(details) {
  if (isBypassed(details.url)) return null;
  if (![200, 206].includes(details.statusCode)) return null;
  if (/^(seg|segment|chunk|frag)[-_]?\d|\.m4s$|[-_]\d+\.(ts|m4s)$/i.test(filenameFromUrl(details.url))) return null; // stream segments: panel lists manifest, not fragments
  if (["ts", "m4s"].includes(extOfUrl(details.url))) return null; // parity Chromium isSniff ext gate: unnumbered segment files
  const type = headerOf(details.responseHeaders, "content-type").split(";")[0].trim().toLowerCase();
  const disp = headerOf(details.responseHeaders, "content-disposition").toLowerCase();
  const ext = extOfUrl(details.url);
  let urlPath = "";
  try { urlPath = new URL(details.url || "").pathname.toLowerCase(); } catch (e) { urlPath = String(details.url || "").split(/[?#]/)[0].toLowerCase(); }
  if (/\.(json|js|mjs|css|html?|map)$/i.test(urlPath)) return null; // pathname only: query ext false-rejects signed IG URLs
  const fname = filenameFromDisp(disp, details.url);
  const isStream = STREAM_TYPES.has(type) || ext === "m3u8" || ext === "mpd";
  const isMedia = type.startsWith("video/") || type.startsWith("audio/") || PANEL_EXTS.has(ext);
  // Meta CDN sniff (IG/FB/Threads share cdninstagram/fbcdn) + known registry
  // CDN pairs (googlevideo/twimg/...) via the same sniff allowances.
  const igSniff = (isMetaCdnUrl(details.url) || isKnownCdnHost(details.url)) && (type.startsWith("video/") || type.startsWith("audio/") || STREAM_TYPES.has(type) || type === "application/octet-stream" || !type || urlPath.endsWith(".mpd") || urlPath.endsWith(".m3u8"));
  if (isStream || isMedia || igSniff) return { url: details.url, type: type || (ext === "mpd" ? "application/dash+xml" : "application/x-mpegurl"), tabId: details.tabId, kind: "panel" };
  const isAttach = disp.startsWith("attachment");
  // NB: nearly all application/* is JSON/JS API traffic. Requiring real file
  // ext (no fname fallback) stops drive-by capture of site XHR.
  const isBinary = type === "application/octet-stream" || type === "application/x-download"
    || (type.startsWith("application/") && !/json|javascript|ecmascript|xhtml/.test(type));
  // Attachment is user intent, but json/js/css payloads are page assets, not downloads.
  var fnameExt = (String(fname || "").split(".").pop() || "").toLowerCase();
  if (/^(json|js|mjs|css|html?)$/.test(fnameExt) || /^(json|js|mjs|css|html?)$/.test(ext)) return null;
  // Dumb pipe: any attachment/binary is candidate; host captureCheck
  // (webcraft Settings) is sole policy decider. Local table removed.
  if (isAttach || isBinary) {
    if (type === "text/html" && !isAttach) return null;
    return { url: details.url, type, tabId: details.tabId, kind: "download", filename: fname, mime: type };
  }
  return null;
}
if (browser.webRequest) {
  try {
    const F = { urls: ["<all_urls>"] };
    const kill = (d) => { try { requestRecords.delete(d.requestId); } catch (e) {} };
    if (browser.webRequest.onBeforeRequest) browser.webRequest.onBeforeRequest.addListener(recordRequest, F);
    if (browser.webRequest.onBeforeSendHeaders) browser.webRequest.onBeforeSendHeaders.addListener(recordRequest, F, ["requestHeaders"]);
    if (browser.webRequest.onCompleted) browser.webRequest.onCompleted.addListener(kill, F);
    if (browser.webRequest.onErrorOccurred) browser.webRequest.onErrorOccurred.addListener(kill, F);
  } catch (e) {}
}
if (browser.webRequest && browser.webRequest.onHeadersReceived) {
  browser.webRequest.onHeadersReceived.addListener(async (details) => {
    recordRequest(details);
    const rec = requestRecords.get(details.requestId);
    if (rec) { rec.status = details.statusCode || 0; rec.size = totalSizeFromHeaders(details.responseHeaders, details.statusCode) || rec.size || 0; const ct = headerOf(details.responseHeaders, "content-type").split(";")[0].trim().toLowerCase(); if (ct) rec.type = ct; }
    const d = captureDecision(details);
    if (!d) return {};
    // XHR/other: observe-only. Cancelling them kills site API calls.
    if (d.kind === "download" && (details.type === "xmlhttprequest" || details.type === "other")) return {};
    if (d.kind === "panel") {
      candidates.set(d.url, { url: d.url, type: d.type, status: details.statusCode || 0, tabId: d.tabId, frameId: details.frameId != null ? details.frameId : null, redirectFrom: rec ? rec.redirectFrom : "", hops: rec ? (rec.hops || []) : [], time: rec ? rec.time : Date.now(), size: totalSizeFromHeaders(details.responseHeaders, details.statusCode) });
      pruneCandidates();
      return {};
    }
    // Direct to webcraft modal (no browser ask box). Host decides filter
    // (webcraft Settings) BEFORE cancel: deny -> browser keeps file.
    try {
      const size = totalSizeFromHeaders(details.responseHeaders, details.statusCode);
      let allow = false;
      try { const r = await nativeSend({ type: "captureCheck", url: d.url, filename: d.filename, mime: d.mime, size }); allow = !!(r && r.allow === true); } catch (e) { allow = false; }
      if (!allow) return {}; // filtered/host down: browser downloads
      const ref = details.originUrl || details.documentUrl || "";
      const tab = details.tabId >= 0 ? await browser.tabs.get(details.tabId).catch(() => null) : null;
      markBypass(d.url); // BEFORE send: onCreated landing mid-send must skip
      const resp = await safeSend(d.url, tab, { filename: d.filename, mime: d.mime, referrer: ref, size, captured: true });
      // Not yet cancelled (return value decides): skipped/error -> browser keeps file.
      if (!resp || resp.skipped || resp.type === "error") return {};
      return { cancel: true };
    } catch (e) { return {}; } // native failed: let browser download proceed
  }, { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "media", "xmlhttprequest", "other"] }, ["blocking", "responseHeaders"]);
}
async function sendDownload(url, tab, extra = {}) {
  let cookies = "";
  let cookiesJson = "";
  // Cookies for page host, not sniffed CDN host (see Chromium).
  // storeId from the tab so container/private sessions resolve their own
  // cookie store, not the default one.
  try { const cq = { url: extra.pageUrl || extra.cookieUrl || url }; if (tab && tab.cookieStoreId) cq.storeId = tab.cookieStoreId; const cs = await browser.cookies.getAll(cq); cookies = cs.map(c => `${c.name}=${c.value}`).join("; "); cookiesJson = JSON.stringify(cs.map(c => ({ name: c.name, value: c.value, domain: c.domain, path: c.path, expirationDate: c.expirationDate, secure: c.secure, httpOnly: c.httpOnly, hostOnly: c.hostOnly }))); } catch (e) {}
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
// Menus: create on startup too (event-page restarts lose onInstalled-only menus).
function createMenus() {
  try {
    browser.contextMenus.create({ id: "downloadcraft-link", title: "Download with webcraft", contexts: ["link"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-media", title: "Download media with webcraft", contexts: ["video", "audio", "image"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with webcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-grab", title: "Download site with webcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-scrap", title: "Scrap site with webcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
  } catch (e) {}
}
browser.runtime.onInstalled.addListener(createMenus);
createMenus();
browser.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) safeSend(info.linkUrl, tab).catch(() => {});
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl && /^(blob|data):/.test(info.srcUrl)) { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }).catch(() => {}); }
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) safeSend(info.srcUrl, tab).catch(() => {});
  // Stale tab (no content script): fall back to grab-site instead of silent dead click.
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" }).catch(() => { if (tab.url) safeSend("webcraft://grab?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url, cookieUrl: tab.url }).catch(() => {}); });
  // "Download site" -> Add URL/File dialog prefilled with the page URL
  // (webcraft://adddial, NOT add?url= which is the IDM capture-confirm modal).
  else if (info.menuItemId === "downloadcraft-grab" && tab && tab.url) safeSend("webcraft://adddial?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url, cookieUrl: tab.url }).catch(() => {});
  // "Scrap site" -> Site Grabber (auto-explore, cookies ride along).
  else if (info.menuItemId === "downloadcraft-scrap" && tab && tab.url) safeSend("webcraft://grab?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url, cookieUrl: tab.url }).catch(() => {});
});
// Never intercept page-asset downloads (json/js/css) even with download attr.
function isAssetDownload(item) {
  const m = String((item && item.mime) || "").toLowerCase();
  if (/json|javascript|ecmascript|css/.test(m)) return true;
  const f = String((item && item.filename) || (item && item.url) || "").split(/[?#]/)[0];
  return /\.(json|js|mjs|css)$/i.test(f);
}
// Cancel-first fallback: blocking webRequest path above stays primary and
// decides before body (deny = browser untouched, never stalled). This covers
// downloads it cannot decide (Save-As, PDF save, XHR/other observe-only).
// Sync guards + sync cancel only: no await, no pause, before cancel. Item
// already cancelled, so verdict re-issues instead of resuming: deny/host-down
// -> browserDownload (fresh browser download); allow -> webcraft. Tradeoff:
// bytes may leak until cancel lands, re-download restarts from scratch
// (same as Chromium cancel-first).
function sizeOfItem(it) { return it && it.totalBytes > 0 ? it.totalBytes : (it && it.fileSize > 0 ? it.fileSize : 0); }
async function captureCheck(entry) {
  // Fail-closed verdict (garbage/host down = deny); caller re-issues in browser.
  try { const r = await nativeSend({ type: "captureCheck", url: entry.url, filename: entry.filename, mime: entry.mime, size: entry.size }); return !!(r && r.allow === true); } catch (e) { return false; }
}
browser.downloads.onCreated.addListener((item) => {
  if (isAssetDownload(item)) return;
  if (!item || !item.url || item.id == null) return;
  if (isBypassed(item.url) || isBypassed(item.finalUrl || item.url)) return;
  if (item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  const url = item.finalUrl || item.url;
  if (!url || url.startsWith("blob:") || url.startsWith("data:")) return;
  const entry = { url, filename: basename(item.filename), mime: item.mime || "", size: sizeOfItem(item), referrer: item.referrer || "" };
  const id = item.id;
  // FIRST action after sync guards: cancel in same handler tick.
  try { browser.downloads.cancel(id).catch(() => {}); } catch (e) {}
  (async () => {
    try {
      const items = await browser.downloads.search({ id }).catch(() => null);
      const cur = items && items[0];
      if (cur && cur.state === "complete") return; // browser won before cancel landed: no duplicate send
      // "interrupted" expected here: own cancel sets it; not terminal proof.
      if (cur) {
        if (!(entry.size > 0)) entry.size = sizeOfItem(cur);
        if (!entry.filename) entry.filename = basename(cur.filename);
        if (!entry.mime) entry.mime = cur.mime || "";
      }
      if (isBypassed(entry.url)) return; // claimed after sync guards (webRequest markBypass before send): claimant owns delivery, this item already cancelled by us
      if (!(await captureCheck(entry))) { browserDownload(entry).catch(() => {}); return; } // deny/host-down: fresh browser download (re-issue marks bypass, re-entrant onCreated skips)
      if (isBypassed(entry.url)) return; // claimed during check: no duplicate send
      markBypass(entry.url); // BEFORE send: re-entrant onCreated must skip
      await browser.downloads.removeFile(id).catch(() => {}); // drop cancelled partial: webcraft owns file now
      try { await browser.downloads.erase({ id }).catch(() => {}); } catch (e) {}
      const resp = await safeSend(entry.url, null, { referrer: entry.referrer, filename: entry.filename, mime: entry.mime, size: entry.size, captured: true });
      if (!resp || resp.skipped || resp.type === "error") browserDownload(entry).catch(() => {}); // skipped/error: already cancelled, re-download in browser
    } catch (e) { browserDownload(entry).catch(() => {}); } // host died mid-send: file must not vanish
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
      browserDownload(it).catch(() => {});
    }
  } catch (e) {} // host down/old host: silently no-op
}
try { setInterval(() => { pollResumeOnce().catch(() => {}); refreshBtnStyle().catch(() => {}); }, 2000); } catch (e) {}
try { pollResumeOnce().catch(() => {}); } catch (e) {} // immediate poll (persistent bg)

// Button appearance: host owns style {icon,w,h,radius}; old/down host replies
// invalid/missing -> keep last good cache. Persistent bg: memory cache is
// primary; storage.session mirror persisted only when that API is present.
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
    if (browser.storage && browser.storage.session) {
      const s = await browser.storage.session.get(["wcBtnStyle"]);
      const v = s && validBtnStyle(s.wcBtnStyle);
      if (v) wcBtnStyle = v;
    }
  } catch (e) {}
}
function broadcastBtnStyle() {
  try {
    browser.tabs.query({}).then((tabs) => {
      for (const t of tabs || []) {
        if (!t || t.id == null) continue;
        try { browser.tabs.sendMessage(t.id, { type: "downloadcraft-button-style", style: wcBtnStyle }).catch(() => {}); } catch (e) {}
      }
    }, () => {});
  } catch (e) {}
}
async function refreshBtnStyle(force) {
  if (!force && Date.now() - wcBtnStyleLastTry < 25000) return wcBtnStyle; // ~30s poll cadence (2s interval above)
  wcBtnStyleLastTry = Date.now();
  await loadBtnStyle();
  let v = null;
  try { v = validBtnStyle(await nativeSend({ type: "getButtonStyle" })); } catch (e) {}
  if (!v) return wcBtnStyle; // host down/old: keep cache, no broadcast
  const changed = JSON.stringify(v) !== JSON.stringify(wcBtnStyle);
  wcBtnStyle = v;
  try { if (browser.storage && browser.storage.session) browser.storage.session.set({ wcBtnStyle: v }).catch(() => {}); } catch (e) {}
  if (changed) broadcastBtnStyle();
  return wcBtnStyle;
}
refreshBtnStyle(true).catch(() => {}); // startup refresh (restores session cache first)

browser.runtime.onMessage.addListener((msg, sender) => {
  // Legacy stale content scripts may still answer removed ask box: ignore.
  if (msg && msg.type === "downloadcraft-confirm-result") return Promise.resolve({ ok: true });
  if (msg && msg.type === "downloadcraft-get-button-style") {
    if (wcBtnStyle) return Promise.resolve({ style: wcBtnStyle });
    return refreshBtnStyle(true).then((s) => ({ style: s }), () => ({ style: wcBtnStyle }));
  }
  if (msg && msg.type === "getCandidates") {
    pruneCandidates();
    sweepRecords();
    const tabId = sender.tab && sender.tab.id;
    let pool;
    if (msg.element) {
      // Element-scoped (IDM): same tab+frame only, never other-frame/tab.
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
      // Site host-scoped fallback (background-only): see Chromium.
      // Scope the tab's media to the page's own site: the registry
      // pairs page hosts with their CDN hosts (Meta->cdninstagram/
      // fbcdn, YouTube->googlevideo, X->twimg, ...). noTiming
      // likewise: hostExtra is window-bound itself.
      if (!pool.length && typeof matchRecordsForElement === "function") {
        let ph = "";
        try { ph = new URL((sender.tab && sender.tab.url) || "").hostname.toLowerCase(); } catch (e) {}
        if (ph) pool = matchRecordsForElement([...byUrl.values()], { ...el, frameId: null, noTiming: true, pageHost: ph }, Date.now(), [...candidates.values()]);
      }
    } else {
      // No frame filter: player frame != sniffer frame on embeds drops all.
      pool = [...candidates.values()].filter((c) => tabId == null || c.tabId === tabId);
    }
    const list = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)(pool, msg.element ? el : null);
    return Promise.resolve({ type: "candidates", candidates: list });
  }
  if (msg && msg.type === "downloadcraft-grab-site" && sender.tab && sender.tab.url) return safeSend("webcraft://grab?url=" + encodeURIComponent(sender.tab.url), sender.tab, { referrer: sender.tab.url, cookieUrl: sender.tab.url });
  if (msg && (msg.type === "downloadcraft-download" || msg.type === "downloadcraft-video-panel")) return safeSend(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer || msg.pageUrl, pageTitle: msg.pageTitle, pageUrl: msg.intent === "page" ? msg.pageUrl : undefined, cookieUrl: msg.pageUrl, intent: msg.intent });
});

// Toolbar click: scan tab for video / page download (was no-op).
// Toolbar click -> content scan (unchanged); IG fallback lives in getCandidates above.
if (browser.browserAction && browser.browserAction.onClicked) browser.browserAction.onClicked.addListener((tab) => { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }).catch(() => {}); });
// SPA pushState nav: url change without status=loading also drops stale streams.
const tabLastUrl = new Map();
if (browser.tabs && browser.tabs.onUpdated) browser.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status !== "loading" || !info.url) return;
  if (tabLastUrl.get(tabId) === info.url) return;
  tabLastUrl.set(tabId, info.url);
  for (const [u, c] of [...candidates]) if (c.tabId === tabId) candidates.delete(u);
  for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k);
});
if (browser.tabs && browser.tabs.onRemoved) browser.tabs.onRemoved.addListener((tabId) => { for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k); for (const [u, c] of [...candidates]) if (c.tabId === tabId) candidates.delete(u); });
