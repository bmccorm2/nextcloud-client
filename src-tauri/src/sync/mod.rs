#[cfg(test)]
mod e2e;
pub mod execute;
pub mod local;
pub mod reconcile;
pub mod remote;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at};

use crate::db::Db;
use crate::ignore::is_ignored;
use crate::webdav::{Dav, DavError};
use reconcile::Action;

const POLL: Duration = Duration::from_secs(300);
const FULL_SYNC: Duration = Duration::from_secs(3600);
const DEBOUNCE: Duration = Duration::from_secs(2);
const MAX_RERUNS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    NotConfigured,
    Idle,
    Syncing { done: usize, total: usize },
    Paused,
    Offline,
    LoginRequired,
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusDto {
    pub kind: &'static str,
    pub text: String,
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::NotConfigured => "Not configured".into(),
            Status::Idle => "Up to date".into(),
            Status::Syncing { total: 0, .. } => "Syncing…".into(),
            Status::Syncing { done, total } => format!("Syncing… {done}/{total}"),
            Status::Paused => "Paused".into(),
            Status::Offline => "Offline".into(),
            Status::LoginRequired => "Login required".into(),
            Status::Error(e) => e.clone(),
        }
    }

    pub fn dto(&self) -> StatusDto {
        let kind = match self {
            Status::NotConfigured => "not_configured",
            Status::Idle => "idle",
            Status::Syncing { .. } => "syncing",
            Status::Paused => "paused",
            Status::Offline => "offline",
            Status::LoginRequired => "login_required",
            Status::Error(_) => "error",
        };
        StatusDto {
            kind,
            text: self.label(),
        }
    }
}

pub struct EngineParams {
    pub dav: Arc<Dav>,
    pub db: Arc<Mutex<Db>>,
    pub root: PathBuf,
    pub paused: Arc<AtomicBool>,
    pub status: Arc<dyn Fn(Status) + Send + Sync>,
    /// Persists the paused flag (and updates the tray).
    pub set_paused: Arc<dyn Fn(bool) + Send + Sync>,
    /// Blocking yes/no dialog.
    pub confirm: Arc<dyn Fn(String) -> bool + Send + Sync>,
    /// What the current run is doing, one item per line. Empty when idle.
    pub activity: Arc<dyn Fn(String) + Send + Sync>,
}

pub struct Engine {
    tx: UnboundedSender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Engine {
    pub fn start(p: EngineParams) -> Engine {
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(main_loop(Arc::new(p), rx));
        Engine { tx, task }
    }

    pub fn sync_now(&self) {
        let _ = self.tx.send(());
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Aborting drops in-flight transfers; leftover .part files are cleaned on the next scan.
        self.task.abort();
    }
}

type Watcher = Debouncer<RecommendedWatcher, RecommendedCache>;

fn relevant(root: &Path, ev: &DebouncedEvent) -> bool {
    if matches!(ev.kind, EventKind::Access(_)) {
        return false;
    }
    ev.paths.iter().any(|p| match p.strip_prefix(root) {
        Ok(rel) => !rel
            .components()
            .any(|c| is_ignored(&c.as_os_str().to_string_lossy())),
        Err(_) => true,
    })
}

fn watch(root: &Path, tx: UnboundedSender<()>) -> anyhow::Result<Watcher> {
    let canon = root.canonicalize()?;
    let mut d = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        if let Ok(events) = res
            && events.iter().any(|e| relevant(&canon, e))
        {
            let _ = tx.send(());
        }
    })?;
    d.watch(root, RecursiveMode::Recursive)?;
    Ok(d)
}

struct RunResult {
    /// Something is left to do; the next poll should sync even if the root etag is unchanged.
    dirty: bool,
    rerun: bool,
    root_missing: bool,
    unauthorized: bool,
}

