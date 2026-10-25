# Changelog

All notable changes to webcraft. Versions follow the app version in Cargo.toml.

## [0.25.0] - 2026-10-24

### Added

- MEGA folder links: pasting a public folder URL enumerates its files
  via the MEGA API (no account needed) and creates one download task
  per file, each resolved through debrid or downloaded directly.
- Real-Debrid folder unrestrict (`POST /unrestrict/folder`) for MEGA
  folders, fixing `hoster_unsupported` errors on folder links that
  work on the Real-Debrid website.
- NZB health check: missing articles (NNTP 430/423/424) count
  immediately with no retries; a download aborts early with a
  user-visible error once 20% or more of checked articles (64+
  sample) are missing, and final errors report the missing share.
- Multi-server Usenet: configure extra NNTP provider servers; the
  total connection pool is split across all enabled servers.
- Options > General: in-app activity log panel.
- Saved proxy list: name and save proxies in Options, then pick one
  per download in Add URL/File or the browser capture dialog.
- Browser captures forward the browser's request headers
  (User-Agent, Accept-Language, Authorization when sent) alongside
  cookies, so UA/session-gated downloads keep working.

### Fixed

- Doubled `Real-Debrid: Real-Debrid:` prefix in debrid errors.
- NZB downloads stuck at 0.0% with no error: connect, article and
  stall timeouts now fail loudly.
- Browser cookies could be silently overwritten by a `Cookie` header
  row; the Cookies/Referrer fields now take precedence.

## [0.24.0] - 2026-10-23

### Added

- Always-on download button on every sized `<video>` on any page
  (IDM-style): visible at rest (opacity .42) with no play or hover
  needed, so paused videos (e.g. Reddit) and generic players get a
  button immediately. Feed-card buttons are retained.
- Options > Browser integration: 10 video-button icon/size presets,
  served to the extensions via the native host (`button-style.json`)
  and applied live with no browser restart. The "Button style" picker
  shows a live preview of the selected button (icon, size and corner
  radius as drawn in the browser), and each dropdown entry carries
  its own icon preview, so presets are identifiable by their icon.

### Fixed

- Globo GE / YouTube embeds "No streams detected": on a registry
  "page" (e.g. `ge.globo.com/.../*.ghtml` with a Clappr blob `<video>`,
  or a Shorts page), `permalinkFor` could walk scope links and offer
  a related article instead of the current page. Both extensions now
  return the cleaned current URL first when it classifies as a
  registry "page", and `showPanel` falls back to the page/embed item
  so the dead-end note is unreachable on video pages.
- YouTube embeds: `youtube-nocookie.com/embed/...` (and
  `/embed|v|live|shorts/<id>`, `youtu.be/<id>`) now canonicalize to
  `https://www.youtube.com/watch?v=<id>`; pages that embed a
  registry-"page" video in an `iframe`/`embed`/`object` get that
  embed as an `intent=page` offer plus a corner page button even
  with no local `<video>`. Rust `is_video_page` parity added for
  nocookie/embed/shorts (incl. `/shorts/DyvaWC4kIYw?feature=share`).
- Feed cards (YouTube home and shelves): cards carrying only a
  thumbnail link previously had no download button until a
  hover-preview `<video>` became active (IDM-style gap). Every card
  whose item link is a registry "page" now gets a definite button
  at rest opacity .42 (clickable, .96 on hover/focus) sending that
  card's watch/shorts URL directly with `intent=page`; the button
  yields to the per-video button once a `<video>` owns the card.
- Sandbox: fixtures for the GE `.ghtml` blob player, the nocookie
  iframe embed, YouTube Shorts (playing / 3-reel shell / paused
  `preload=none`), and thumbnail-only home-feed cards
  (`cardbtn` activation asserts rest opacity, direct payload, no
  panel); harness main-frame routing fix (`req.frame`).

## [0.23.3] - 2026-10-22

### Fixed

