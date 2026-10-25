<p align="center">
  <img src="assets/webcraft.png" alt="webcraft" width="128">
</p>

<h1 align="center">webcraft</h1>

<p align="center">
  A fast, cross-platform download manager written in Rust — segmented downloads,
  browser capture, debrid support, and built-in video playback.
</p>

<p align="center">
  <img alt="Version" src="https://img.shields.io/badge/version-0.25.0-blue">
  <img alt="Platforms" src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey">
  <img alt="Language" src="https://img.shields.io/badge/built%20with-Rust%20%2B%20egui-orange">
  <a href="https://addons.mozilla.org/en-US/firefox/addon/webcraft/"><img alt="Firefox Add-on" src="https://img.shields.io/badge/Firefox-Add--on%20on%20AMO-FF7139?logo=firefoxbrowser&logoColor=white"></a>
</p>

---

webcraft reimplements the observable behavior of Internet Download Manager from scratch: segmented downloads that resume, queues and schedules, DASH/HLS handling, a site grabber, and deep browser integration. No IDM code, binaries, icons, or activation logic — and no affiliation with Tonec.

**Current version:** 0.25.0 · Changelog lives in GitHub Releases.

**Runs on:** Windows 10/11 x86_64 · macOS 13+ (Intel and Apple Silicon) · Linux x86_64 (Ubuntu 22.04+, X11 and Wayland).

## Highlights

- **Fast starts** — links are probed and segmented in the background as soon as they are captured or pasted into the Add dialog, so confirming a download goes straight to the first byte. Queued tasks wake instantly instead of waiting for the next poll.
- **Segmented HTTP/S** — parallel connections with resume, dynamic tail-splitting when a connection stalls, and a hard cap so segments never multiply into dozens of tiny parts.
- **Debrid built in** — Real-Debrid, AllDebrid, and TorBox resolve hoster links and magnets; the list and progress dialog show which provider and hoster served each file.
- **Every protocol** — FTP/FTPS/SFTP, BitTorrent, NNTP/NZB with PAR2 repair, Metalink, WebDAV, DASH, and HLS.
- **Watch while you download** — video pages resolve through yt-dlp with a format/subtitle picker and browser cookies, and streams can play directly in mpv (embedded or external).
- **Browser capture** — Chromium and Firefox extensions detect media on the page, pass cookies, and hand downloads to the app silently.

## Features

### Download engine

- Segmented HTTP/S downloads that resume from part files after a restart, with work-stealing across a fixed connection pool (configurable in Options) and correct byte accounting — downloaded never exceeds total
- Options → **Part size MB (0 = auto)**: `0` auto-scales part size with file size; a set value is the exact part target (e.g. a 6 GiB file at 512 MB gives 12 parts). Applies to new downloads only — resumed downloads keep their saved segmentation
- Pre-warm probing on add/capture: range probe, redirect resolution, and segment planning happen before you confirm; probe responses are reused instead of re-downloaded
- Custom HTTP headers per download (User-Agent, Referer, Authorization Bearer, or any header) from the Add dialog's *Headers (advanced)* section — HTTP/S only
- Per-download proxy override (Add URL/File *Transfer → Proxy*, browser Capture, or Progress dialog — Progress edits apply on restart); empty inherits the global Settings proxy. Applies to HTTP/HTTPS, HLS/DASH, Metalink, debrid, fetching NZB/.torrent files, and yt-dlp probe/download (`--proxy`); FTP/SFTP, NNTP articles, and torrent swarm stay direct. HTTP and SOCKS5 proxies work, with credentials in the URL
- Mirrors per task with failover, per-task and global speed limits, SHA-256 verification, rename templates (`{name}`, `{ext}`, `{date}`, `{host}`, `{category}`), and category subfolders

### Protocols