async fn main_loop(p: Arc<EngineParams>, mut rx: UnboundedReceiver<()>) {
    let (ev_tx, mut ev_rx) = unbounded_channel::<()>();
    let mut watcher: Option<Watcher> = None;
    let mut poll = interval_at(Instant::now() + POLL, POLL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut hourly = interval(FULL_SYNC); // first tick fires at once: initial full sync
    hourly.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut dirty = true;
    // After a 401, only `Sync now` retries (repeated bad logins trigger server throttling).
    let mut unauthorized = false;

    loop {
        let mut full = false;
        tokio::select! {
            _ = hourly.tick() => {
                if unauthorized {
                    continue;
                }
                full = true;
            }
            _ = poll.tick() => {
                if unauthorized || p.paused.load(Ordering::SeqCst) {
                    continue;
                }
                match should_sync(&p, dirty).await {
                    Poll::Sync => {}
                    Poll::Skip => continue,
                    // Sync once the network is back, so the status leaves "Offline".
                    Poll::Offline => {
                        dirty = true;
                        continue;
                    }
                    Poll::Unauthorized => {
                        unauthorized = true;
                        continue;
                    }
                }
            }
            Some(()) = rx.recv() => {}
            Some(()) = ev_rx.recv() => {
                if unauthorized {
                    continue;
                }
            }
        }

        let mut runs = 0;
        loop {
            if watcher.is_none() && p.root.is_dir() {
                match watch(&p.root, ev_tx.clone()) {
                    Ok(w) => watcher = Some(w),
                    Err(e) => tracing::error!("cannot watch {}: {e}", p.root.display()),
                }
            }
            let res = run_once(&p, full).await;
            (p.activity)(String::new());
            if res.root_missing {
                watcher = None;
            }
            dirty = res.dirty;
            unauthorized = res.unauthorized;
            runs += 1;
            let mut again = res.rerun;
            while rx.try_recv().is_ok() {
                again = true;
            }
            while ev_rx.try_recv().is_ok() {
                again = true;
            }
            if !again || unauthorized || runs >= MAX_RERUNS || p.paused.load(Ordering::SeqCst) {
                break;
            }
            full = false;
        }
    }
}

enum Poll {
    Sync,
    Skip,
    Offline,
    Unauthorized,
}

/// Poll check: Depth-0 PROPFIND of the root.
async fn should_sync(p: &EngineParams, dirty: bool) -> Poll {
    if dirty {
        return Poll::Sync;
    }
    match p.dav.root_etag().await {
        Ok(etag) => {
            let stored = p.db.lock().unwrap().get_meta("root_etag").ok().flatten();
            if stored.as_deref() == Some(etag.as_str()) {
                Poll::Skip
            } else {
                Poll::Sync
            }
        }
        Err(DavError::Unauthorized) => {
            (p.status)(Status::LoginRequired);
            Poll::Unauthorized
        }
        Err(DavError::Network(e)) => {
            tracing::warn!("poll: {e}");
            (p.status)(Status::Offline);
            Poll::Offline
        }
        Err(e) => {
            tracing::warn!("poll: {e}");
            Poll::Skip
        }
    }
}

fn dav_status(e: &DavError) -> Status {
    match e {
        DavError::Unauthorized => Status::LoginRequired,
        DavError::Network(_) => Status::Offline,
        e => Status::Error(format!("Server error: {e}")),
    }
}

async fn run_once(p: &EngineParams, full: bool) -> RunResult {
    let mut res = RunResult {
        dirty: true,
        rerun: false,
        root_missing: false,
        unauthorized: false,
    };
    if p.paused.load(Ordering::SeqCst) {
        (p.status)(Status::Paused);
        return res;
    }
    if !p.root.is_dir() {
        tracing::error!("sync folder missing: {}", p.root.display());
        (p.status)(Status::Error("Sync folder missing".into()));
        res.root_missing = true;
        return res;
    }
    (p.status)(Status::Syncing { done: 0, total: 0 });
    tracing::info!("sync start (full={full})");

    let base: BTreeMap<_, _> = match p.db.lock().unwrap().all() {
        Ok(rows) => rows.into_iter().map(|e| (e.path.clone(), e)).collect(),
        Err(e) => {
            (p.status)(Status::Error(format!("Database error: {e}")));
            return res;
        }
    };
    (p.activity)("Scanning local folder…".into());
    let root = p.root.clone();
    let local = match tokio::task::spawn_blocking(move || local::scan(&root)).await {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => {
            tracing::error!("local scan: {e}");
            (p.status)(Status::Error(format!("Cannot read sync folder: {e}")));
            return res;
        }
        Err(e) => {
            (p.status)(Status::Error(e.to_string()));
            return res;
        }
    };
    let act = p.activity.clone();
    let on_dir = move |d: &str| act(format!("Checking server: /{d}"));
    let remote = match remote::scan(&p.dav, &base, full, &on_dir).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("remote scan: {e}");
            res.unauthorized = matches!(e, DavError::Unauthorized);
            (p.status)(dav_status(&e));
            return res;
        }
    };
    // Re-check: the folder may have been moved while scanning.
    if !p.root.is_dir() {
        (p.status)(Status::Error("Sync folder missing".into()));
        res.root_missing = true;
        return res;
    }

    // Leave folders the server could not list completely alone.
    let mut local = local;
    let mut remote = remote;
    let mut base = base;
    let skipped: Vec<String> = remote.skipped.iter().map(|(d, _)| d.clone()).collect();
    if !skipped.is_empty() {
        let hit = |k: &str| skipped.iter().any(|d| execute::is_under(k, d));
        local.entries.retain(|k, _| !hit(k));
        remote.entries.retain(|k, _| !hit(k));
        base.retain(|k, _| !hit(k));
    }

    let plan = reconcile::plan(&local.entries, &remote.entries, &base);
    if !plan.is_empty() {
        tracing::info!("plan: {} actions", plan.len());
    }

    // Safety: never wipe a whole side without asking.
    if !base.is_empty() {
        let has = |a: Action| plan.iter().any(|(_, x)| *x == a);
        let side = if has(Action::DeleteRemote)
            && base.keys().all(|k| !local.entries.contains_key(k))
        {
            Some("the server")
        } else if has(Action::TrashLocal) && base.keys().all(|k| !remote.entries.contains_key(k)) {
            Some("this Mac")
        } else {
            None
        };
        if let Some(side) = side {
            tracing::warn!("run would delete every tracked file on {side}; asking");
            (p.set_paused)(true);
            (p.status)(Status::Paused);
            let confirm = p.confirm.clone();
            let msg = format!("All files would be deleted on {side}. Continue?");
            let ok = tokio::task::spawn_blocking(move || confirm(msg))
                .await
                .unwrap_or(false);
            if !ok {
                tracing::warn!("mass delete declined; staying paused");
                return res;
            }
            (p.set_paused)(false);
        }
    }

    let status = p.status.clone();
    let exec = Arc::new(execute::Exec::new(
        p.dav.clone(),
        p.db.clone(),
        p.root.clone(),
        local,
        remote.entries,
        base,
        p.paused.clone(),
        Box::new(move |done, total| status(Status::Syncing { done, total })),
        p.activity.clone(),
    ));
    exec.mark_failed(skipped.iter().cloned());
    let out = exec.run(plan).await;

    if out.failed.is_empty() {
        let _ =
            p.db.lock()
                .unwrap()
                .set_meta("root_etag", &remote.root_etag);
    }
    let status = if out.unauthorized {
        Status::LoginRequired
    } else if p.paused.load(Ordering::SeqCst) {
        Status::Paused
    } else if out.network_errors > 0 {
        Status::Offline
    } else if let Some((dir, e)) = remote.skipped.first() {
        Status::Error(format!("Cannot list “{dir}” on server ({e})"))
    } else if out.other_errors > 0 {
        let n = out.other_errors;
        Status::Error(format!(
            "{n} item{} failed, see log",
            if n == 1 { "" } else { "s" }
        ))
    } else {
        Status::Idle
    };
    tracing::info!("sync done: {}", status.label());
    (p.status)(status);
    res.dirty = !out.failed.is_empty() || out.rerun;
    res.rerun = out.rerun;
    res.unauthorized = out.unauthorized;
    res
}