- Globo G1 "no streams found": G1 `.ghtml` articles render only
  `<bs-player videoid="...">` (no `<video>`, no `og:video`) until
  play, so no per-video button could attach pre-play. Extensions
  now treat declarative player evidence (`bs-player[videoid]`,
  `[data-video-id]`, ...) and registry "page" classification as
  sufficient proof, and add a discreet corner page button that
  sends the article URL to yt-dlp (`intent=page`). Globo joined
  the shared site registry (`videoPageKind`/`siteOf`/
  `hostMatchesScope`: page CDN `video.globo.com`/`glbimg.com`)
  and the Rust `is_video_page` parity set (`.ghtml`,
  `globoplay /v/<id>`). The microdata MP4 is evidence only, never
  an offered stream (it 400s standalone and yt-dlp's GloboArticle
  crashes claiming `*.globo.com` MP4s).
- Download button hid while playing: the 2500 ms playing-flash
  timer hid the button even with the mouse parked over the video,
  and the non-Meta `mouseover`-only path could never re-show it
  without an element-boundary crossing. Hover is now unified on
  all sites: fresh `videoRect()` coordinate test on every pointer
  event (overlay-proof), tracked pointer/focus/drag state gating
  every hide path (flash-hide included), `showBtn` repositions
  first, ResizeObserver tracks play-time resizes, and the non-Meta
  rest state is discreet yet clickable like Meta's.
- App hardening: a failed yt-dlp probe on a page-intent task now
  surfaces `yt-dlp failed: ...` instead of falling through to
  download the article HTML as a fake media file; a preserved
  sniffed stream (`x-page-stream`) still retries natively.
- Sandbox: fixtures for the Globo `<bs-player>` article,
  stationary-mouse (>2.5 s) hover with a real click, and the
  overlay-sibling variant; registry tests cover Globo kind/CDN
  scope/site (chromium + firefox parity).

## [0.23.2] - 2026-10-22

### Fixed

- Multi-site video capture: extensions previously special-cased
  Instagram only, so videos on Facebook, Threads, Twitch clips,
  TikTok, Vimeo channels and others got no button or a wrong URL.
  A shared yt-dlp site registry (`videoPageKind`/`videoIdOf`/
  `hostMatchesScope`/`siteOf` in `rank.js`, byte-identical in both
  extensions) now classifies pages as single-video vs feed for
  YouTube, Instagram, Facebook, Threads, TikTok, X, Reddit, Vimeo,
  Twitch, Dailymotion, Bilibili, NicoNico, SoundCloud, Rumble,
  Odysee and PeerTube; the per-video button sends that video's page
  URL to yt-dlp (`intent=page`), falling back to the sniffed
  manifest/direct stream correlated per site CDN (Meta→cdninstagram/
  fbcdn, YouTube→googlevideo, X→twimg, Reddit→v.redd.it, ...).
  Toolbar click on any non-feed page with a video now also offers
  the page to yt-dlp in both browsers.
- Rust `is_video_page` widened for the same shapes (Threads posts,
  Facebook share links, Twitch clips, TikTok photos, Vimeo
  channel/group URLs, Dailymotion embeds, Bilibili bangumi,
  NicoNico, Rumble, PeerTube) for paste/legacy routing parity.
- Local sandbox (`tests/sandbox/run_sandbox.py`, Playwright +
  request interception on fake hosts) exercises YouTube/Instagram/
  generic/feed/TikTok/og:video fixtures end-to-end; CI now runs
  `node extensions/rank.test.mjs`.

## [0.23.1] - 2026-10-22

### Fixed

- Universal per-video finder: the "This video" URL is now resolved
  generically from the clicked `<video>` (wrapping anchor, timestamp/
  permalink link in the video's own container, scope data attributes,
  nearest differing link), with the previous site-shaped link patterns
  as fallback. A feed video with no per-video link yields no page row
  instead of the feed/home URL (`x.com/home` was downloaded before).
  Streams still bind via src/id/timing tiers. The app additionally
  refuses to promote feed/home URLs (`/`, `/home`, `/feed`, `/explore`,
  `/search`, ...) to yt-dlp targets.
- Reverted v0.23.0 (universal capture rewrite): it sent feed/home
  pages as "this video" and broke Instagram. Instagram X/Twitter,
  Reddit and Globo behavior from 0.22.4 is restored; timing matches
  now order closest-to-play first, frame-agnostic retries never widen
  to timing, and Firefox sniffing rejects `.ts`/`.m4s` segments like
  Chromium.