| Protocol | Notes |
| --- | --- |
| HTTP / HTTPS | Segmented, resumable, mirror failover |
| FTP | EPSV → PASV → active (PORT) fallback, NAT-aware PASV address handling, segmented downloads when the server accepts `REST` |
| FTPS | Explicit (`ftps://`) and implicit (`ftpsi://`) TLS; login happens after `AUTH TLS` |
| SFTP | Password, key, and keyboard-interactive auth with fallback; trust-on-first-use host keys; seek resume |
| BitTorrent | Magnet links and `.torrent` files, per-file progress, optional seeding after completion |
| NNTP / NZB | yEnc decoding, CRC32 checks, TLS, in-process PAR2 repair when the NZB includes `.par2` files |
| Metalink | v3 and v4, with bundled mirrors and SHA-256 checksums |
| WebDAV | `webdav://` and `webdavs://`, mapped to HTTP/HTTPS |
| DASH / HLS | MPD and playlist downloads, AES-128 HLS, subtitle sidecars. DRM-protected streams are refused |

### Drives (cloud storage)

- Drives tab (next to Streams / Site Grabber): dual-pane browser — This PC (download folder) or any account per pane, breadcrumb navigation, copy/move/delete/mkdir/rename, transfer panel with pause/resume/cancel and speed. Copy skips existing names unless *Overwrite* on. WebDAV files can also go *To Downloads* into segmented engine.
- **Server-side copy:** when both panes are on Google Drive accounts, a *Server-side copy* button appears in the pane action bar: Drive copies bytes server-to-server (`files.copy`) with no data passing through your machine — large files land in seconds. No staging fallback: permission failures surface as job errors. Cross-account copies only work when destination account can already see source file (Google hides unshared files as 404 File not found): share file, parent folder, or Shared Drive with destination account first, or use normal Copy for cross-account transfers (download+upload through local temp file). S3 → S3 copies stay silent and automatic (`CopyObject`) with staging fallback; objects over 5 GiB skip to staging. Crypt-wrapped accounts always stage, and Move only deletes the source after a copy that verifiably succeeded.
- Accounts (unlimited, no tier limits): Google Drive, OneDrive, Dropbox (OAuth in browser — paste your own OAuth client_id/secret from Google/Azure/Dropbox app; tokens plain in `state.json` like Debrid tokens, auto-refreshed), WebDAV (Nextcloud/ownCloud/etc app passwords), S3-compatible (AWS/R2/B2/MinIO, SigV4, multipart), SFTP, FTP/FTPS, Telegram via your self-hosted teldrive server (server URL + API key), local folders.
- Getting tokens — Wizard (rclone config order): from empty Drives tab or Accounts > Add account. 1 Provider 2 Details+Setup steps (console URL, clicks, Client ID/secret fields) 3 Connect (OAuth browser opens at redirect `http://127.0.0.1:53682/` registered in console — Google Desktop app needs none — or paste-code fallback; key/password providers Test here) 4 Root: Google separate Team Drive pick (list with new token, My Drive default) + optional start folder (paste drive folder link/id), S3 bucket, other root paths 5 Review with final test > Finish. Only Finish saves account (no save-first trips), then auto-opens it in a pane. Cancel saves nothing.
- Multithreaded: per-account Parallel streams (Edit account) = concurrent files per job plus segments per file; parallel ranged downloads, SFTP parallel sessions, S3 multipart parallel parts, Drive/Dropbox/OneDrive session chunk uploads. FTP single-stream per file.
- Sync: saved jobs, compare preview first, copy (missing) / update (newer or bigger) / mirror (plus delete destination extras) between any two accounts.
- Optional per-account rclone-crypt encryption: file contents + filenames encrypted on upload (scrypt keys, standard name encryption), compatible with rclone crypt remotes using same password/salt.
- Import: Accounts > Import rclone — paste rclone.conf sections to create accounts (obscured passwords revealed; token/team_drive mapped; warnings for bucket/root fields to finish).

### Debrid

- Real-Debrid, AllDebrid, and TorBox: hoster links and magnets are unrestricted through your account, in your preferred provider order
- Alias domains route to a capable provider instead of direct HTTP: `mega.nz` / `mega.co.nz` / `mega.io` and `rapidgator.net` / `rg.to` / `rapidgator.asia` are treated as one hoster each
- Provider refusals (e.g. `infringing_file`) surface as task errors — the hoster HTML page is never saved as a fake completed file
- TorBox: non-cached hoster links create a server-side web download; the app polls until TorBox reports it ready, then downloads, and surfaces server errors
- Magnet results fan out to one task per file; hoster checks are cached so repeat adds start fast
- The downloads list shows a Debrid badge and the progress dialog names the provider, hoster, and cache status — no silent fallbacks

