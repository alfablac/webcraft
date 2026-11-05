//! Drives tab UI (bin crate): dual-pane account browser, transfer jobs,
//! account editor, sync. Sizes follow app conventions: editor 580x700,
//! sync 620x840, panes split 50/50, job panel 168px.
use crate::icons::{icon, Icon};
use crate::theme;
use crate::ui;
use crate::Runtime;
use downloadcraft_lib::drive::jobs::JobKind;
use downloadcraft_lib::drive::sync::{plan as sync_plan, SyncMode, SyncPlan};
use downloadcraft_lib::drive::{backend_for, jobs, DriveBackend, Progress, RemoteEntry};
use downloadcraft_lib::model::{DriveAccount, DriveKind, DriveSync, Settings};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const LOCAL_ID: &str = "__local";
/// Stable width for Pause/Resume toggles (label swap never shifts the row).
const PAUSE_W: f32 = 72.0;

fn lock<T>(m: &Arc<Mutex<T>>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

type StreamSlot = Arc<Mutex<Option<Result<(String, Vec<(String, String)>), String>>>>;
type ListSlot = Arc<Mutex<Option<(u64, Result<Vec<RemoteEntry>, String>)>>>;
type ActSlot = Arc<Mutex<Option<Result<String, String>>>>;
type SyncSlot = Arc<Mutex<Option<Result<(Vec<RemoteEntry>, Vec<RemoteEntry>), String>>>>;
type DrivesSlot = Arc<Mutex<Option<(String, Result<Vec<(String, String)>, String>)>>>;

#[derive(Default)]
pub struct Pane {
    pub account_id: String,
    pub path: String,
    pub entries: Vec<RemoteEntry>,
    pub selected: HashSet<String>,
    /// Last clicked row index (Shift range anchor).
    pub anchor: Option<usize>,
    pub loading: bool,
    pub error: String,
    pub seq: u64,
    pub slot: Option<ListSlot>,
    pub path_edit: String,
    pub new_name: String,
    pub rename_name: String,
}

impl Pane {
    fn new() -> Self {
        Self {
            account_id: LOCAL_ID.to_string(),
            ..Default::default()
        }
    }
}

/// rclone-config style wizard state. Draft tokens live RAM only until
/// Finish (single settings write) — Cancel discards, nothing orphaned.
pub struct WizardState {
    pub step: u8, // 0 provider, 1 details, 2 connect, 3 root, 4 review
    pub draft: DriveAccount,
    pub name_touched: bool,
    pub verified: bool,
    pub status: String,
    pub oauth_busy: bool,
    pub auth_url: String,
    pub code_input: String,
    pub pending: Option<Arc<downloadcraft_lib::drive::oauth::PendingAuth>>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub token_slot: Arc<Mutex<Option<Result<downloadcraft_lib::drive::oauth::TokenOut, String>>>>,
    pub test_slot: ActSlot,
    pub test_busy: bool,
    pub drives_slot: DrivesSlot,
    pub shared_list: Vec<(String, String)>,
    pub root_loading: bool,
    pub final_started: bool,
    pub final_ok: bool,
    pub final_slot: ActSlot,
    pub use_team: bool,
}

impl WizardState {
    fn new() -> Self {
        Self {
            step: 0,
            draft: DriveAccount {
                id: downloadcraft_lib::model::new_id(),
                name: "Google Drive".into(),
                kind: DriveKind::GoogleDrive,
                streams: 4,
                ..Default::default()
            },
            name_touched: false,
            verified: false,
            status: "Pick provider. Steps follow rclone config: type > credentials > sign-in > Shared Drive > keep.".into(),
            oauth_busy: false,
            auth_url: String::new(),
            code_input: String::new(),
            pending: None,
            cancel: None,
            token_slot: Arc::new(Mutex::new(None)),
            test_slot: Arc::new(Mutex::new(None)),
            test_busy: false,
            drives_slot: Arc::new(Mutex::new(None)),
            shared_list: Vec::new(),
            root_loading: false,
            final_started: false,
            final_ok: false,
            final_slot: Arc::new(Mutex::new(None)),
            use_team: false,
        }
    }
    fn is_oauth(&self) -> bool {
        matches!(
            self.draft.kind,
            DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox
        )
    }
    fn invalidate(&mut self) {
        self.verified = false;
        self.draft.access_token.clear();
        self.draft.refresh_token.clear();
        self.shared_list.clear();
        self.final_started = false;
        self.final_ok = false;
    }
    fn start_test(&mut self, full: bool) {
        let mut acc = self.draft.clone();
        if !full && !matches!(acc.kind, DriveKind::S3) {
            acc.root.clear();
        }
        self.test_busy = true;
        self.status = "Testing…".into();
        let slot = self.test_slot.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let res = match rt {
                Ok(rt) => rt.block_on(downloadcraft_lib::drive::test_account(&acc)),
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some(res);
        });
    }
    fn start_final_test(&mut self) {
        let acc = self.draft.clone();
        let slot = self.final_slot.clone();
        self.status = "Final test at picked root…".into();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let res = match rt {
                Ok(rt) => rt.block_on(downloadcraft_lib::drive::test_account(&acc)),
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some(res);
        });
    }
    fn fetch_drives(&mut self) {
        let acc = self.draft.clone();
        let slot = self.drives_slot.clone();
        let id = acc.id.clone();
        self.root_loading = true;
        self.status = "Fetching Shared Drive list…".into();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let res = match rt {
                Ok(rt) => rt.block_on(downloadcraft_lib::drive::shared_drives(&acc)),
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some((id, res));
        });
    }
    fn start_oauth(&mut self) {
        let Some(app) = downloadcraft_lib::drive::oauth::preset(self.draft.kind, &self.draft)
        else {
            self.status = "Not an OAuth provider.".into();
            return;
        };
        if app.client_id.trim().is_empty() {
            self.status = "Client ID empty. Back to Details — paste it (Setup steps).".into();
            return;
        }
        let pending = match downloadcraft_lib::drive::oauth::start(&app) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                self.status = e;
                return;
            }
        };
        self.auth_url = pending.auth_url.clone();
        self.code_input.clear();
        self.pending = Some(pending.clone());
        self.oauth_busy = true;
        let slot = self.token_slot.clone();
        *lock(&slot) = None;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        std::thread::spawn(move || {
            let res = downloadcraft_lib::drive::oauth::wait_loopback(&pending, cancel, true);
            *lock(&slot) = Some(res);
        });
        self.status = "Browser opened. Sign in. Paste fallback if stuck.".into();
    }
    fn paste_finish(&mut self) {
        let Some(pending) = self.pending.clone() else {
            self.status = "No sign-in waiting. Click Connect first.".into();
            return;
        };
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
        let pasted = self.code_input.clone();
        let slot = self.token_slot.clone();
        self.status = "Exchanging pasted code…".into();
        std::thread::spawn(move || {
            let res = downloadcraft_lib::drive::oauth::finish(&pending, &pasted);
            *lock(&slot) = Some(res);
        });
    }
}

pub struct JobUi {
    #[allow(dead_code)]
    pub id: String,
    /// Short summary: "Copy file.mp4 → MyDrive:dir" / full source→dest list.
    pub label: String,
    pub prog: Progress,
    pub status: Arc<Mutex<String>>,
    pub done_flag: Arc<AtomicBool>,
    pub last_done: u64,
    pub last_at: std::time::Instant,
    pub speed: f64,
    pub started: std::time::Instant,
    pub show_info: bool,
    /// Destination account id + dir, for Remove with files.
    pub dst_account: String,
    pub dst_dir: String,
    /// Names written under dst_dir (completed or partial).
    pub dest_names: Vec<String>,
    /// Was job direction download to local pane (dest_names via dst backend).
    pub is_transfer: bool,
}

pub struct DrivesUi {
    pub panes: [Pane; 2],
    pub focus: usize,
    pub jobs: Vec<JobUi>,
    pub status: String,
    pub overwrite: bool,
    pub wizard: Option<WizardState>,
    pub show_import: bool,
    pub import_text: String,
    pub show_accounts: bool,
    pub edit: Option<DriveAccount>,
    pub edit_is_new: bool,
    pub oauth_slot: ActSlot,
    pub oauth_busy: bool,
    pub oauth_for: String,
    pub oauth_auth_url: String,
    pub oauth_code_input: String,
    pub oauth_cancel: Option<Arc<AtomicBool>>,
    pub oauth_paste_request: bool,
    pub pending_auth: Option<Arc<downloadcraft_lib::drive::oauth::PendingAuth>>,
    pub test_slot: ActSlot,
    pub drives_slot: DrivesSlot,
    pub shared_for: String,
    pub shared_list: Vec<(String, String)>,
    pub show_sync: bool,
    pub sync_src_account: String,
    pub sync_src_path: String,
    pub sync_dst_account: String,
    pub sync_dst_path: String,
    pub sync_mode: String,
    pub sync_name: String,
    pub sync_preview: Option<SyncPlan>,
    pub sync_slot: Option<SyncSlot>,
    pub pending_download_tasks: Vec<(String, String, String)>, // url,user,pass
    /// Play requests awaiting built-in player (drained by app):
    /// (url-or-local-path, http headers for remote streams).
    pub pending_play: Vec<(String, Vec<(String, String)>)>,
    /// In-flight stream URL resolution (double-click remote video).
    pub stream_slot: Option<StreamSlot>,
    /// Pending destructive confirmation (remote delete / account removal).
    pub confirm: Option<DriveConfirm>,
}

/// Destructive drive actions routed through a confirmation modal.
pub enum DriveConfirm {
    /// Delete selected entries of pane idx.
    Files(usize),
    /// Remove account (id, display name).
    Account(String, String),
}

impl Default for DrivesUi {
    fn default() -> Self {
        Self {
            panes: [Pane::new(), Pane::new()],
            focus: 1,
            jobs: Vec::new(),
            status: "Add an account, then browse. Local pane uses download folder.".into(),
            overwrite: false,
            wizard: None,
            show_import: false,
            import_text: String::new(),
            show_accounts: false,
            edit: None,
            edit_is_new: false,
            oauth_slot: Arc::new(Mutex::new(None)),
            oauth_busy: false,
            oauth_for: String::new(),
            oauth_auth_url: String::new(),
            oauth_code_input: String::new(),
            oauth_cancel: None,
            oauth_paste_request: false,
            pending_auth: None,
            test_slot: Arc::new(Mutex::new(None)),
            drives_slot: Arc::new(Mutex::new(None)),
            shared_for: String::new(),
            shared_list: Vec::new(),
            show_sync: false,
            sync_src_account: String::new(),
            sync_src_path: String::new(),
            sync_dst_account: String::new(),
            sync_dst_path: String::new(),
            sync_mode: "update".into(),
            sync_name: String::new(),
            sync_preview: None,
            sync_slot: None,
            pending_download_tasks: Vec::new(),
            pending_play: Vec::new(),
            stream_slot: None,
            confirm: None,
        }
    }
}

pub fn local_account(settings: &Settings) -> DriveAccount {
    DriveAccount {
        id: LOCAL_ID.into(),
        name: "This PC".into(),
        kind: DriveKind::Local,
        endpoint: settings.download_dir.to_string_lossy().into_owned(),
        root: settings.download_dir.to_string_lossy().into_owned(),
        streams: 4,
        ..Default::default()
    }
}

pub fn account_by_id(settings: &Settings, id: &str) -> Option<DriveAccount> {
    if id == LOCAL_ID {
        return Some(local_account(settings));
    }
    settings.drive_accounts.iter().find(|a| a.id == id).cloned()
}