- Download button more discreet (27px, translucent dark, lower rest
  opacity) and z-index raised to `2147483647` so page players no
  longer paint over it.

## [0.22.4] - 2026-10-22

### Fixed

- Cross-site capture regression from the Instagram fixes (x.com,
  reddit.com, globo.com and others): the download button is anchored in
  `document.body` again instead of the video's parent element, so site
  player wrappers (React/Lit re-renders, `transform`/`filter` containing
  blocks, `overflow:hidden` clipping, shadow-DOM customs like Globo's
  `bs-player`) can no longer hide, misplace or delete it. During
  fullscreen the button moves into the fullscreen element's subtree and
  back on exit, so the Instagram viewer keeps working; when the `<video>`
  itself is the fullscreen element the button hides until exit (children
  of `<video>` never render).
- Non-Instagram hover restored to containment checks (coordinate
  hit-testing and the wrapper-rect fallback are Instagram-scoped again);
  Chromium request sniffing rejects stream segments again so X/Reddit
  segments no longer flood the candidate pool.

## [0.22.3] - 2026-10-22

### Fixed

- Repo-tracked Firefox XPI (`extensions/webcraft-firefox.xpi`) rebuilt at
  0.22.3 so installing from the repository no longer yields the stale
  0.18.0 extension without the Instagram capture fixes; same for the
  0.22.2 fixes below. README now states the button visibility model
  (hover-revealed generally, always semi-visible on Instagram) and that
  unpacked/release extension installs never auto-update — reload the
  extension and reopen the tab after upgrading.

## [0.22.2] - 2026-10-22

### Fixed

- Instagram overlay still invisible in feed and fullscreen viewer: on
  Instagram the download button is now always semi-visible (opacity .55,
  clickable) instead of hover-only, is anchored inside the video's parent
  element so it survives the fullscreen subtree rule (body-level buttons
  are not rendered in fullscreen), re-anchors if a React re-render drops
  it, and repositions on `fullscreenchange`.

## [0.22.1] - 2026-10-22

### Fixed

- Instagram capture button invisible after 0.22.0: button now uses
  `position:fixed`, repositions on inner-container scroll (capture-phase
  listener), effective video rect falls back to sized wrapper ancestor
  (IG inserts `blob:`/MSE `<video>` at 0×0), hover hit-tests pointer
  coordinates against video rect (IG overlay/action-rail siblings never
  satisfied containment hover), and button flashes 2.5 s on playback start
  so it can be found without moving the mouse. Sniffing accepts IG-CDN
  (`cdninstagram.com`/`fbcdn.net`) media on any request type, SPA
  navigations no longer purge tab candidates, candidate TTL/cap raised
  (30 min / 200), ranking gains host-scoped IG fallback when src/postId
  tiers cannot match blob playback.

## [0.22.0] - 2026-10-22

### Added

- Built-in player chapter navigation: Prev/Next buttons + chapter dropdown
  (mpv `chapter-list`/`chapter` observed via IPC). Hidden when file has no
  chapters.
- Chatbot: persistent chat history (`chat-history.json`, restored on startup,
  New/History/Export .md/Search/Clear in Chatbot header), append-only audit
  log (`chat-audit.jsonl`), live context: local weather/news by IP geolocation
  or named place (Open-Meteo), explicit web search (`pesquise/search/buscar …`,
  EN/PT/ES, DuckDuckGo HTML, no API key) and user-supplied page capture
  (http(s) only, private hosts refused, size-capped) fed to the model as
  `[CONTEXT]` with source URLs; model stays fenced — actions proposed only,
  user confirms before execution.
- App-wide audit logging + persistent download history: daily JSONL in
  `logs/app-YYYY-MM-DD.jsonl` and `dl-history/dl-YYYY-MM-DD.jsonl` (host only,
  never full URL), shared 50 MiB budget, 90-day retention, daily rotation.
  History tab shows 7/30/90-day totals, bytes-per-day bars, 24 h speed chart
  with axes, top hosts, recent completed/failed — survives restart; chatbot
  `[APPDATA]` answers downloads/streaming usage questions from it.