### Video and playback

- Video pages on YouTube, Vimeo, TikTok, Twitch, Dailymotion, Instagram, Threads, X, Facebook, Reddit, SoundCloud, Bilibili, NicoNico, Rumble, Odysee, PeerTube, and short-link hosts, resolved by a yt-dlp sidecar (native DASH/HLS fallback)
- Format, audio, and subtitle picker before downloading; browser cookies passed from the extension for login-gated pages; yt-dlp runs silently with no console windows
- Direct playback with mpv: Streams page, embedded player controls (pause, seek, volume, speed), and external mpv with your own options
- Drives: double-click a video file to open it in the built-in player popup — local files play direct, remote drive files stream in place (provider URL + auth headers into mpv; Google Drive, OneDrive, Dropbox, S3 presigned, WebDAV, Teldrive; SFTP/FTP/crypt accounts cannot stream and ask you to download first). No rclone mount needed. The player popup now has **Audio** and **Subtitle** track selectors (subtitle Off included), **Load sub…** for an external subtitle file, and a **Sub style** row — size, position, text/shadow color, bold, and force-on-ASS — applied live and remembered in *Options → mpv → Subtitle style*. Chapter dropdown with Prev/Next buttons appears when file has chapters. Dropdowns hide video frame while open (native video cannot layer under egui menus); Esc exits fullscreen, Full/Window toggles, Close quits player. Embedded player ignores your mpv config/scripts (`--no-config`) so external OSC/OSD overlays never leak in; detached mpv keeps them

### Automation and interface

- Queues with priority, retry counts, and per-queue concurrency; schedules with start/stop windows, weekday masks, quotas, missed-run catch-up, and completion actions (including shutdown with confirmation)
- Site Grabber with depth, scope, size, and extension filters and a mirror mode that rewrites links to local relative paths — including SharePoint via browser cookies
- Progress dialog with per-part status, speed graph, and live details; downloads table with saved sort/widths, protocol badges, and type filtering from the side rail
- File size is highlighted (strong accent) in the progress and completed windows, with downloaded / total / percent; when total size is unknown (HLS/DASH), the completed window falls back to the final downloaded bytes
- Chatbot app actions: ask the assistant to add a download (file, magnet, `.torrent`, NZB, or video page), pause / resume / delete / open-folder by name, start / stop all, delete completed, or change a whitelisted setting; `list_tasks` / `search_tasks` / `get_stats` run instantly, app state is supplied via an `[APPDATA]` context block, unsupported actions are reported instead of dropped, and mutating actions ask for Confirm first. Chat history persists across restarts (`chat-history.json`; New/History/Export .md/Search/Clear in the Chatbot header) with an append-only audit log (`chat-audit.jsonl`). Live answers: ask about local weather/news (IP geolocation, or name a place — `Clima em Xique-Xique, Bahia`), say `pesquise/search/buscar …` for web search, or paste a URL to have the page fetched — results arrive as `[CONTEXT]` with source URLs cited. The model never touches your PC directly: actions are proposed, you confirm
- History tab + app-wide logs: every download event (added/started/paused/resumed/completed/failed/removed, host only — never full URLs) and ~5 s speed samples persist in daily JSONL under `dl-history/`, app events under `logs/`; both share a 50 MiB budget with 90-day retention and daily rotation. The History tab shows 7/30/90-day totals, bytes-per-day, a 24 h speed chart with axes, top hosts, and recent items — graphs survive restart, and the chatbot answers usage questions ("how much did I download this week?") from the same history
- Desktop notifications, system tray, autostart, single-instance lock, clipboard URL monitoring, drag-and-drop, HTTP/SOCKS5 proxy with authentication (global default, per-download override — see Download engine), and atomic state saving with a `.bak` fallback