fn fmt_time(secs: u64) -> String {
    if secs == 0 {
        return "—".into();
    }
    // UTC date YYYY-MM-DD HH:MM via civil math.
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
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
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

impl DrivesUi {
    pub fn backend(
        settings: &Settings,
        id: &str,
    ) -> Result<(DriveAccount, Arc<dyn DriveBackend>), String> {
        let acc = account_by_id(settings, id).ok_or("account not found")?;
        let be = backend_for(&acc)?;
        Ok((acc, be))
    }

    fn poll_pane(&mut self, idx: usize, settings: &Settings) {
        let done = self.panes[idx].slot.as_ref().and_then(|s| lock(s).take());
        if let Some((seq, res)) = done {
            if seq == self.panes[idx].seq {
                self.panes[idx].loading = false;
                match res {
                    Ok(v) => {
                        self.panes[idx].entries = v;
                        self.panes[idx].error.clear();
                        self.panes[idx].selected.clear();
                    }
                    Err(e) => self.panes[idx].error = e,
                }
                // about (best-effort, cached once)
                let _ = settings;
            }
        }
    }

    pub fn refresh(&mut self, rt: &Arc<tokio::runtime::Runtime>, settings: &Settings, idx: usize) {
        let pane = &mut self.panes[idx];
        pane.seq += 1;
        let seq = pane.seq;
        let slot: ListSlot = Arc::new(Mutex::new(None));
        pane.slot = Some(slot.clone());
        pane.loading = true;
        pane.error.clear();
        let path = pane.path.clone();
        let acc_id = pane.account_id.clone();
        let settings2 = settings.clone();
        rt.spawn(async move {
            let res = async {
                let (_, be) = Self::backend(&settings2, &acc_id)?;
                be.list(&path).await
            }
            .await;
            *lock(&slot) = Some((seq, res));
        });
    }

    fn ensure_loaded(&mut self, rt: &Arc<tokio::runtime::Runtime>, settings: &Settings) {
        for i in 0..2 {
            if self.panes[i].slot.is_none()
                && self.panes[i].entries.is_empty()
                && !self.panes[i].loading
            {
                self.refresh(rt, settings, i);
            }
        }
    }

    fn spawn_job(
        &mut self,
        rt: &Arc<tokio::runtime::Runtime>,
        label: String,
        f: impl std::future::Future<Output = Result<String, String>> + Send + 'static,
        prog: Progress,
    ) {
        let status = Arc::new(Mutex::new("Running".to_string()));
        let done_flag = Arc::new(AtomicBool::new(false));
        let st2 = status.clone();
        let df2 = done_flag.clone();
        self.jobs.push(JobUi {
            id: downloadcraft_lib::model::new_id(),
            label,
            prog,
            status,
            done_flag,
            last_done: 0,
            last_at: std::time::Instant::now(),
            speed: 0.0,
            started: std::time::Instant::now(),
            show_info: false,
            dst_account: String::new(),
            dst_dir: String::new(),
            dest_names: Vec::new(),
            is_transfer: false,
        });
        rt.spawn(async move {
            let r = f.await;
            *lock(&st2) = match r {
                Ok(m) => format!("Done · {m}"),
                Err(e) if e == "cancelled" => "Cancelled".into(),
                Err(e) => format!("Error · {e}"),
            };
            df2.store(true, Ordering::Relaxed);
        });
        self.status = "Job queued in Downloads > Drives panel (bottom).".into();
    }

    /// Explicit server-side copy pane `from` -> other pane current dir.
    /// Google Drive pairs only (button gated); strict files.copy, no
    /// staging fallback: failures surface as job errors.
    fn server_side_copy_selected(
        &mut self,
        rt: &Arc<tokio::runtime::Runtime>,
        settings: &Settings,
        from: usize,
    ) {
        let to = 1 - from;
        let src_id = self.panes[from].account_id.clone();
        let dst_id = self.panes[to].account_id.clone();
        // Defense in depth: same gate as button visibility.
        let pair_ok = [&src_id, &dst_id].iter().all(|id| {
            account_by_id(settings, id)
                .map(|a| {
                    a.kind == downloadcraft_lib::model::DriveKind::GoogleDrive
                        && a.crypt_password.is_empty()
                })
                .unwrap_or(false)
        });
        let same_place = src_id == dst_id
            && self.panes[from].path.trim_matches('/') == self.panes[to].path.trim_matches('/');
        if !pair_ok {
            self.status = "Server-side copy needs Google Drive on both panes (crypt off).".into();
            return;
        }
        if same_place {
            self.status = "Server-side copy: destination is the source folder.".into();
            return;
        }
        let dst_dir = self.panes[to].path.clone();
        let entries: Vec<RemoteEntry> = self.panes[from]
            .entries
            .iter()
            .filter(|e| self.panes[from].selected.contains(&e.id))
            .cloned()
            .collect();
        if entries.is_empty() {
            self.status = "Select file(s) first (click rows).".into();
            return;
        }
        let settings2 = settings.clone();
        let prog = Progress::new();
        let prog2 = prog.clone();
        let overwrite = self.overwrite;
        let dst_label = account_by_id(settings, &dst_id)
            .map(|a| a.display())
            .unwrap_or(dst_id.clone());
        let first = entries.first().map(|e| e.name.clone()).unwrap_or_default();
        let label = if entries.len() == 1 {
            format!(
                "Server copy {first} → {dst_label}:{}",
                if dst_dir.is_empty() { "/" } else { &dst_dir }
            )
        } else {
            format!(
                "Server copy {} items (first {first}) → {dst_label}:{}",
                entries.len(),
                if dst_dir.is_empty() { "/" } else { &dst_dir }
            )
        };
        let dst_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let dst_id2 = dst_id.clone();
        let dst_dir2 = dst_dir.clone();
        self.spawn_job(
            rt,
            label,
            async move {
                let (_, src_be) = Self::backend(&settings2, &src_id)?;
                let (_, dst_be) = Self::backend(&settings2, &dst_id)?;
                if !overwrite {
                    let existing: HashSet<String> = dst_be
                        .list(&dst_dir)
                        .await
                        .map_err(|e| format!("list destination before copy: {e}"))?
                        .into_iter()
                        .map(|e| e.name)
                        .collect();
                    let mut kept = Vec::new();
                    let mut skipped = 0usize;
                    for e in &entries {
                        if existing.contains(&e.name) {
                            skipped += 1;
                        } else {
                            kept.push(e.clone());
                        }
                    }
                    if kept.is_empty() {
                        return Ok("all skipped (exist, Overwrite off)".to_string());
                    }
                    let r =
                        jobs::run_server_side_copy(&src_be, &kept, &dst_be, &dst_dir, &prog2).await;
                    return r.map(|m| format!("{m}; {skipped} skipped (exist)"));
                }
                jobs::run_server_side_copy(&src_be, &entries, &dst_be, &dst_dir, &prog2).await
            },
            prog,
        );
        if let Some(j) = self.jobs.last_mut() {
            j.dst_account = dst_id2;
            j.dst_dir = dst_dir2;
            j.dest_names = dst_names;
            j.is_transfer = true;
        }
    }

    /// Copy/move selected entries pane `from` -> other pane current dir.
    fn transfer_selected(
        &mut self,
        rt: &Arc<tokio::runtime::Runtime>,
        settings: &Settings,
        from: usize,
        kind: JobKind,
    ) {
        let to = 1 - from;
        let src_id = self.panes[from].account_id.clone();
        let dst_id = self.panes[to].account_id.clone();
        let dst_dir = self.panes[to].path.clone();
        let entries: Vec<RemoteEntry> = self.panes[from]
            .entries
            .iter()
            .filter(|e| self.panes[from].selected.contains(&e.id))
            .cloned()
            .collect();
        if entries.is_empty() {
            self.status = "Select file(s) first (click rows).".into();
            return;
        }
        let settings2 = settings.clone();
        let prog = Progress::new();
        let prog2 = prog.clone();
        let overwrite = self.overwrite;
        let src_label = account_by_id(settings, &src_id)
            .map(|a| a.display())
            .unwrap_or(src_id.clone());
        let dst_label = account_by_id(settings, &dst_id)
            .map(|a| a.display())
            .unwrap_or(dst_id.clone());
        let first = entries.first().map(|e| e.name.clone()).unwrap_or_default();
        let verb = match kind {
            JobKind::Move => "Move",
            _ => "Copy",
        };
        let label = if entries.len() == 1 {
            format!(
                "{verb} {first} → {dst_label}:{}",
                if dst_dir.is_empty() { "/" } else { &dst_dir }
            )
        } else {
            format!(
                "{verb} {} items (first {first}) {} → {dst_label}:{}",
                entries.len(),
                src_label,
                if dst_dir.is_empty() { "/" } else { &dst_dir }
            )
        };
        // Per-account "Parallel streams" = both: segments per file (streams)
        // and concurrent files per job (conc). One knob, no duplicate setting.
        let streams = account_by_id(settings, &src_id)
            .map(|a| a.streams.max(1))
            .unwrap_or(4);
        let conc = streams as usize;
        let dst_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let dst_id2 = dst_id.clone();
        let dst_dir2 = dst_dir.clone();
        self.spawn_job(
            rt,
            label,
            async move {
                let (_, src_be) = Self::backend(&settings2, &src_id)?;
                let (_, dst_be) = Self::backend(&settings2, &dst_id)?;
                if !overwrite {
                    let existing: HashSet<String> = dst_be
                        .list(&dst_dir)
                        .await
                        .map_err(|e| format!("list destination before copy: {e}"))?
                        .into_iter()
                        .map(|e| e.name)
                        .collect();
                    let mut kept = Vec::new();
                    let mut skipped = 0usize;
                    for e in &entries {
                        if existing.contains(&e.name) {
                            skipped += 1;
                        } else {
                            kept.push(e.clone());
                        }
                    }
                    if kept.is_empty() {
                        return Ok("all skipped (exist, Overwrite off)".to_string());
                    }
                    let r = jobs::run_job(
                        kind, &src_be, &kept, &dst_be, &dst_dir, None, streams, conc, &prog2,
                    )
                    .await;
                    return r.map(|m| format!("{m}; {skipped} skipped (exist)"));
                }
                jobs::run_job(
                    kind, &src_be, &entries, &dst_be, &dst_dir, None, streams, conc, &prog2,
                )
                .await
            },
            prog,
        );
        if let Some(j) = self.jobs.last_mut() {
            j.dst_account = dst_id2;
            j.dst_dir = dst_dir2;
            j.dest_names = dst_names;
            j.is_transfer = true;
        }
    }

    fn delete_selected(
        &mut self,
        rt: &Arc<tokio::runtime::Runtime>,
        settings: &Settings,
        pane_idx: usize,
    ) {
        let acc_id = self.panes[pane_idx].account_id.clone();
        let entries: Vec<RemoteEntry> = self.panes[pane_idx]
            .entries
            .iter()
            .filter(|e| self.panes[pane_idx].selected.contains(&e.id))
            .cloned()
            .collect();
        if entries.is_empty() {
            return;
        }
        let settings2 = settings.clone();
        let prog = Progress::new();
        let prog2 = prog.clone();
        self.spawn_job(
            rt,
            format!("Delete {} item(s)", entries.len()),
            async move {
                let (_, be) = Self::backend(&settings2, &acc_id)?;
                for e in &entries {
                    prog2.check()?;
                    be.delete(e).await?;
                }
                Ok(format!("deleted {}", entries.len()))
            },
            prog,
        );
    }

    fn simple_op(
        &mut self,
        rt: &Arc<tokio::runtime::Runtime>,
        settings: &Settings,
        acc_id: String,
        label: String,
        f: impl FnOnce(
                Arc<dyn DriveBackend>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
            > + Send
            + 'static,
    ) {
        let settings2 = settings.clone();
        let prog = Progress::new();
        self.spawn_job(
            rt,
            label,
            async move {
                let (_, be) = Self::backend(&settings2, &acc_id)?;
                f(be).await
            },
            prog,
        );
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        app_rt: &Arc<Mutex<Runtime>>,
        tokio_rt: &Arc<tokio::runtime::Runtime>,
    ) {
        let settings: Settings = crate::lock_unpoison(app_rt).state.settings.clone();
        for p in &mut self.panes {
            if p.account_id == LOCAL_ID && p.path.is_empty() {
                p.path = settings.download_dir.to_string_lossy().into_owned();
            }
        }
        self.ensure_loaded(tokio_rt, &settings);
        for i in 0..2 {
            self.poll_pane(i, &settings);
        }
        // Stream URL resolved -> queue play; failure -> status (SFTP/
        // FTP/crypt say download first via backend Err text).
        if let Some(slot) = &self.stream_slot {
            let done = crate::lock_unpoison(slot).take();
            if let Some(r) = done {
                self.stream_slot = None;
                match r {
                    Ok((url, headers)) => {
                        self.status = "Streaming in built-in player".into();
                        self.pending_play.push((url, headers));
                    }
                    Err(e) => self.status = format!("Stream failed: {e}"),
                }
            }
        }
        self.poll_oauth(app_rt);
        ui.add_space(theme::SP_SM);
        ui.horizontal(|ui| {
            ui.heading("Drives");
            ui.add_space(theme::SP_SM);
            if ui::secondary_button(ui, "Accounts").clicked() {
                self.show_accounts = true;
            }
            if ui::secondary_button(ui, "Sync").clicked() {
                self.show_sync = true;
            }
            ui.checkbox(&mut self.overwrite, "Overwrite");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("Google/OneDrive/Dropbox need your own OAuth client_id (Accounts). Teldrive = your server.")
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
            });
        });
        if !self.status.is_empty() {
            ui.label(
                egui::RichText::new(self.status.clone())
                    .color(theme::TEXT_MUTED)
                    .small(),
            );
        }
        if settings.drive_accounts.is_empty() {
            ui.add_space(theme::SP_XS);
            theme::panel().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("No accounts yet.")
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Wizard: provider > credentials > sign-in > root (Shared Drive).",
                        )
                        .color(theme::TEXT_MUTED)
                        .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui::primary_button(ui, "Add account").clicked() {
                            self.wizard = Some(WizardState::new());
                        }
                    });
                });
            });
        }
        ui.add_space(theme::SP_SM);

        // Two panes 50/50: use columns via available width halves.
        let total_h = ui.available_height();
        let job_h = if self.jobs.is_empty() { 28.0 } else { 168.0 };
        let pane_h = (total_h - job_h - 10.0).max(240.0);
        let mut actions: Vec<PaneAction> = Vec::new();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), pane_h),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                let w = (ui.available_width() - 8.0) / 2.0;
                for idx in 0..2 {
                    ui.allocate_ui_with_layout(
                        egui::vec2(w, pane_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            self.pane_ui(ui, idx, &settings, &mut actions);
                        },
                    );
                    ui.add_space(theme::SP_SM);
                }
            },
        );
        for a in actions {
            match a {
                PaneAction::Refresh(i) => self.refresh(tokio_rt, &settings, i),
                PaneAction::Navigate(i, path) => {
                    self.panes[i].path = path;
                    self.panes[i].path_edit.clear();
                    self.refresh(tokio_rt, &settings, i);
                }
                PaneAction::AccountChanged(i, id) => {
                    self.panes[i].account_id = id;
                    self.panes[i].path = if self.panes[i].account_id == LOCAL_ID {
                        settings.download_dir.to_string_lossy().into_owned()
                    } else {
                        String::new()
                    };
                    self.refresh(tokio_rt, &settings, i);
                }
                PaneAction::Copy(i) => {
                    self.transfer_selected(tokio_rt, &settings, i, JobKind::Copy)
                }
                PaneAction::Move(i) => {
                    self.transfer_selected(tokio_rt, &settings, i, JobKind::Move)
                }
                PaneAction::ServerCopy(i) => self.server_side_copy_selected(tokio_rt, &settings, i),
                PaneAction::Delete(i) => self.confirm = Some(DriveConfirm::Files(i)),
                PaneAction::Mkdir(i) => {
                    let name = self.panes[i].new_name.trim().to_string();
                    if !name.is_empty() {
                        let parent = self.panes[i].path.clone();
                        let acc = self.panes[i].account_id.clone();
                        self.panes[i].new_name.clear();
                        self.simple_op(
                            tokio_rt,
                            &settings,
                            acc,
                            format!("New folder {name}"),
                            move |be| {
                                Box::pin(async move {
                                    be.mkdir(&downloadcraft_lib::drive::join_path(&parent, &name))
                                        .await?;
                                    Ok("folder created (Refresh)".into())
                                })
                            },
                        );
                    }
                }
                PaneAction::Rename(i) => {
                    let name = self.panes[i].rename_name.trim().to_string();
                    let entry = self.panes[i]
                        .entries
                        .iter()
                        .find(|e| self.panes[i].selected.contains(&e.id))
                        .cloned();
                    if let (false, Some(e)) = (name.is_empty(), entry) {
                        let acc = self.panes[i].account_id.clone();
                        self.panes[i].rename_name.clear();
                        self.simple_op(
                            tokio_rt,
                            &settings,
                            acc,
                            format!("Rename -> {name}"),
                            move |be| {
                                Box::pin(async move {
                                    be.rename(&e, &name).await?;
                                    Ok("renamed (Refresh)".into())
                                })
                            },
                        );
                    }
                }
                PaneAction::Play(_pane, path, headers) => {
                    self.status = format!("Playing {path} in built-in player");
                    self.pending_play.push((path, headers));
                }
                PaneAction::StreamPlay(i, entry) => {
                    // Resolve provider stream URL async; result polled
                    // in ui(). Crypt/SFTP/FTP backends Err -> status.
                    if let Ok((_acc, be)) = Self::backend(&settings, &self.panes[i].account_id) {
                        {
                            let slot: StreamSlot = Arc::new(Mutex::new(None));
                            let slot2 = slot.clone();
                            let name = entry.name.clone();
                            self.status = format!("Resolving stream for {name}...");
                            self.stream_slot = Some(slot);
                            tokio_rt.spawn(async move {
                                let r = be.stream_url(&entry).await.map(|s| (s.url, s.headers));
                                *crate::lock_unpoison(&slot2) = Some(r);
                            });
                        }
                    }
                }
                PaneAction::DownloadTask(i) => {
                    // WebDAV only: direct URL into segmented engine.
                    let entry = self.panes[i]
                        .entries
                        .iter()
                        .find(|e| self.panes[i].selected.contains(&e.id) && !e.is_dir)
                        .cloned();
                    if let Some(e) = entry {
                        if let Some(acc) = account_by_id(&settings, &self.panes[i].account_id) {
                            if acc.kind == DriveKind::WebDav {
                                let base = acc.endpoint.trim_end_matches('/');
                                self.pending_download_tasks.push((
                                    format!(
                                        "{base}/{}",
                                        downloadcraft_lib::drive::pct_path(&e.path)
                                    ),
                                    acc.username.clone(),
                                    acc.secret.clone(),
                                ));
                                self.status = "Sent to Downloads (segmented engine).".into();
                            } else {
                                self.status = "Download button (copy to other pane) for this backend; Send-to-Downloads supports WebDAV direct links.".into();
                            }
                        }
                    }
                }
            }
        }

        // Jobs panel.
        let any_running = self
            .jobs
            .iter()
            .any(|j| !j.done_flag.load(Ordering::Relaxed));
        if any_running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
        let mut remove_jobs: Vec<(usize, bool)> = Vec::new(); // (idx, delete_files)
        theme::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Transfers").strong().color(theme::TEXT));
                ui.label(
                    egui::RichText::new(format!("{} job(s)", self.jobs.len()))
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
                ui.label(
                    egui::RichText::new("Parallel per account: Edit account > Parallel streams")
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui::secondary_button(ui, "Clear finished").clicked() {
                        self.jobs.retain(|j| !j.done_flag.load(Ordering::Relaxed));
                    }
                });
            });
            if self.jobs.is_empty() {
                ui.label(
                    egui::RichText::new("No transfers. Select files in one pane, Copy/Move to other pane.")
                        .color(theme::TEXT_MUTED)
                        .small(),
                );
            }
            egui::ScrollArea::vertical()
                .id_salt("drive-jobs")
                .max_height(120.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let mut remove_click: Vec<(usize, bool)> = Vec::new();
                    for (ji, j) in self.jobs.iter_mut().enumerate() {
                        let done = j.prog.done.load(Ordering::Relaxed);
                        let total = j.prog.total.load(Ordering::Relaxed);
                        let now = std::time::Instant::now();
                        let dt = now.duration_since(j.last_at).as_secs_f64();
                        if dt >= 0.5 {
                            j.speed = (done as f64 - j.last_done as f64) / dt.max(0.01);
                            j.last_done = done;
                            j.last_at = now;
                        }
                        let st = lock(&j.status).clone();
                        let is_err = st.starts_with("Error");
                        let is_done_flag = j.done_flag.load(Ordering::Relaxed);
                        let elapsed = j.started.elapsed().as_secs();
                        let stats = format!(
                            "{} / {} · {}/s · {} · {}",
                            fmt_bytes(done.min(total.max(done))),
                            if total > 0 { fmt_bytes(total) } else { "?".into() },
                            fmt_bytes(j.speed.max(0.0) as u64),
                            st.chars().take(80).collect::<String>(),
                            fmt_elapsed(elapsed)
                        );
                        ui.horizontal(|ui| {
                            let lab_w = (ui.available_width() * 0.38).clamp(70.0, 240.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(lab_w, theme::BTN_H),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(j.label.clone()).color(theme::TEXT).small())
                                            .truncate(),
                                    )
                                    .on_hover_text(j.label.clone());
                                },
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if is_done_flag {
                                    // Stopped rows removable; Cancelled/Error can delete data.
                                    if ui::secondary_button(ui, "Remove").clicked() {
                                        remove_click.push((ji, is_err || st.starts_with("Cancelled")));
                                    }
                                } else if ui::secondary_button(ui, "Cancel").clicked() {
                                    j.prog.cancelled.store(true, Ordering::Relaxed);
                                }
                                if !is_done_flag {
                                    let paused = j.prog.paused.load(Ordering::Relaxed);
                                    if ui::Btn::secondary(if paused { "Resume" } else { "Pause" }).min_width(PAUSE_W).show(ui).clicked() {
                                        j.prog.paused.store(!paused, Ordering::Relaxed);
                                    }
                                }
                                if ui::icon_button(ui, Icon::Info, "Details").clicked() {
                                    j.show_info = !j.show_info;
                                }
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(stats.clone())
                                            .color(if is_err { theme::ERR } else { theme::TEXT_MUTED })
                                            .small(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(stats.clone());
                            });
                        });
                        if is_err {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(st.clone()).color(theme::ERR).small(),
                                )
                                .wrap(),
                            )
                            .on_hover_text(st.clone());
                        }
                        if j.show_info {
                            let eta = if j.speed > 1.0 && total > done {
                                fmt_elapsed(((total - done) as f64 / j.speed) as u64)
                            } else {
                                "—".into()
                            };
                            let pct = if total > 0 { format!("{:.0}%", done as f32 / total as f32 * 100.0) } else { "?".into() };
                            ui.label(
                                egui::RichText::new(format!(
                                    "What: {} · progress {} ({}) · speed {}/s · ETA {} · elapsed {} · done may count download+upload for copies",
                                    j.label,
                                    fmt_bytes(done),
                                    pct,
                                    fmt_bytes(j.speed.max(0.0) as u64),
                                    eta,
                                    fmt_elapsed(elapsed)
                                ))
                                .color(theme::TEXT_FAINT)
                                .small(),
                            );
                        }
                        let frac = if total > 0 {
                            (done as f32 / total as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(ui.available_width())
                                .desired_height(8.0),
                        );
                    }
                    remove_jobs.extend(remove_click);
                });
        });
        for (ji, delete_files) in remove_jobs.into_iter().rev() {
            if ji >= self.jobs.len() {
                continue;
            }
            let job = self.jobs.remove(ji);
            if delete_files && job.is_transfer && !job.dst_account.is_empty() {
                let settings2 = settings.clone();
                let dst_id = job.dst_account.clone();
                let dst_dir = job.dst_dir.clone();
                let names = job.dest_names.clone();
                tokio_rt.spawn(async move {
                    if let Ok((_, be)) = Self::backend(&settings2, &dst_id) {
                        for n in &names {
                            downloadcraft_lib::drive::jobs::remove_dest(&be, &dst_dir, n).await;
                        }
                    }
                });
                self.status = "Transfer removed; destination data deleted.".into();
            } else {
                self.status = "Transfer removed.".into();
            }
        }

        // Draw order = stacking order: wizard opens from Accounts, the
        // confirmation from either, so each later modal sits on top.
        if self.show_accounts || self.edit.is_some() {
            self.accounts_window(ui, app_rt, tokio_rt);
        }
        if self.show_sync {
            self.sync_window(ui, app_rt, tokio_rt);
        }
        if self.wizard.is_some() {
            self.wizard_window(ui, app_rt, tokio_rt);
        }
        if self.confirm.is_some() {
            self.confirm_window(ui.ctx(), app_rt, tokio_rt, &settings);
        }
    }

    fn confirm_window(
        &mut self,
        ctx: &egui::Context,
        app_rt: &Arc<Mutex<Runtime>>,
        tokio_rt: &Arc<tokio::runtime::Runtime>,
        settings: &Settings,
    ) {
        let Some(c) = &self.confirm else {
            return;
        };
        let (title, body, action) = match c {
            DriveConfirm::Files(i) => {
                let n = self.panes[*i].selected.len();
                let acc = account_by_id(settings, &self.panes[*i].account_id)
                    .map(|a| a.display())
                    .unwrap_or_default();
                (
                    format!("Delete {n} item(s)?"),
                    format!("Permanently deletes the selected files and folders from {acc}. This cannot be undone."),
                    "Delete",
                )
            }
            DriveConfirm::Account(_, name) => (
                "Remove account?".to_string(),
                format!("Removes \"{name}\" and its stored credentials from webcraft. Files on the remote are not touched."),
                "Remove account",
            ),
        };
        let mut confirm = false;
        let mut cancel = false;
        let modal = ui::Modal::new("drive-confirm", title)
            .width(ui::size::SM)
            .show(ctx, |ui| {
                ui::dialog_text(ui, body, false);
                ui.add_space(theme::SP_SM);
                ui.separator();
                ui::footer_row(ui, |ui| {
                    if ui::destructive_button(ui, action).clicked() {
                        confirm = true;
                    }
                    let c = ui::secondary_button(ui, "Cancel");
                    ui::focus_on_open(ui, &c);
                    if c.clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            match self.confirm.take() {
                Some(DriveConfirm::Files(i)) => self.delete_selected(tokio_rt, settings, i),
                Some(DriveConfirm::Account(id, _)) => {
                    let mut g = crate::lock_unpoison(app_rt);
                    g.state.settings.drive_accounts.retain(|a| a.id != id);
                    g.save();
                }
                None => {}
            }
        } else if cancel || modal.close {
            self.confirm = None;
        }
    }

    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        settings: &Settings,
        actions: &mut Vec<PaneAction>,
    ) {
        theme::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Header: account picker + refresh.
            let mut changed_account: Option<String> = None;
            ui.horizontal(|ui| {
                let cur = account_by_id(settings, &self.panes[idx].account_id)
                    .map(|a| a.display())
                    .unwrap_or_else(|| "Select account".into());
                egui::ComboBox::from_id_salt(format!("drive-acc-{idx}"))
                    .selected_text(cur)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_value(
                                &mut self.panes[idx].account_id,
                                LOCAL_ID.to_string(),
                                "This PC (download folder)",
                            )
                            .clicked()
                        {
                            changed_account = Some(LOCAL_ID.to_string());
                        }
                        for acc in &settings.drive_accounts {
                            if ui
                                .selectable_value(
                                    &mut self.panes[idx].account_id,
                                    acc.id.clone(),
                                    acc.display(),
                                )
                                .clicked()
                            {
                                changed_account = Some(acc.id.clone());
                            }
                        }
                    });
                if ui::icon_button(ui, Icon::Up, "Up (parent folder)").clicked() {
                    let p = self.panes[idx].path.clone();
                    let parent = if self.panes[idx].account_id == LOCAL_ID {
                        std::path::Path::new(&p)
                            .parent()
                            .map(|x| x.to_string_lossy().into_owned())
                            .unwrap_or(p.clone())
                    } else {
                        downloadcraft_lib::drive::parent_path(&p)
                    };
                    actions.push(PaneAction::Navigate(idx, parent));
                }
                if ui::secondary_button(ui, "Refresh").clicked() {
                    actions.push(PaneAction::Refresh(idx));
                }
                if self.panes[idx].loading {
                    ui.spinner();
                }
            });
            if let Some(id) = changed_account {
                actions.push(PaneAction::AccountChanged(idx, id));
            }
            // Breadcrumb / local path.
            if self.panes[idx].account_id == LOCAL_ID {
                ui.horizontal(|ui| {
                    let mut p = if self.panes[idx].path_edit.is_empty() {
                        self.panes[idx].path.clone()
                    } else {
                        self.panes[idx].path_edit.clone()
                    };
                    let resp = ui.add(
                        ui::text_field(&mut p)
                            .hint_text("Local folder path")
                            .desired_width(f32::INFINITY),
                    );
                    self.panes[idx].path_edit = p.clone();
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        actions.push(PaneAction::Navigate(idx, p));
                        self.panes[idx].path_edit.clear();
                    }
                });
            } else {
                ui.horizontal_wrapped(|ui| {
                    if ui.link("root").clicked() {
                        actions.push(PaneAction::Navigate(idx, String::new()));
                    }
                    let parts: Vec<String> = self.panes[idx]
                        .path
                        .trim_matches('/')
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    let mut acc_path = String::new();
                    for (i, part) in parts.iter().enumerate() {
                        ui.label("/");
                        if !acc_path.is_empty() {
                            acc_path.push('/');
                        }
                        acc_path.push_str(part);
                        let target = acc_path.clone();
                        if ui.link(part.clone()).clicked() {
                            let _ = i;
                            actions.push(PaneAction::Navigate(idx, target));
                        }
                    }
                });
            }
            if !self.panes[idx].error.is_empty() {
                ui.label(
                    egui::RichText::new(self.panes[idx].error.clone())
                        .color(theme::ERR)
                        .small(),
                );
            }
            // Transfer buttons.
            ui.horizontal_wrapped(|ui| {
                let has_sel = !self.panes[idx].selected.is_empty();
                ui.add_enabled_ui(has_sel, |ui| {
                    if ui::secondary_button(ui, if idx == 0 { "Copy →" } else { "← Copy" })
                        .clicked()
                    {
                        actions.push(PaneAction::Copy(idx));
                    }
                    if ui::secondary_button(ui, "Move").clicked() {
                        actions.push(PaneAction::Move(idx));
                    }
                    if ui::secondary_button(ui, "Delete").clicked() {
                        actions.push(PaneAction::Delete(idx));
                    }
                    if ui::secondary_button(ui, "To Downloads").clicked() {
                        actions.push(PaneAction::DownloadTask(idx));
                    }
                });
                // Server-side copy: only Google Drive -> Google Drive
                // panes (either account, incl. same account navigated
                // twice). Crypt accounts excluded (ids encrypted).
                let gdrive_pair = [idx, 1 - idx].iter().all(|&i| {
                    account_by_id(settings, &self.panes[i].account_id)
                        .map(|a| a.kind == DriveKind::GoogleDrive && a.crypt_password.is_empty())
                        .unwrap_or(false)
                });
                if gdrive_pair {
                    ui.add_enabled_ui(has_sel, |ui| {
                        if ui::secondary_button(ui, "Server-side copy")
                            .on_hover_text(
                                "Google Drive copies bytes server-to-server (files.copy); \
                                 no download to this PC. No staging fallback: failures are errors.",
                            )
                            .clicked()
                        {
                            actions.push(PaneAction::ServerCopy(idx));
                        }
                    });
                }
                ui.add(
                    ui::text_field(&mut self.panes[idx].new_name)
                        .hint_text("New folder")
                        .desired_width(110.0),
                );
                if ui::secondary_button(ui, "Mkdir").clicked() {
                    actions.push(PaneAction::Mkdir(idx));
                }
                ui.add(
                    ui::text_field(&mut self.panes[idx].rename_name)
                        .hint_text("Rename to")
                        .desired_width(110.0),
                );
                if ui::secondary_button(ui, "Rename").clicked() {
                    actions.push(PaneAction::Rename(idx));
                }
            });
            ui.add_space(theme::SP_XS);
            // File list.
            let avail_h = ui.available_height().max(120.0);
            theme::list_frame().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(format!("drive-pane-{idx}"))
                    .max_height(avail_h)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // Header row
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Name")
                                    .strong()
                                    .small()
                                    .color(theme::TEXT_MUTED),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new("Modified    Size")
                                            .small()
                                            .color(theme::TEXT_FAINT),
                                    );
                                },
                            );
                        });
                        ui.separator();
                        let entries = self.panes[idx].entries.clone();
                        for (ei, e) in entries.iter().enumerate() {
                            let selected = self.panes[idx].selected.contains(&e.id);
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 20.0),
                                egui::Sense::click(),
                            );
                            if selected {
                                ui.painter().rect_filled(rect, 4.0, theme::STEEL_DIM);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(rect, 4.0, theme::SURFACE_HOVER);
                            }
                            let fg = if selected {
                                egui::Color32::WHITE
                            } else {
                                theme::TEXT
                            };
                            let irect = egui::Rect::from_min_size(
                                egui::pos2(rect.left() + 4.0, rect.top() + 3.0),
                                egui::vec2(16.0, 16.0),
                            );
                            icon(
                                ui.painter(),
                                &if e.is_dir {
                                    Icon::Folder
                                } else {
                                    crate::app::cat_icon_pub(&e.name)
                                },
                                irect,
                                fg,
                            );
                            // name truncated
                            let name_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 26.0, rect.top()),
                                egui::pos2(rect.right() - 170.0, rect.bottom()),
                            );
                            ui.painter().text(
                                name_rect.left_top() + egui::vec2(0.0, 3.0),
                                egui::Align2::LEFT_TOP,
                                &e.name,
                                egui::TextStyle::Body.resolve(ui.style()),
                                fg,
                            );
                            ui.painter().text(
                                egui::pos2(rect.right() - 160.0, rect.top() + 3.0),
                                egui::Align2::LEFT_TOP,
                                fmt_time(e.modified),
                                egui::TextStyle::Small.resolve(ui.style()),
                                theme::TEXT_FAINT,
                            );
                            ui.painter().text(
                                egui::pos2(rect.right() - 8.0, rect.top() + 3.0),
                                egui::Align2::RIGHT_TOP,
                                if e.is_dir {
                                    "—".into()
                                } else {
                                    fmt_bytes(e.size)
                                },
                                egui::TextStyle::Small.resolve(ui.style()),
                                theme::TEXT_MUTED,
                            );
                            if resp.clicked() {
                                self.focus = idx;
                                let (ctrl, shift) = ui.input(|i| {
                                    (i.modifiers.ctrl || i.modifiers.command, i.modifiers.shift)
                                });
                                if shift {
                                    if let Some(a) = self.panes[idx].anchor {
                                        let (lo, hi) = (a.min(ei), a.max(ei));
                                        if !ctrl {
                                            self.panes[idx].selected.clear();
                                        }
                                        for e2 in &entries[lo..=hi] {
                                            self.panes[idx].selected.insert(e2.id.clone());
                                        }
                                    } else {
                                        self.panes[idx].selected.clear();
                                        self.panes[idx].selected.insert(e.id.clone());
                                        self.panes[idx].anchor = Some(ei);
                                    }
                                } else if ctrl {
                                    if selected {
                                        self.panes[idx].selected.remove(&e.id);
                                    } else {
                                        self.panes[idx].selected.insert(e.id.clone());
                                    }
                                    self.panes[idx].anchor = Some(ei);
                                } else {
                                    self.panes[idx].selected.clear();
                                    self.panes[idx].selected.insert(e.id.clone());
                                    self.panes[idx].anchor = Some(ei);
                                }
                            }
                            if resp.double_clicked() && e.is_dir {
                                let p = if self.panes[idx].account_id == LOCAL_ID {
                                    e.id.clone()
                                } else {
                                    e.path.clone()
                                };
                                actions.push(PaneAction::Navigate(idx, p));
                            } else if resp.double_clicked()
                                && downloadcraft_lib::model::category_for(&e.name) == "Video"
                            {
                                // Double-click video: local plays direct;
                                // remote resolves stream URL async (mpv
                                // streams provider URL, no download).
                                if self.panes[idx].account_id == LOCAL_ID {
                                    actions.push(PaneAction::Play(idx, e.id.clone(), Vec::new()));
                                } else {
                                    actions.push(PaneAction::StreamPlay(idx, e.clone()));
                                }
                            }
                        }
                        if entries.is_empty() && !self.panes[idx].loading {
                            ui.label(
                                egui::RichText::new("Empty or not loaded. Refresh.")
                                    .color(theme::TEXT_MUTED)
                                    .small(),
                            );
                        }
                    });
            });
        });
    }

    fn poll_oauth(&mut self, app_rt: &Arc<Mutex<Runtime>>) {
        let done = lock(&self.oauth_slot).take();
        if let Some(res) = done {
            self.oauth_busy = false;
            self.pending_auth = None;
            self.oauth_cancel = None;
            match res {
                Ok(msg) => self.status = msg,
                Err(e) => self.status = format!("OAuth: {e}"),
            }
            // Connected Google account: auto-fetch shared drives for picker.
            if !self.oauth_for.is_empty() {
                let acc = crate::lock_unpoison(app_rt)
                    .state
                    .settings
                    .drive_accounts
                    .iter()
                    .find(|a| a.id == self.oauth_for)
                    .cloned();
                if let Some(acc) = acc {
                    if acc.kind == DriveKind::GoogleDrive {
                        self.fetch_shared_drives(acc);
                    }
                }
            }
        }
        let test = lock(&self.test_slot).take();
        if let Some(res) = test {
            match res {
                Ok(m) => self.status = m,
                Err(e) => self.status = e,
            }
        }
        let drives = lock(&self.drives_slot).take();
        if let Some((for_id, res)) = drives {
            self.shared_for = for_id;
            match res {
                Ok(v) => {
                    self.status = format!("Shared drives: {} found. Pick in Edit.", v.len());
                    self.shared_list = v;
                }
                Err(e) => self.status = format!("Shared drives: {e}"),
            }
        }
    }

    fn fetch_shared_drives(&mut self, acc: DriveAccount) {
        let slot = self.drives_slot.clone();
        let id = acc.id.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let res = match rt {
                Ok(rt) => rt.block_on(downloadcraft_lib::drive::shared_drives(&acc)),
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some((id, res));
        });
    }

    fn test_account(&mut self, acc: DriveAccount) {
        let slot = self.test_slot.clone();
        self.status = format!("Testing {}…", acc.display());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let res = match rt {
                Ok(rt) => rt.block_on(downloadcraft_lib::drive::test_account(&acc)),
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some(res);
        });
    }

    fn persist_tokens(
        app_rt: &Arc<Mutex<Runtime>>,
        acc_id: &str,
        tok: &downloadcraft_lib::drive::oauth::TokenOut,
    ) -> Result<String, String> {
        if tok.refresh_token.is_empty() {
            return Err("provider returned no refresh token — check offline access / consent prompt, then Connect again".into());
        }
        let mut g = crate::lock_unpoison(app_rt);
        let a = g
            .state
            .settings
            .drive_accounts
            .iter_mut()
            .find(|x| x.id == acc_id)
            .ok_or("account deleted during sign-in")?;
        a.access_token = tok.access_token.clone();
        a.refresh_token = tok.refresh_token.clone();
        a.token_expires = downloadcraft_lib::drive::oauth::expires_at(tok.expires_in);
        g.save();
        Ok("Connected. Tokens saved — now Test.".to_string())
    }

    // ── Wizard (rclone config order) ─────────────────────────────
    fn wz_poll(&self, wz: &mut WizardState) {
        if let Some(res) = lock(&wz.token_slot).take() {
            wz.oauth_busy = false;
            wz.pending = None;
            wz.cancel = None;
            match res {
                Ok(tok) if !tok.refresh_token.is_empty() => {
                    wz.draft.access_token = tok.access_token;
                    wz.draft.refresh_token = tok.refresh_token;
                    wz.draft.token_expires =
                        downloadcraft_lib::drive::oauth::expires_at(tok.expires_in);
                    wz.verified = true;
                    wz.status = "Signed in. Next: pick root / Shared Drive.".into();
                }
                Ok(_) => {
                    wz.status = "No refresh token returned — enable offline access + consent prompt, try Connect again.".into()
                }
                Err(e) => wz.status = format!("Sign-in: {e}"),
            }
        }
        if let Some(res) = lock(&wz.test_slot).take() {
            wz.test_busy = false;
            match res {
                Ok(m) => {
                    wz.verified = true;
                    wz.status = m;
                }
                Err(e) => {
                    wz.verified = false;
                    wz.status = e;
                }
            }
        }
        if let Some((_, res)) = lock(&wz.drives_slot).take() {
            wz.root_loading = false;
            match res {
                Ok(v) => {
                    wz.status = format!("{} Shared Drive(s) found.", v.len());
                    wz.shared_list = v;
                }
                Err(e) => wz.status = format!("Shared drives: {e}"),
            }
        }
        if wz.step == 4 {
            if let Some(res) = lock(&wz.final_slot).take() {
                match res {
                    Ok(m) => {
                        wz.final_ok = true;
                        wz.status = m;
                    }
                    Err(e) => {
                        wz.final_ok = false;
                        wz.status = e;
                    }
                }
            }
        }
    }

    fn wizard_window(
        &mut self,
        ui: &mut egui::Ui,
        app_rt: &Arc<Mutex<Runtime>>,
        tokio_rt: &Arc<tokio::runtime::Runtime>,
    ) {
        let Some(mut wz) = self.wizard.take() else {
            return;
        };
        self.wz_poll(&mut wz);
        let ctx = ui.ctx().clone();
        let mut close_cancel = false;
        let mut finish = false;
        let modal = ui::Modal::new("drive-wizard", "Add account")
            .width(ui::size::MD)
            .height(700.0)
            .resizable(true)
            .show(&ctx, |ui| {
                const STEPS: [&str; 5] = ["Provider", "Details", "Connect", "Root", "Review"];
                ui.horizontal(|ui| {
                    for (i, s) in STEPS.iter().enumerate() {
                        let cur = wz.step as usize == i;
                        ui::badge(
                            ui,
                            &format!("{} {s}", i + 1),
                            if cur { ui::BadgeKind::Info } else { ui::BadgeKind::Neutral },
                        );
                    }
                });
                ui.add_space(theme::SP_XS);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(wz.status.clone())
                            .color(theme::STEEL_TEXT)
                            .small(),
                    )
                    .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .on_hover_text(wz.status.clone());
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("wz-body")
                    .auto_shrink([false, false])
                    .max_height((ui.available_height() - ui::FOOTER_H).max(48.0))
                    .show(ui, |ui| {
                        match wz.step {
                            0 => {
                                ui.label("Storage type (rclone: Choose storage to configure)");
                                for k in DriveKind::ALL {
                                    if ui
                                        .selectable_label(wz.draft.kind == k, format!("{} — {}", k.label(), kind_hint(k)))
                                        .clicked()
                                    {
                                        wz.draft.kind = k;
                                        if !wz.name_touched {
                                            wz.draft.name = k.label().into();
                                        }
                                        wz.invalidate();
                                    }
                                }
                            }
                            1 => {
                                let before = wz.draft.clone();
                                form_row(ui, "Name", &mut wz.draft.name, false, "");
                                if wz.draft.name != before.name {
                                    wz.name_touched = true;
                                }
                                setup_panel(ui, wz.draft.kind);
                                credential_fields(ui, &mut wz.draft, false);
                                if wz.draft.client_id != before.client_id
                                    || wz.draft.client_secret != before.client_secret
                                    || wz.draft.secret != before.secret
                                    || wz.draft.endpoint != before.endpoint
                                    || wz.draft.username != before.username
                                {
                                    wz.invalidate();
                                }
                            }
                            2 => {
                                if wz.is_oauth() {
                                    ui.label("Use web browser to authenticate? webcraft opens browser, redirect http://127.0.0.1:53682/ must be registered (Dropbox/OneDrive; Google: Desktop app).");
                                    ui.horizontal(|ui| {
                                        if ui::secondary_button(ui, "Connect (open browser)").clicked() && !wz.oauth_busy {
                                            wz.start_oauth();
                                        }
                                        if ui::secondary_button(ui, "Copy sign-in link").clicked() {
                                            copy_text(&wz.auth_url.clone());
                                        }
                                        if wz.oauth_busy && ui::secondary_button(ui, "Cancel").clicked() {
                                            if let Some(c) = &wz.cancel {
                                                c.store(true, Ordering::Relaxed);
                                            }
                                            wz.oauth_busy = false;
                                            wz.pending = None;
                                            wz.cancel = None;
                                        }
                                    });
                                    if wz.oauth_busy {
                                        ui.label("Waiting for browser… or paste fallback:");
                                        ui.horizontal(|ui| {
                                            ui.add(ui::text_field(&mut wz.code_input).hint_text("http://127.0.0.1:53682/?code=...").desired_width((ui.available_width() - 130.0).max(140.0)));
                                            if ui::secondary_button(ui, "Finish paste").clicked() {
                                                wz.paste_finish();
                                            }
                                        });
                                    }
                                } else {
                                    ui.label("Test credentials before root pick.");
                                    if ui::secondary_button(ui, "Test connection").clicked() && !wz.test_busy {
                                        wz.start_test(false);
                                    }
                                }
                                if wz.verified {
                                    ui.label(egui::RichText::new("Verified. Next enabled.").color(theme::OK).small());
                                }
                            }
                            3 => {
                                match wz.draft.kind {
                                    DriveKind::GoogleDrive => {
                                        ui.label("Configure this as a Shared Drive (Team Drive)? (rclone step)");
                                        let mut is_team = wz.use_team || !wz.draft.team_drive.is_empty();
                                        ui.horizontal(|ui| {
                                            if ui.selectable_label(!is_team, "My Drive").clicked() {
                                                wz.draft.team_drive.clear();
                                                wz.use_team = false;
                                                is_team = false;
                                            }
                                            if ui.selectable_label(is_team, "Shared Drive").clicked() {
                                                wz.use_team = true;
                                                is_team = true;
                                                if wz.shared_list.is_empty() && !wz.root_loading {
                                                    wz.fetch_drives();
                                                }
                                            }
                                        });
                                        if is_team {
                                            let cur = wz
                                                .shared_list
                                                .iter()
                                                .find(|(id, _)| id == &wz.draft.team_drive)
                                                .map(|(_, n)| n.clone())
                                                .unwrap_or_else(|| {
                                                    if wz.draft.team_drive.is_empty() {
                                                        "Pick Shared Drive…".into()
                                                    } else {
                                                        wz.draft.team_drive.clone()
                                                    }
                                                });
                                            ui.horizontal(|ui| {
                                                egui::ComboBox::from_id_salt("wz-shared")
                                                    .selected_text(cur)
                                                    .width(230.0)
                                                    .show_ui(ui, |ui| {
                                                        for (id, name) in wz.shared_list.clone() {
                                                            ui.selectable_value(&mut wz.draft.team_drive, id, name);
                                                        }
                                                    });
                                                if ui::secondary_button(ui, "Load").clicked() && !wz.root_loading {
                                                    wz.fetch_drives();
                                                }
                                                if wz.root_loading {
                                                    ui.spinner();
                                                }
                                            });
                                            let mut tid = wz.draft.team_drive.clone();
                                            form_row(ui, "Or paste Team Drive link/id", &mut tid, false, "drive.google.com/drive/folders/<driveId>");
                                            wz.draft.team_drive = downloadcraft_lib::drive::gdrive::parse_drive_id(&tid);
                                        } else {
                                            wz.draft.team_drive.clear();
                                        }
                                        let mut raw = wz.draft.root.clone();
                                        form_row(ui, "Start folder (optional)", &mut raw, false, "blank = drive root; folder link/id inside picked drive");
                                        wz.draft.root = downloadcraft_lib::drive::gdrive::parse_drive_id(&raw);
                                    }
                                    DriveKind::S3 => {
                                        form_row(ui, "Bucket", &mut wz.draft.root, false, "bucket name");
                                        ui.label(egui::RichText::new("Bucket required; final Review test verifies it.").color(theme::TEXT_FAINT).small());
                                    }
                                    DriveKind::Local => {
                                        form_row(ui, "Folder", &mut wz.draft.root, false, "C:\\data");
                                    }
                                    _ => {
                                        form_row(ui, "Root path (optional)", &mut wz.draft.root, false, "");
                                    }
                                }
                            }
                            _ => {
                                ui.label(egui::RichText::new("Configuration summary (rclone: Keep this remote?)").strong());
                                for (k, v) in review_rows(&wz.draft) {
                                    ui.horizontal(|ui| {
                                        ui.add_sized([110.0, 18.0], egui::Label::new(egui::RichText::new(k).color(theme::TEXT_MUTED).small()));
                                        ui.add(egui::Label::new(egui::RichText::new(v).small()).wrap_mode(egui::TextWrapMode::Truncate));
                                    });
                                }
                                if !wz.final_started {
                                    wz.final_started = true;
                                    wz.start_final_test();
                                }
                                if wz.final_ok {
                                    ui.label(egui::RichText::new("Final test passed. Finish saves.").color(theme::OK).small());
                                } else {
                                    ui.horizontal(|ui| {
                                        if ui::secondary_button(ui, "Re-test").clicked() {
                                            wz.final_started = true;
                                            wz.start_final_test();
                                        }
                                        ui.label("Finish disabled until final test passes.");
                                    });
                                }
                            }
                        }
                    });
                ui.add_space(theme::SP_SM);
                ui.separator();
                ui::footer_row(ui, |ui| {
                    {
                        let (next_label, can_next) = match wz.step {
                            0 => ("Next", true),
                            1 => ("Next", details_valid(&wz.draft)),
                            2 => ("Next", wz.verified),
                            3 => ("Next", root_valid(&wz.draft) && (!wz.use_team || !wz.draft.team_drive.is_empty())),
                            _ => ("Finish", wz.final_ok),
                        };
                        ui.add_enabled_ui(can_next, |ui| {
                            if ui::primary_button(ui, next_label).clicked() {
                                if wz.step == 4 {
                                    finish = true;
                                } else {
                                    wz.step += 1;
                                    // Entering Root for Google auto-fetches after Connect.
                                    if wz.step == 3
                                        && wz.draft.kind == DriveKind::GoogleDrive
                                        && wz.shared_list.is_empty()
                                        && wz.verified
                                    {
                                        wz.fetch_drives();
                                    }
                                    wz.status = String::new();
                                }
                            }
                        });
                        if wz.step > 0 && ui::secondary_button(ui, "Back").clicked() {
                            if let Some(c) = &wz.cancel {
                                c.store(true, Ordering::Relaxed);
                            }
                            wz.oauth_busy = false;
                            wz.pending = None;
                            wz.cancel = None;
                            wz.step -= 1;
                        }
                        if ui::ghost_button(ui, "Cancel").clicked() {
                            close_cancel = true;
                        }
                    }
                });
            });
        let open = !modal.close;
        if finish {
            let mut draft = wz.draft.clone();
            if draft.kind == DriveKind::GoogleDrive {
                draft.team_drive =
                    downloadcraft_lib::drive::gdrive::parse_drive_id(&draft.team_drive);
                draft.root = downloadcraft_lib::drive::gdrive::parse_drive_id(&draft.root);
            }
            let idx = if self.panes[1].account_id == LOCAL_ID {
                1
            } else if self.panes[0].account_id == LOCAL_ID {
                0
            } else {
                self.focus
            };
            {
                let mut g = crate::lock_unpoison(app_rt);
                g.state.settings.drive_accounts.push(draft.clone());
                g.save();
            }
            self.panes[idx].account_id = draft.id.clone();
            // Local pane path semantics differ; cloud starts at root "".
            self.panes[idx].path = String::new();
            self.panes[idx].selected.clear();
            let settings = crate::lock_unpoison(app_rt).state.settings.clone();
            self.status = format!("Account added: {}", draft.display());
            self.wizard = None;
            self.refresh(tokio_rt, &settings, idx);
            return;
        }
        if close_cancel || !open {
            if let Some(c) = &wz.cancel {
                c.store(true, Ordering::Relaxed);
            }
            self.wizard = None;
            self.status = "Wizard cancelled. Nothing saved.".into();
            return;
        }
        self.wizard = Some(wz);
    }

    fn accounts_window(
        &mut self,
        ui: &mut egui::Ui,
        app_rt: &Arc<Mutex<Runtime>>,
        tokio_rt: &Arc<tokio::runtime::Runtime>,
    ) {
        let ctx = ui.ctx().clone();
        let mut close_clicked = false;
        let modal = ui::Modal::new("drive-accounts", "Drive accounts")
            .width(ui::size::MD)
            .height(700.0)
            .resizable(true)
            .show(&ctx, |ui| {
              ui::dialog_body_footer(ui, "drive-accounts-body", |ui| {
                let accounts: Vec<DriveAccount> = crate::lock_unpoison(app_rt).state.settings.drive_accounts.clone();
                ui.horizontal(|ui| {
                    if ui::Btn::new(if self.edit.is_some() { ui::ButtonKind::Secondary } else { ui::ButtonKind::Primary }, "Add account").show(ui).clicked() {
                        self.wizard = Some(WizardState::new());
                    }
                    if ui::secondary_button(ui, "Import rclone").clicked() {
                        self.show_import = !self.show_import;
                    }
                    ui.label(
                        egui::RichText::new("Secrets plain in state.json (same as Debrid). Never logged.")
                            .color(theme::TEXT_FAINT)
                            .small(),
                    );
                });
                if self.show_import {
                    theme::panel().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Paste rclone config").strong().color(theme::TEXT));
                        ui.label(egui::RichText::new("Paste whole rclone.conf or sections ([name] type=drive/webdav/s3/sftp/ftp/dropbox/onedrive). Import creates accounts; check warnings, Test, then Edit Bucket/root if told.").color(theme::TEXT_MUTED).small());
                        ui.add(
                            egui::TextEdit::multiline(&mut self.import_text)
                                .hint_text("[mygdrive]\ntype = drive\nclient_id = ...")
                                .desired_width(ui.available_width())
                                .desired_rows(8),
                        );
                        ui.horizontal(|ui| {
                            if ui::secondary_button(ui, "Import").clicked() {
                                let (accs, warns) = downloadcraft_lib::drive::rclone_import::parse_rclone_config(&self.import_text);
                                let n = accs.len();
                                if n > 0 {
                                    let mut g = crate::lock_unpoison(app_rt);
                                    g.state.settings.drive_accounts.extend(accs);
                                    g.save();
                                }
                                self.import_text.clear();
                                self.show_import = false;
                                self.status = if warns.is_empty() {
                                    format!("Imported {n} account(s) from rclone.")
                                } else {
                                    format!("Imported {n} · warnings: {}", warns.join("; "))
                                };
                            }
                            if ui::secondary_button(ui, "Cancel").clicked() {
                                self.import_text.clear();
                                self.show_import = false;
                            }
                        });
                    });
                }
                ui.add_space(theme::SP_SM);
                for acc in &accounts {
                    ui.horizontal(|ui| {
                        ui::badge(ui, acc.kind.label(), ui::BadgeKind::Info);
                        ui.label(egui::RichText::new(acc.display()).color(theme::TEXT));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui::ghost_button(ui, "Remove").clicked() {
                                self.confirm = Some(DriveConfirm::Account(acc.id.clone(), acc.display()));
                            }
                            if ui::secondary_button(ui, "Test").clicked() {
                                self.test_account(acc.clone());
                            }
                            if ui::secondary_button(ui, "Duplicate").clicked() {
                                let mut dup = acc.clone();
                                dup.id = downloadcraft_lib::model::new_id();
                                dup.name = format!("{} copy", acc.name);
                                {
                                    let mut g = crate::lock_unpoison(app_rt);
                                    g.state.settings.drive_accounts.push(dup.clone());
                                    g.save();
                                }
                                self.status = "Duplicated. Edit copy: change name + root / Team Drive.".into();
                                self.edit = Some(dup);
                                self.edit_is_new = false;
                            }
                            if ui::secondary_button(ui, "Edit").clicked() {
                                self.edit = Some(acc.clone());
                                self.edit_is_new = false;
                                if acc.kind == DriveKind::GoogleDrive && !acc.refresh_token.is_empty() {
                                    self.fetch_shared_drives(acc.clone());
                                }
                            }
                            if matches!(acc.kind, DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox)
                                && ui::secondary_button(ui, if acc.refresh_token.is_empty() { "Connect" } else { "Reconnect" }).clicked()
                            {
                                self.start_oauth(app_rt, acc.clone());
                            }
                        });
                    });
                    ui.label(
                        egui::RichText::new(if acc.refresh_token.is_empty() && matches!(acc.kind, DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox) {
                            "not connected".into()
                        } else {
                            acc.endpoint.clone()
                        })
                        .color(theme::TEXT_FAINT)
                        .small(),
                    );
                    ui.separator();
                }
                if accounts.is_empty() {
                    ui.label(egui::RichText::new("No accounts. Add one.").color(theme::TEXT_MUTED).small());
                }
                let mut save_edit = false;
                let mut cancel_edit = false;
                let mut fetch_shared_edit: Option<DriveAccount> = None;
                if let Some(edit) = &mut self.edit {
                    ui.add_space(theme::SP_SM);
                    theme::panel().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new(if self.edit_is_new { "Add account" } else { "Edit account" }).strong());
                        // Kind picker
                        ui.horizontal(|ui| {
                            ui.label("Type");
                            egui::ComboBox::from_id_salt("drive-kind")
                                .selected_text(edit.kind.label())
                                .show_ui(ui, |ui| {
                                    for k in DriveKind::ALL {
                                        ui.selectable_value(&mut edit.kind, k, k.label());
                                    }
                                });
                        });
                        form_row(ui, "Name", &mut edit.name, false, "");
                        // Guided setup panel per provider.
                        {
                            let (console, steps) =
                                downloadcraft_lib::drive::setup_steps(edit.kind);
                            theme::panel().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new("Setup steps")
                                            .strong()
                                            .color(theme::TEXT),
                                    );
                                    ui.label(
                                        egui::RichText::new(console)
                                            .color(theme::STEEL_TEXT)
                                            .small(),
                                    );
                                    if ui::secondary_button(ui, "Copy redirect URI").clicked() {
                                        copy_text(
                                            downloadcraft_lib::drive::oauth::REDIRECT_URI,
                                        );
                                    }
                                });
                                for (i, s) in steps.iter().enumerate() {
                                    ui.label(
                                        egui::RichText::new(format!("{}. {s}", i + 1))
                                            .color(theme::TEXT_MUTED)
                                            .small(),
                                    );
                                }
                                if matches!(
                                    edit.kind,
                                    DriveKind::GoogleDrive
                                        | DriveKind::OneDrive
                                        | DriveKind::Dropbox
                                ) {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Redirect to register (exact): {}",
                                            downloadcraft_lib::drive::oauth::REDIRECT_URI
                                        ))
                                        .color(theme::TEXT_FAINT)
                                        .small(),
                                    );
                                }
                            });
                        }
                        match edit.kind {
                            DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox => {
                                form_row(ui, "Client ID", &mut edit.client_id, false, "apps.googleusercontent.com / Azure app ID / Dropbox App key");
                                form_row(ui, "Client secret", &mut edit.client_secret, true, "secret Value, shown once at creation");
                                if edit.kind == DriveKind::GoogleDrive {
                                    // Team Drive picker (team_drive) + separate start folder (root).
                                    let cur_label = if edit.team_drive.is_empty() {
                                        "My Drive".to_string()
                                    } else {
                                        self.shared_list
                                            .iter()
                                            .find(|(id, _)| id == &edit.team_drive)
                                            .map(|(_, n)| n.clone())
                                            .unwrap_or_else(|| edit.team_drive.clone())
                                    };
                                    ui.horizontal(|ui| {
                                        ui.add_sized([110.0, 20.0], egui::Label::new(egui::RichText::new("Drive").color(theme::TEXT_MUTED)));
                                        egui::ComboBox::from_id_salt("gdrive-picker")
                                            .selected_text(cur_label)
                                            .width(230.0)
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(&mut edit.team_drive, String::new(), "My Drive");
                                                for (id, name) in self.shared_list.clone() {
                                                    ui.selectable_value(&mut edit.team_drive, id, name);
                                                }
                                            });
                                        if ui::secondary_button(ui, "Load").clicked() {
                                            fetch_shared_edit = Some(edit.clone());
                                        }
                                    });
                                    form_row(ui, "Team Drive link/id", &mut edit.team_drive, false, "paste overwrites; empty = My Drive");
                                    form_row(ui, "Start folder", &mut edit.root, false, "blank = drive root; folder link/id inside drive");
                                } else {
                                    form_row(ui, "Root (optional)", &mut edit.root, false, "folder prefix / empty");
                                }
                                ui.label(egui::RichText::new("Save, then Connect in list above: browser opens, sign in, tokens saved.").color(theme::TEXT_FAINT).small());
                            }
                            DriveKind::WebDav => {
                                form_row(ui, "URL", &mut edit.endpoint, false, "https://host/remote.php/dav/files/user/");
                                form_row(ui, "Username", &mut edit.username, false, "");
                                form_row(ui, "Password/app-pw", &mut edit.secret, true, "");
                            }
                            DriveKind::S3 => {
                                form_row(ui, "Endpoint", &mut edit.endpoint, false, "empty=AWS, or https://...r2.cloudflarestorage.com");
                                form_row(ui, "Region", &mut edit.region, false, "us-east-1 / auto for R2");
                                form_row(ui, "Bucket (Root)", &mut edit.root, false, "");
                                form_row(ui, "Access key", &mut edit.username, false, "");
                                form_row(ui, "Secret key", &mut edit.secret, true, "");
                            }
                            DriveKind::Sftp => {
                                form_row(ui, "Host", &mut edit.endpoint, false, "");
                                let mut port = edit.port as i64;
                                ui.horizontal(|ui| { ui.label("Port"); ui.add(egui::DragValue::new(&mut port).range(0..=65535)); });
                                edit.port = port as u16;
                                form_row(ui, "Username", &mut edit.username, false, "");
                                form_row(ui, "Password", &mut edit.secret, true, "or key below");
                                form_row(ui, "SSH key path", &mut edit.ssh_key_path, false, "~/.ssh/id_ed25519");
                                form_row(ui, "Root path", &mut edit.root, false, "/home/user");
                            }
                            DriveKind::Ftp => {
                                form_row(ui, "Host", &mut edit.endpoint, false, "host or ftps://host");
                                let mut port = edit.port as i64;
                                ui.horizontal(|ui| { ui.label("Port"); ui.add(egui::DragValue::new(&mut port).range(0..=65535)); });
                                edit.port = port as u16;
                                form_row(ui, "Username", &mut edit.username, false, "");
                                form_row(ui, "Password", &mut edit.secret, true, "");
                                form_row(ui, "Root path", &mut edit.root, false, "");
                            }
                            DriveKind::Teldrive => {
                                form_row(ui, "Server URL", &mut edit.endpoint, false, "https://teldrive.example.com");
                                form_row(ui, "API key/token", &mut edit.secret, true, "api_key:XXXX or bearer token from teldrive Settings");
                                ui.label(egui::RichText::new("Self-hosted teldrive server required (Go+Postgres+Telegram login there).").color(theme::TEXT_FAINT).small());
                            }
                            DriveKind::Local => {
                                form_row(ui, "Folder", &mut edit.root, false, "C:\\data");
                            }
                        }
                        let mut streams = edit.streams as i64;
                        ui.horizontal(|ui| {
                            ui.label("Parallel streams");
                            ui.add(egui::DragValue::new(&mut streams).range(1..=16));
                            ui.label(egui::RichText::new("(ranged HTTP/S3/SFTP; others single)").color(theme::TEXT_FAINT).small());
                        });
                        edit.streams = streams as u32;
                        form_row(ui, "Crypt password", &mut edit.crypt_password, true, "rclone-crypt; empty=off");
                        form_row(ui, "Crypt salt", &mut edit.crypt_salt, true, "rclone password2; empty=default salt");
                        ui.horizontal(|ui| {
                            if ui::primary_button(ui, "Save account").clicked() {
                                save_edit = true;
                            }
                            if ui::secondary_button(ui, "Cancel").clicked() {
                                cancel_edit = true;
                            }
                        });
                    });
                }
                if save_edit {
                    if let Some(mut a) = self.edit.take() {
                        if a.kind == DriveKind::GoogleDrive {
                            a.team_drive = downloadcraft_lib::drive::gdrive::parse_drive_id(&a.team_drive);
                            // Legacy root alone handled by gdrive resolved_root probe.
                            a.root = downloadcraft_lib::drive::gdrive::parse_drive_id(&a.root);
                        }
                        let mut g = crate::lock_unpoison(app_rt);
                        if self.edit_is_new {
                            g.state.settings.drive_accounts.push(a);
                        } else if let Some(slot) = g.state.settings.drive_accounts.iter_mut().find(|x| x.id == a.id) {
                            *slot = a;
                        } else {
                            g.state.settings.drive_accounts.push(a);
                        }
                        g.save();
                        self.status = "Account saved.".into();
                    }
                }
                if cancel_edit {
                    self.edit = None;
                }
                if let Some(a) = fetch_shared_edit {
                    self.fetch_shared_drives(a);
                }
                if self.oauth_busy || self.pending_auth.is_some() {
                    let mut oauth_paste_click = false;
                    let mut oauth_cancel_click = false;
                    theme::panel().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Sign-in waiting").strong().color(theme::TEXT));
                        ui.label(egui::RichText::new("1) Browser opened — sign in there, this fills itself.").color(theme::TEXT_MUTED).small());
                        ui.horizontal(|ui| {
                            if ui::secondary_button(ui, "Open browser again").clicked() {
                                downloadcraft_lib::platform::open_url(&self.oauth_auth_url);
                            }
                            if ui::secondary_button(ui, "Copy sign-in link").clicked() {
                                copy_text(&self.oauth_auth_url.clone());
                            }
                            if ui::secondary_button(ui, "Cancel sign-in").clicked() {
                                oauth_cancel_click = true;
                            }
                        });
                        ui.label(egui::RichText::new("2) Stuck paste fallback: copy full address-bar URL after sign-in (127.0.0.1 page may fail to load — URL still works) or code, paste here.").color(theme::TEXT_MUTED).small());
                        ui.horizontal(|ui| {
                            ui.add(ui::text_field(&mut self.oauth_code_input).hint_text("http://127.0.0.1:53682/?code=...").desired_width((ui.available_width() - 230.0).max(140.0)));
                            if ui::secondary_button(ui, "Finish with pasted code").clicked() {
                                oauth_paste_click = true;
                            }
                        });
                    });
                    if oauth_paste_click {
                        self.oauth_paste_request = true;
                    }
                    if oauth_cancel_click {
                        self.oauth_cancel();
                    }
                }
                if self.oauth_paste_request {
                    self.oauth_paste_request = false;
                    self.oauth_paste_finish(app_rt);
                }
              }, |ui| {
                if ui::secondary_button(ui, "Close").clicked() {
                    close_clicked = true;
                }
              });
            });
        if modal.close || close_clicked {
            self.show_accounts = false;
            self.edit = None;
        }
        let _ = tokio_rt;
    }

    fn start_oauth(&mut self, app_rt: &Arc<Mutex<Runtime>>, acc: DriveAccount) {
        if self.oauth_busy {
            self.status = "Sign-in already waiting — paste code, or Cancel it first.".into();
            return;
        }
        let Some(app) = downloadcraft_lib::drive::oauth::preset(acc.kind, &acc) else {
            return;
        };
        if app.client_id.trim().is_empty() {
            self.status =
                "Paste Client ID + secret first (Setup steps in Edit), Save, then Connect.".into();
            self.edit = Some(acc);
            self.edit_is_new = false;
            return;
        }
        let pending = match downloadcraft_lib::drive::oauth::start(&app) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                self.status = e;
                return;
            }
        };
        self.oauth_auth_url = pending.auth_url.clone();
        self.oauth_code_input.clear();
        self.pending_auth = Some(pending.clone());
        self.oauth_busy = true;
        self.oauth_for = acc.id.clone();
        let slot = self.oauth_slot.clone();
        *lock(&slot) = None;
        let cancel = Arc::new(AtomicBool::new(false));
        self.oauth_cancel = Some(cancel.clone());
        let app_rt2 = app_rt.clone();
        let acc_id = acc.id.clone();
        std::thread::spawn(move || {
            let res = downloadcraft_lib::drive::oauth::wait_loopback(&pending, cancel, true)
                .and_then(|tok| Self::persist_tokens(&app_rt2, &acc_id, &tok));
            *lock(&slot) = Some(res);
        });
        self.status = "Browser opened. Sign in there. Paste fallback below if page stalls.".into();
    }

    fn oauth_paste_finish(&mut self, app_rt: &Arc<Mutex<Runtime>>) {
        let Some(pending) = self.pending_auth.clone() else {
            self.status = "No sign-in waiting. Click Connect first.".into();
            return;
        };
        if let Some(c) = &self.oauth_cancel {
            c.store(true, Ordering::Relaxed);
        }
        let pasted = self.oauth_code_input.clone();
        let slot = self.oauth_slot.clone();
        let app_rt2 = app_rt.clone();
        let acc_id = self.oauth_for.clone();
        self.status = "Exchanging pasted code…".into();
        std::thread::spawn(move || {
            let res = downloadcraft_lib::drive::oauth::finish(&pending, &pasted)
                .and_then(|tok| Self::persist_tokens(&app_rt2, &acc_id, &tok));
            *lock(&slot) = Some(res);
        });
    }

    fn oauth_cancel(&mut self) {
        if let Some(c) = &self.oauth_cancel {
            c.store(true, Ordering::Relaxed);
        }
        self.oauth_busy = false;
        self.pending_auth = None;
        self.oauth_cancel = None;
        *lock(&self.oauth_slot) = None;
        self.status = "Sign-in cancelled. Connect restarts it.".into();
    }

    fn sync_window(
        &mut self,
        ui: &mut egui::Ui,
        app_rt: &Arc<Mutex<Runtime>>,
        tokio_rt: &Arc<tokio::runtime::Runtime>,
    ) {
        let ctx = ui.ctx().clone();
        let mut close_clicked = false;
        let modal = ui::Modal::new("drive-sync", "Drive sync")
            .width(ui::size::MD)
            .height(700.0)
            .resizable(true)
            .show(&ctx, |ui| {
              ui::dialog_body_footer(ui, "drive-sync-body", |ui| {
                let settings: Settings = crate::lock_unpoison(app_rt).state.settings.clone();
                ui.label(egui::RichText::new("Compare folders, then copy/update/mirror. Mirror deletes destination extras — preview first.").color(theme::TEXT_MUTED).small());
                ui.add_space(theme::SP_SM);
                // Saved syncs
                let syncs = settings.drive_syncs.clone();
                for s in &syncs {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("{} · {}:{} -> {}:{} · {}", if s.name.is_empty() { "sync" } else { &s.name }, s.src_account, s.src_path, s.dst_account, s.dst_path, s.mode)).small());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui::secondary_button(ui, "Delete").clicked() {
                                let mut g = crate::lock_unpoison(app_rt);
                                g.state.settings.drive_syncs.retain(|x| x.id != s.id);
                                g.save();
                            }
                            if ui::secondary_button(ui, "Load").clicked() {
                                self.sync_src_account = s.src_account.clone();
                                self.sync_src_path = s.src_path.clone();
                                self.sync_dst_account = s.dst_account.clone();
                                self.sync_dst_path = s.dst_path.clone();
                                self.sync_mode = s.mode.clone();
                                self.sync_name = s.name.clone();
                                self.sync_preview = None;
                            }
                        });
                    });
                }
                ui.separator();
                form_row(ui, "Name", &mut self.sync_name, false, "");
                account_picker(ui, "Source account", &mut self.sync_src_account, &settings);
                form_row(ui, "Source path", &mut self.sync_src_path, false, "");
                account_picker(ui, "Dest account", &mut self.sync_dst_account, &settings);
                form_row(ui, "Dest path", &mut self.sync_dst_path, false, "");
                ui.horizontal(|ui| {
                    ui.label("Mode");
                    egui::ComboBox::from_id_salt("sync-mode")
                        .selected_text(self.sync_mode.clone())
                        .show_ui(ui, |ui| {
                            for m in ["copy", "update", "mirror"] {
                                ui.selectable_value(&mut self.sync_mode, m.to_string(), m);
                            }
                        });
                    ui.label(egui::RichText::new("copy=missing, update=newer/bigger, mirror=+delete extras").color(theme::TEXT_FAINT).small());
                });
                // poll preview
                let done = self.sync_slot.as_ref().and_then(|s| lock(s).take());
                if let Some(res) = done {
                    match res {
                        Ok((src_files, dst_files)) => {
                            let p = sync_plan(
                                SyncMode::parse(&self.sync_mode),
                                &src_files,
                                &self.sync_src_path,
                                &dst_files,
                                &self.sync_dst_path,
                            );
                            self.status = format!("Preview: {} to copy, {} to delete, {} unchanged", p.to_copy.len(), p.to_delete.len(), p.unchanged);
                            // stash plan entries in preview job via status; keep plan
                            self.sync_preview = Some(p);
                        }
                        Err(e) => self.status = format!("Sync preview: {e}"),
                    }
                    self.sync_slot = None;
                }
                ui.horizontal(|ui| {
                    if ui::secondary_button(ui, "Preview compare").clicked() {
                        let slot: SyncSlot = Arc::new(Mutex::new(None));
                        self.sync_slot = Some(slot.clone());
                        self.sync_preview = None;
                        let (sa, sp, da, dp) = (self.sync_src_account.clone(), self.sync_src_path.clone(), self.sync_dst_account.clone(), self.sync_dst_path.clone());
                        let settings2 = settings.clone();
                        tokio_rt.spawn(async move {
                            let r = async {
                                let (_, sbe) = Self::backend(&settings2, &sa)?;
                                let (_, dbe) = Self::backend(&settings2, &da)?;
                                let sf = collect_at(&sbe, &sp).await?;
                                let df = collect_at(&dbe, &dp).await?;
                                Ok::<_, String>((sf, df))
                            }
                            .await;
                            *lock(&slot) = Some(r);
                        });
                        self.status = "Comparing…".into();
                    }
                    let can_run = self.sync_preview.is_some();
                    // Run deletes destination extras when the plan has any:
                    // destructive styling, consequence listed in preview below.
                    let deletes = self.sync_preview.as_ref().is_some_and(|p| !p.to_delete.is_empty());
                    ui.add_enabled_ui(can_run, |ui| {
                        let kind = if deletes { ui::ButtonKind::Destructive } else { ui::ButtonKind::Primary };
                        if ui::Btn::new(kind, "Run sync").show(ui).clicked() {
                            if let Some(p) = self.sync_preview.take() {
                                let (sa, da, dp) = (self.sync_src_account.clone(), self.sync_dst_account.clone(), self.sync_dst_path.clone());
                                let settings2 = settings.clone();
                                let src_streams = account_by_id(&settings, &self.sync_src_account).map(|a| a.streams.max(1)).unwrap_or(4);
                                let prog = Progress::new();
                                let prog2 = prog.clone();
                                let del = p.to_delete.clone();
                                // rebase to_copy paths: entries carry src paths; dst dir = dst_path + rel handled in run via Copy into dst root preserving basename only for listed root files.
                                // Planner entries are files with full src path; copy_file keeps name into given dst_dir: need rel subdir.
                                let src_root = self.sync_src_path.clone();
                                let dst_root = dp.clone();
                                let copies = p.to_copy.clone();
                                let src_streams2 = src_streams;
                                self.spawn_job(tokio_rt, format!("Sync {} file(s)", copies.len()), async move {
                                    let (_, sbe) = Self::backend(&settings2, &sa)?;
                                    let (_, dbe) = Self::backend(&settings2, &da)?;
                                    prog2.set_total(copies.iter().map(|e| e.size).sum());
                                    for e in &copies {
                                        prog2.check()?;
                                        let rel = e.path.trim_matches('/').strip_prefix(src_root.trim_matches('/')).unwrap_or("").trim_matches('/');
                                        let parent = downloadcraft_lib::drive::parent_path(rel);
                                        let ddir = if parent.is_empty() { dst_root.clone() } else { downloadcraft_lib::drive::join_path(&dst_root, &parent) };
                                        jobs::copy_file(&sbe, e, &dbe, &ddir, src_streams2, &prog2).await?;
                                    }
                                    for e in &del {
                                        dbe.delete(e).await?;
                                    }
                                    Ok(format!("sync done, {} deleted", del.len()))
                                }, prog);
                            }
                        }
                    });
                    if ui::secondary_button(ui, "Save sync").clicked() {
                        let mut g = crate::lock_unpoison(app_rt);
                        g.state.settings.drive_syncs.push(DriveSync {
                            id: downloadcraft_lib::model::new_id(),
                            name: self.sync_name.clone(),
                            src_account: self.sync_src_account.clone(),
                            src_path: self.sync_src_path.clone(),
                            dst_account: self.sync_dst_account.clone(),
                            dst_path: self.sync_dst_path.clone(),
                            mode: self.sync_mode.clone(),
                        });
                        g.save();
                        self.status = "Sync saved.".into();
                    }
                });
                if let Some(p) = &self.sync_preview {
                    ui.add_space(theme::SP_SM);
                    ui.label(egui::RichText::new(format!("Copy {} file(s) · {} bytes · delete {} · unchanged {}", p.to_copy.len(), fmt_bytes(p.to_copy.iter().map(|e| e.size).sum()), p.to_delete.len(), p.unchanged)).small());
                    egui::ScrollArea::vertical().id_salt("sync-preview").max_height(260.0).show(ui, |ui| {
                        for e in p.to_copy.iter().take(200) {
                            ui.label(egui::RichText::new(format!("copy  {} ({})", e.path, fmt_bytes(e.size))).small().color(theme::TEXT_MUTED));
                        }
                        for e in p.to_delete.iter().take(200) {
                            ui.label(egui::RichText::new(format!("DELETE {}", e.path)).small().color(theme::ERR));
                        }
                    });
                }
              }, |ui| {
                if ui::secondary_button(ui, "Close").clicked() {
                    close_clicked = true;
                }
              });
            });
        if modal.close || close_clicked {
            self.show_sync = false;
        }
    }
}