### Fixed

- Browser capture on Instagram: extension now detects videos on `/reel/`,
  `/reels/`, `/p/`, `/tv/` and stories (shortcode extraction, host-scoped
  video-page check, IG-CDN `cdninstagram.com`/`fbcdn.net` media accepted on
  single-post pages, extension-less `application/octet-stream` media sniffed);
  download button reveals on delegated hover (IG overlay layers blocked
  `mouseenter`); app-side `is_video_page` accepts `/reels/`, `/tv/` and
  `/stories/<user>/<id>` so the page reaches yt-dlp instead of plain-link path.

- Player dropdown menus (Audio/Subtitle/Speed/Chapter) rendered behind video:
  native mpv child HWND composites above egui, so video frame hides while any
  popup open (playback continues) and returns on close.
- Player fullscreen had no exit: Esc now leaves fullscreen (popup owns Esc
  when menu open), Full button toggles to Window, Close button added.
- Foreign mpv OSD/OSC appearing on pause: embedded spawn now `--no-config`
  (ignores user mpv.conf/scripts, eg `--script`/`--include` vectors stripped).
  Detached mpv launch keeps user config.

## [0.21.0] - 2026-10-22

### Added

- Drives remote video streaming: double-click a video on a remote pane plays it in the built-in player without downloading or mounting anything. mpv streams the provider URL directly (`DriveBackend::stream_url` + `http-header-fields`, set at spawn and live on file switch): Google Drive `alt=media` + Bearer, OneDrive `@microsoft.graph.downloadUrl` (pre-authenticated; `/content` redirect would strip auth), Dropbox temporary link, S3 SigV4 presigned URL (1h), WebDAV Basic auth, Teldrive API key/Bearer. No rclone mount or local proxy needed. SFTP/FTP and crypt-wrapped accounts cannot stream (no HTTP URL / ciphertext bytes) and show download-first status. Track/subtitle controls from 0.20.0 apply to streams as well.

## [0.20.2] - 2026-10-22

### Fixed

- Drives Server-side copy cross-account `404 File not found`: Drive runs `files.copy` as the destination account, so files that account cannot already see (different account, not shared) return 404, not 403. Error now explains this and names the fix: share the file, parent folder, or Shared Drive with the destination account, then retry; normal Copy (download+upload) remains the cross-account path without sharing. Same-account copies unaffected.

## [0.20.1] - 2026-10-22

### Changed

- Drives server-side copy is now an explicit **Server-side copy** button in the pane action bar, shown only when both panes are Google Drive accounts (crypt off). It runs Drive `files.copy` under the destination account — no data through the local machine, no staging fallback (failures are job errors). Normal Copy/Move keep the staged download+upload path. S3 -> S3 keeps silent automatic `CopyObject` with staged fallback, unchanged.

## [0.20.0] - 2026-10-21

### Added

- Drives: double-click a local video (This PC pane) opens the built-in player popup; remote panes show a download-first hint.
- Player popup: Audio and Subtitle track selectors (with Off), Load sub file, and Sub style row (size, position, text/shadow color, bold, force-on-ASS) applied live via mpv IPC. Style persists in Settings and is editable in Options > mpv > Subtitle style; launch flags emitted only when non-default.

### Changed

- Chatbot `add_download` on URL+add-intent is detected deterministically and routed to the Confirm UI instead of relying on the model.

## [0.19.0] - 2026-10-21

### Added

- Chatbot app actions: `add_download` (file, magnet, `.torrent`, NZB, video page), `pause_task` / `resume_task` / `delete_task` / `open_folder` by name, `start_all` / `stop_all` / `delete_completed`, and `set_setting` (whitelisted keys only). `list_tasks` / `search_tasks` / `get_stats` run instantly without confirmation; mutating actions need Confirm. App state is injected as `[APPDATA]` context, and unsupported actions are reported instead of silently dropped.

### Changed

- Settings "Max part MB" renamed "Part size MB (0 = auto)" and now means an exact target: 0 = auto tiers that scale with file size; a set value sizes parts exactly (6 GiB at 512 MB = 12 parts). Resumed downloads keep their saved segmentation.
- File size is highlighted (strong accent) in the progress and completed windows, with downloaded/total/percent. Completed downloads with unknown total (HLS/DASH) show final downloaded bytes instead.
- TorBox: non-cached hoster links create a web download server-side; the app polls until ready, then downloads. Server errors are surfaced.