> [!NOTE]
> Multi-track downloads are muxed with ffmpeg when ffmpeg is on PATH or beside the exe; single-track downloads do not need it. The app never downloads ffmpeg at runtime. yt-dlp is fetched from its GitHub releases when no binary is found, and updated after 30 days.

## Getting started

### Install

- **Windows:** run `webcraft-*-windows-setup.exe` (NSIS, per-user, no admin) or unpack the portable zip. Uninstall from Settings → Apps, or run `$LOCALAPPDATA\Programs\webcraft\Uninstall.exe`.
- **macOS:** open the `.dmg` and drag `webcraft.app` to Applications. Builds are unsigned, so the first launch needs right-click → Open.
- **Linux:** run the AppImage, unpack the tarball, or `sudo apt install ./webcraft_*_amd64.deb`. The .deb registers the desktop entry and protocol handler; GNOME tray needs AppIndicator support.

Older installs keep working: on first launch, webcraft copies `state.json`, `capture-filter.json`, and pending inbox messages from the previous (downloadcraft, or older idmcraft) config directory if the new one has no state yet. Existing webcraft state is never overwritten.

### Build from source

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
./target/release/webcraft --smoke-test
```

> [!TIP]
> On Windows, cargo may not be on PATH in a fresh PowerShell session. Use `$HOME\.cargo\bin\cargo.exe` or add `$HOME\.cargo\bin` to PATH first.

Artifacts land in `target/release/` (`webcraft` app, `webcraft-host` native messaging host) and `dist/` (portable zip via `scripts/package-windows.ps1`, Firefox XPI via `scripts/package-firefox.ps1`).

## Browser integration

The desktop app registers the native messaging host and the `webcraft://` protocol every time it starts, so moving the exe never leaves a stale host path. *Options → Repair browser integration* does the same by hand. The legacy `com.downloadcraft.host` name stays registered for one release cycle so already-installed extensions keep working.