enum PaneAction {
    Refresh(usize),
    Navigate(usize, String),
    AccountChanged(usize, String),
    Copy(usize),
    Move(usize),
    ServerCopy(usize),
    Delete(usize),
    Mkdir(usize),
    Rename(usize),
    DownloadTask(usize),
    Play(usize, String, Vec<(String, String)>),
    StreamPlay(usize, RemoteEntry),
}

async fn collect_at(be: &Arc<dyn DriveBackend>, root: &str) -> Result<Vec<RemoteEntry>, String> {
    // Root listing then recursive files.
    let root_entry = RemoteEntry {
        id: root.to_string(),
        name: downloadcraft_lib::drive::base_name(root),
        path: root.to_string(),
        is_dir: true,
        size: 0,
        modified: 0,
        mime: String::new(),
    };
    jobs::collect_files(be, &root_entry).await
}

fn kind_hint(k: DriveKind) -> &'static str {
    match k {
        DriveKind::GoogleDrive => "OAuth, Shared Drives picker",
        DriveKind::OneDrive => "OAuth, personal/work",
        DriveKind::Dropbox => "OAuth, App key",
        DriveKind::WebDav => "Nextcloud etc, app password",
        DriveKind::S3 => "AWS/R2/B2/MinIO keys",
        DriveKind::Sftp => "SSH host, key/password",
        DriveKind::Ftp => "FTP/FTPS host",
        DriveKind::Teldrive => "your teldrive server",
        DriveKind::Local => "folder on this PC",
    }
}