### Fixed

- Debrid alias domains (`mega.nz`/`mega.co.nz`/`mega.io`, `rapidgator.net`/`rg.to`/`rapidgator.asia`) route to a capable Real-Debrid/AllDebrid/TorBox provider instead of direct HTTP.
- Provider errors during unrestrict (e.g. `infringing_file`) now surface as task errors; the hoster HTML page is no longer downloaded as a junk file.

## [0.18.0] - 2026-10-21

### Added

- Per-download debrid selection: Add URL/File dialog and browser capture dialog show a Debrid section (Auto default, Local/no-debrid, or force Real-Debrid/AllDebrid/TorBox) for hoster URLs, magnets and .torrent files. Choice persists on task and honored at download start; defaults follow Options provider settings.
- .torrent downloads can route through debrid: infohash probed and resolved as magnet at download start, local torrent fallback on failure.
- Chatbot assistant (bring-your-own LLM) with web widgets; docs/privacy-policy.md for extension store listing.

### Changed

- "Add URL" renamed "Add URL/File" where dialog accepts files.
- Firefox extension published to addons.mozilla.org (automatic updates); manifest requires Firefox 140+ and declares data collection (browsing activity + website content sent to local native host only).

### Fixed

- Debrid hoster routing uses domain similarity (>85%) with exact/alias matches first: near-miss domains (mega.nz vs mega.co.nz) now resolve through a capable provider instead of first-enabled provider failing.

## [0.17.0] - 2026-10-21

### Changed

- UI overhaul: shared component module (`src/ui.rs`) for buttons (primary / secondary / ghost / destructive, 28/32/36 pt heights), badges (neutral / success / warning / error / info), form rows and dialogs. Spacing on a 4/8 pt scale set once in the theme; type scale via text styles only.
- Dialogs use shared width tiers (400 / 560 / 760 / 960) clamped to the screen. In-window dialogs are centered modals that dim and block the window behind them, close with Esc or the close button, and keep the footer pinned with one primary action on the right.
- Window behavior: the close button quits webcraft; minimizing hides it to the tray. Options "Close to tray" is now "Minimize to tray" (existing setting carried over).
- README: Interface section with a screenshot tour (`images/`).

### Fixed

- Drives: deleting remote files and removing an account now ask for confirmation first. Run sync is styled as destructive when the plan deletes files.
- Disabled buttons are visibly dimmed; text fields match button height; badges no longer stretch to row height; delete-confirmation footer no longer shifts when "also delete files" is toggled.
- Extension manifest versions back in lockstep with the app version.

## [0.16.0] - 2026-10-20

### Fixed

- Segmented downloads: per-download connection pool, parts sized relative to file (`total/(pool×4)`, clamped [4 MB, 64 MB]) instead of infinitesimal splits; 429 responses teach a learned-max gate (coalesces queued parts, drops permits before Retry-After sleep, caps throttle waits). Resume checkpoints byte progress atomically every 1 MB, so Stop/Play no longer restarts from zero.
- Progress window shows "Reconstructing file" during part assembly (and "Finalizing" during validate/checksum) instead of appearing hung at 100%; status chip now shows engine phase for all downloads, not only yt-dlp.
- Debrid (Rapidgator etc.): badge and provider info appear as soon as link resolves; progress window Links section shows original + debrided + final URLs live and after completion, with **Copy link** for both. Resolved debrided URL is persisted on the task (bearer token, per user choice) and cleared on redownload.

## [0.15.0] - 2026-10-20

### Added