- **Chromium:** load `extensions/chromium` as an unpacked extension.
- **Firefox:** install from [AMO](https://addons.mozilla.org/en-US/firefox/addon/webcraft/) — signed builds update automatically (start webcraft once first so the host is registered). Alternatively install `extensions/webcraft-firefox.xpi` from about:addons → gear → Install Add-on From File; release Firefox refuses unsigned add-ons permanently — use Developer Edition/Nightly, set `xpinstall.signatures.required` to false where allowed, or load `extensions/firefox` temporarily from about:debugging. Rebuild the XPI with `scripts/package-firefox.ps1`.

The extension watches downloads passively and ranks what it finds, so a page with many small segments offers the playlist or file first. Playlist candidates (.m3u8/.mpd) are kept for 30 minutes (other candidates 5), evicted last when the candidate cap is hit, and partial (206) responses count as candidates too — so a stream detected early is still offered when you click later. It hands video-page links to the app with the page's cookies, where yt-dlp resolves formats. Capture can be limited by an extension allowlist and minimum size in Options (`capture-filter.json`). Video buttons appear when playback starts and reveal fully on hover; at rest they stay semi-visible and clickable on all sites, including players whose overlay layers cover the `<video>` element. Pages whose player only declares itself declaratively (Globo `<bs-player>`, mounted on play) get a discreet corner button that sends the article page to yt-dlp. The button lives at page level and moves into the fullscreen subtree while fullscreen is active, so it shows in fullscreen viewers without being affected by site player wrappers. The per-video link is found generically from the clicked video (wrapping anchor, timestamp/permalink link in its container), so feed videos download their own item page — never the feed home URL. A shared site registry (YouTube, Instagram, Facebook, Threads, TikTok, X, Reddit, Vimeo, Twitch, Dailymotion, Bilibili, NicoNico, SoundCloud, Rumble, Odysee, PeerTube, Globo) classifies each page as a single video or a feed before anything is offered: on a recognized video page the button sends that video's page URL to yt-dlp, otherwise it offers the sniffed manifest or direct stream, correlated to the clicked video by source, page ID, timing, or the site's CDN host. Clicking the extension toolbar button on any non-feed page with a video offers the page the same way. Local fixtures under `tests/sandbox/` (`python tests/sandbox/run_sandbox.py`) exercise the button against simulated YouTube/Instagram/TikTok/feed/generic/Globo-bs-player pages plus stationary-mouse hover cases, and `node extensions/rank.test.mjs` covers the registry and ranking logic (also run in CI).

Extensions installed unpacked or from a release file never auto-update: after upgrading, reload the extension (Chromium: `chrome://extensions` → Reload; Firefox: reinstall the XPI) and close/reopen already-open tabs — a tab keeps the content script from when it was loaded. AMO-installed Firefox extensions update automatically. The repo-tracked `extensions/webcraft-firefox.xpi` is rebuilt each release; prefer the XPI attached to the GitHub release.

## Interface

<p align="center">
  <img src="images/interface.gif" alt="webcraft interface tour" width="900">
</p>

<table>
  <tr>
    <td align="center"><a href="images/01-downloads.png"><img src="images/01-downloads.png" width="280" alt="Downloads"></a><br><sub>Downloads</sub></td>
    <td align="center"><a href="images/02-add-download.png"><img src="images/02-add-download.png" width="280" alt="Add download"></a><br><sub>Add download</sub></td>
    <td align="center"><a href="images/03-progress.png"><img src="images/03-progress.png" width="280" alt="Progress"></a><br><sub>Progress</sub></td>
  </tr>
  <tr>
    <td align="center"><a href="images/04-download-complete.png"><img src="images/04-download-complete.png" width="280" alt="Download complete"></a><br><sub>Download complete</sub></td>
    <td align="center"><a href="images/05-queues.png"><img src="images/05-queues.png" width="280" alt="Queues"></a><br><sub>Queues</sub></td>
    <td align="center"><a href="images/06-scheduler.png"><img src="images/06-scheduler.png" width="280" alt="Scheduler"></a><br><sub>Scheduler</sub></td>
  </tr>
  <tr>
    <td align="center"><a href="images/07-site-grabber.png"><img src="images/07-site-grabber.png" width="280" alt="Site Grabber"></a><br><sub>Site Grabber</sub></td>
    <td align="center"><a href="images/08-drives.png"><img src="images/08-drives.png" width="280" alt="Drives"></a><br><sub>Drives</sub></td>
    <td align="center"><a href="images/09-options.png"><img src="images/09-options.png" width="280" alt="Options"></a><br><sub>Options</sub></td>
  </tr>
</table>

<sub>Screenshots use demo downloads from a local test server; no real accounts or tokens.</sub>

The main window opens at 1440×900 and groups related actions and labels each group: download actions stay with the selected download, queue actions with all downloads. Narrow windows switch to a compact column set, persisted column widths shrink to fit instead of forcing horizontal scrolling, and the side rail scrolls when it runs out of height. Column widths and sort order persist between launches. Add (560×840), Capture (560×700), Progress (760×860), Collision (560×240), and Player (960×600) dialogs open as separate windows; opening Add or a browser Capture grows a smaller restored main window back to full size. In-window dialogs dim and block the window behind them, close with Esc, and never make a destructive action the Enter default. Closing the window quits; minimizing hides webcraft to the tray. Options save as soon as a setting changes.

## Documentation

- `docs/reverse-engineering.md` — observations of IDM behavior, DASH/HLS experiment tables, design decisions
- `docs/parity.md` — feature status with the tests backing each claim
- `docs/release-checklist.md` — manual per-OS sign-off list

## Limits

DRM-protected streams are refused on purpose. NNTP repair uses PAR2 files included in the NZB; archive auto-unpack (RAR/ZIP/7z, multi-volume; password from inside NZB: `<meta type="password">` or `{{password}}` in NZB/archive name) on by default in Options → NNTP — archive deleted only after verified extraction, failure keeps archive and marks download Error. Segmented FTP restarts the file when resumed (single-connection FTP resumes normally). There is no Safari extension — that needs Xcode signing. `docs/parity.md` lists the remaining partial items.