fn setup_panel(ui: &mut egui::Ui, kind: DriveKind) {
    let (console, steps) = downloadcraft_lib::drive::setup_steps(kind);
    theme::panel().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Setup steps")
                    .strong()
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new(console)
                    .color(theme::STEEL_TEXT)
                    .small(),
            );
            if matches!(
                kind,
                DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox
            ) && ui::secondary_button(ui, "Copy redirect URI").clicked()
            {
                copy_text(downloadcraft_lib::drive::oauth::REDIRECT_URI);
            }
        });
        for (i, s) in steps.iter().enumerate() {
            ui.label(
                egui::RichText::new(format!("{}. {s}", i + 1))
                    .color(theme::TEXT_MUTED)
                    .small(),
            );
        }
        if matches!(
            kind,
            DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox
        ) {
            ui.label(
                egui::RichText::new(format!(
                    "Redirect to register (exact): {}",
                    downloadcraft_lib::drive::oauth::REDIRECT_URI
                ))
                .color(theme::TEXT_FAINT)
                .small(),
            );
        }
    });
}

fn credential_fields(ui: &mut egui::Ui, edit: &mut DriveAccount, in_wizard: bool) {
    match edit.kind {
        DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox => {
            form_row(
                ui,
                "Client ID",
                &mut edit.client_id,
                false,
                "client id / Azure app ID / Dropbox App key",
            );
            form_row(
                ui,
                "Client secret",
                &mut edit.client_secret,
                true,
                "Value shown once at creation",
            );
            if edit.kind != DriveKind::GoogleDrive && !in_wizard {
                form_row(
                    ui,
                    "Root (optional)",
                    &mut edit.root,
                    false,
                    "folder prefix / empty",
                );
            }
        }
        DriveKind::WebDav => {
            form_row(
                ui,
                "URL",
                &mut edit.endpoint,
                false,
                "https://host/remote.php/dav/files/user/",
            );
            form_row(ui, "Username", &mut edit.username, false, "");
            form_row(ui, "Password/app-pw", &mut edit.secret, true, "");
        }
        DriveKind::S3 => {
            form_row(
                ui,
                "Endpoint",
                &mut edit.endpoint,
                false,
                "empty=AWS; https://<acct>.r2.cloudflarestorage.com",
            );
            form_row(
                ui,
                "Region",
                &mut edit.region,
                false,
                "auto (R2) / us-east-1",
            );
            if !in_wizard {
                form_row(ui, "Bucket (Root)", &mut edit.root, false, "");
            }
            form_row(ui, "Access key", &mut edit.username, false, "");
            form_row(ui, "Secret key", &mut edit.secret, true, "");
        }
        DriveKind::Sftp => {
            form_row(ui, "Host", &mut edit.endpoint, false, "");
            let mut port = edit.port as i64;
            ui.horizontal(|ui| {
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut port).range(0..=65535));
            });
            edit.port = port as u16;
            form_row(ui, "Username", &mut edit.username, false, "");
            form_row(
                ui,
                "Password",
                &mut edit.secret,
                true,
                "key passphrase if key below",
            );
            form_row(
                ui,
                "SSH key path",
                &mut edit.ssh_key_path,
                false,
                "~/.ssh/id_ed25519 (not .pub)",
            );
            if !in_wizard {
                form_row(ui, "Root path", &mut edit.root, false, "/home/user");
            }
        }
        DriveKind::Ftp => {
            form_row(ui, "Host", &mut edit.endpoint, false, "host or ftps://host");
            let mut port = edit.port as i64;
            ui.horizontal(|ui| {
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut port).range(0..=65535));
            });
            edit.port = port as u16;
            form_row(ui, "Username", &mut edit.username, false, "");
            form_row(ui, "Password", &mut edit.secret, true, "");
            if !in_wizard {
                form_row(ui, "Root path", &mut edit.root, false, "");
            }
        }
        DriveKind::Teldrive => {
            form_row(
                ui,
                "Server URL",
                &mut edit.endpoint,
                false,
                "https://teldrive.example.com",
            );
            form_row(
                ui,
                "API key/token",
                &mut edit.secret,
                true,
                "api_key:XXXX from teldrive Settings > API Keys",
            );
        }
        DriveKind::Local => {
            form_row(ui, "Folder", &mut edit.root, false, "C:\\data");
            edit.endpoint = edit.root.clone();
        }
    }
    if !in_wizard {
        let mut streams = edit.streams as i64;
        ui.horizontal(|ui| {
            ui.label("Parallel streams");
            ui.add(egui::DragValue::new(&mut streams).range(1..=16));
            ui.label(
                egui::RichText::new(
                    "files at once + segments per file (rclone --transfers / multi-thread)",
                )
                .color(theme::TEXT_FAINT)
                .small(),
            );
        });
        edit.streams = streams as u32;
        form_row(
            ui,
            "Crypt password",
            &mut edit.crypt_password,
            true,
            "rclone-crypt; empty=off",
        );
        form_row(
            ui,
            "Crypt salt",
            &mut edit.crypt_salt,
            true,
            "rclone password2; empty=default salt",
        );
    }
}