- Drives tab: dual-pane cloud storage (next to Streams / Site Grabber) - Google Drive, OneDrive, Dropbox (OAuth, own client_id), WebDAV, S3-compatible, SFTP, FTP/FTPS, Telegram via self-hosted teldrive, local folders. Browse/upload/download/copy/move with pause/resume/cancel, multithreaded ranged/session transfers where supported, sync (compare preview, copy/update/mirror, saved jobs), per-account rclone-crypt encryption.
- Drives account setup wizard (rclone config order) from zero accounts: Provider > Details (per-provider console steps, fixed redirect http://127.0.0.1:53682/) > Connect/Test in draft (browser auto-capture, paste-code fallback, cancel) > Root pick after sign-in (Google Team Drive separate from optional start folder; Shared Drives auto-list) > Review final test > Finish saves once and opens selected account in a pane.
- Drives Accounts: Edit anytime with Test connection, Duplicate (clone credentials to another Team Drive/root), and Import rclone.conf paste (obscured passwords revealed; token/team_drive mapped).
- Drives transfers panel: multi-file concurrency via per-account Parallel streams (concurrent files + segments per file), S3 multipart parallel parts, job summaries (source -> destination), (i) info (bytes/%/speed/ETA/elapsed), per-job Remove (deletes destination data for stopped jobs), red full error text. Shift-range and Ctrl/Cmd multi-select; Refresh and Up on both panes.

### Fixed

- Windows browser open no longer truncates OAuth URL at & (Google Missing required parameter: client_id 400).
- Wizard/accounts/sync windows fixed max 580/620x700 (content no longer pushes window big).
- Destination folders created once per job after existence check (Google Drive create-folder was duplicating per file/segment).
- Upload progress counts destination-upload bytes only (local staging copy no longer jumps to 100%).
- Upload cancel now interrupts in-flight upload; partial destinations removed on error.

## [0.14.0] - 2026-10-20

### Added


- Per-download proxy override (Add URL *Transfer → Proxy*, browser Capture, Progress dialog; Progress edits apply on restart). Empty inherits global Settings proxy. Drives reqwest downloads, probing (incl. yt-dlp `--proxy`), torrent/NZB/debrid fetches; SOCKS5 proxies now work (reqwest `socks` feature). FTP/SFTP, NNTP articles, torrent swarm stay direct.
- NZB post-download unpack: archive extraction with NZB-embedded passwords (`<meta type="password">`, `{{pw}}` in NZB/archive names, per-archive override); `{{pw}}` secrets stripped from output names.

### Changed

- Main window opens 1440×900 (was 1180×720); Capture/Add/Progress/Collision/Player dialogs larger; browser Capture/Add reveal grows a smaller restored main window back to full size.
- Extension candidates: playlist URLs (.m3u8/.mpd) retained 30 min and evicted last at cap; 206 partial responses count as candidates.

### Fixed

- Release pipeline: all clippy warnings fixed under `-D warnings` (incl. platform-only Linux/macOS warnings), Rust toolchain pinned in CI — tag builds publish again (v0.13.0 tag built but failed clippy, so no Release was created).
- Downloads table: persisted column widths shrink to fit instead of overflowing; side rail scrolls; NNTP `{{pw}}` percent-encoded names decode correctly.

## [0.13.0] - 2026-10-19

### Added

- Streams app function: Streams tab plays direct video links in embedded mpv player inside app window (native child surface + IPC controls: play/pause, seek, volume, speed, mute).
- Options mpv tab: mpv path, fullscreen, keep-open, loop, volume, speed, start position, hwdec, cache, subtitle auto-load, extra flags.
- Duplicate filename prompt on Add/Capture: Overwrite / Rename name_N.ext / Cancel; batch, grabber, clipboard adds auto-rename.

### Changed

- Streams removed Downloads type filter (now dedicated tab).
- New URL / capture modals always open in front main window keyboard focus in dialog.
- yt-dlp warnings in dialogs compacted: short summary + collapsed Details instead raw traceback block.

### Fixed

- yt-dlp downloads saved x.mp4 (host-named) on X/Twitter etc: extracted video title now used filename.
- Embedded mpv: no-audio/no-controls caused mpv rendering over app GL surface; forced volume/mute settings, mpv log file, audio-track state shown in player.

## [0.12.1] - 2026-10-17

### Added

- Video download button top-left; app-styled options panel.

[0.14.0]: https://github.com/alfablac/webcraft/releases/tag/v0.14.0
[0.13.0]: https://github.com/alfablac/webcraft/releases/tag/v0.13.0