fn details_valid(a: &DriveAccount) -> bool {
    if a.name.trim().is_empty() {
        return false;
    }
    match a.kind {
        DriveKind::GoogleDrive | DriveKind::OneDrive | DriveKind::Dropbox => {
            !a.client_id.trim().is_empty()
        }
        DriveKind::WebDav | DriveKind::Teldrive => !a.endpoint.trim().is_empty(),
        DriveKind::S3 => !a.username.trim().is_empty() && !a.secret.is_empty(),
        DriveKind::Sftp | DriveKind::Ftp => {
            !a.endpoint.trim().is_empty() && !a.username.trim().is_empty()
        }
        DriveKind::Local => !a.root.trim().is_empty(),
    }
}

fn root_valid(a: &DriveAccount) -> bool {
    match a.kind {
        DriveKind::S3 => !a.root.trim().is_empty(),
        DriveKind::Local => !a.root.trim().is_empty(),
        _ => true,
    }
}

fn review_rows(a: &DriveAccount) -> Vec<(&'static str, String)> {
    let root = match a.kind {
        DriveKind::GoogleDrive => {
            let team = if a.team_drive.is_empty() {
                "My Drive"
            } else {
                &a.team_drive
            };
            if a.root.is_empty() || a.root == a.team_drive {
                format!("{team} (root)")
            } else {
                format!("{team} / folder {}", a.root)
            }
        }
        _ if a.root.is_empty() => "(account root)".to_string(),
        _ => a.root.clone(),
    };
    vec![
        ("Type", a.kind.label().to_string()),
        ("Name", a.name.clone()),
        (
            "Endpoint",
            if a.endpoint.is_empty() {
                "—".into()
            } else {
                a.endpoint.clone()
            },
        ),
        ("Root", root),
        (
            "Signed in",
            if a.refresh_token.is_empty() {
                "credentials entered".into()
            } else {
                "token saved at Finish".into()
            },
        ),
    ]
}

fn account_picker(ui: &mut egui::Ui, label: &str, cur: &mut String, settings: &Settings) {
    ui.horizontal(|ui| {
        ui.label(label);
        let text = account_by_id(settings, cur)
            .map(|a| a.display())
            .unwrap_or_else(|| "pick".into());
        egui::ComboBox::from_id_salt(label)
            .selected_text(text)
            .show_ui(ui, |ui| {
                ui.selectable_value(cur, LOCAL_ID.to_string(), "This PC");
                for a in &settings.drive_accounts {
                    ui.selectable_value(cur, a.id.clone(), a.display());
                }
            });
    });
}

fn form_row(ui: &mut egui::Ui, label: &str, val: &mut String, password: bool, hint: &str) {
    ui.horizontal(|ui| {
        ui::form_label(ui, label);
        let mut t = ui::text_field(val).desired_width(ui.available_width().max(120.0));
        if password {
            t = t.password(true);
        }
        if !hint.is_empty() {
            t = t.hint_text(hint);
        }
        ui.add(t);
    });
}

fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn copy_text(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

pub fn fmt_bytes(n: u64) -> String {
    downloadcraft_lib::drive::fmt_bytes(n)
}
