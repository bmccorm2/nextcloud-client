use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use filetime::FileTime;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::db::{Db, DbEntry};
use crate::sync::local::{self, LocalEntry, LocalScan};
use crate::sync::reconcile::Action;
use crate::webdav::{CHUNK_SIZE, Cond, Dav, DavError, DavResult, RemoteEntry};

const MAX_TRANSFERS: usize = 4;

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// Paths that failed or were skipped.
    pub failed: Vec<String>,
    pub network_errors: usize,
    pub other_errors: usize,
    pub unauthorized: bool,
    /// Something changed during the run; sync again.
    pub rerun: bool,
}

pub struct Exec {
    pub dav: Arc<Dav>,
    pub db: Arc<Mutex<Db>>,
    pub root: PathBuf,
    pub local: LocalScan,
    pub remote: HashMap<String, RemoteEntry>,
    pub base: BTreeMap<String, DbEntry>,
    pub paused: Arc<AtomicBool>,
    pub progress: Box<dyn Fn(usize, usize) + Send + Sync>,
    pub activity: Arc<dyn Fn(String) + Send + Sync>,
    /// In-flight items: (path, description).
    active: Mutex<Vec<(String, String)>>,
    done: AtomicUsize,
    total: AtomicUsize,
    unauthorized: AtomicBool,
    rerun: AtomicBool,
    network_errors: AtomicUsize,
    other_errors: AtomicUsize,
    failed: Mutex<Vec<String>>,
}

/// `<stem> (conflicted copy YYYY-MM-DD HHMMSS)<.ext>`; folders and dot-files keep the whole name as stem.
pub fn conflict_name(name: &str, stamp: &str, is_dir: bool) -> String {
    let split = if is_dir {
        None
    } else {
        name.rfind('.').filter(|&i| i > 0)
    };
    let (stem, ext) = match split {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    format!("{stem} (conflicted copy {stamp}){ext}")
}

fn conflict_path(path: &Path, is_dir: bool) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S").to_string();
    let mut candidate = path.with_file_name(conflict_name(&name, &stamp, is_dir));
    let mut n = 2;
    while candidate.symlink_metadata().is_ok() {
        candidate = path.with_file_name(conflict_name(&name, &format!("{stamp} {n}"), is_dir));
        n += 1;
    }
    candidate
}

pub fn is_under(path: &str, dir: &str) -> bool {
    path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

fn human_size(n: i64) -> String {
    let (mut v, mut unit) = (n as f64, "B");
    for u in ["KB", "MB", "GB", "TB"] {
        if v < 1000.0 {
            break;
        }
        v /= 1000.0;
        unit = u;
    }
    if unit == "B" {
        format!("{n} B")
    } else {
        format!("{v:.1} {unit}")
    }
}

fn depth(p: &str) -> usize {
    p.matches('/').count()
}

impl Exec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dav: Arc<Dav>,
        db: Arc<Mutex<Db>>,
        root: PathBuf,
        local: LocalScan,
        remote: HashMap<String, RemoteEntry>,
        base: BTreeMap<String, DbEntry>,
        paused: Arc<AtomicBool>,
        progress: Box<dyn Fn(usize, usize) + Send + Sync>,
        activity: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Exec {
        Exec {
            dav,
            db,
            root,
            local,
            remote,
            base,
            paused,
            progress,
            activity,
            active: Mutex::new(Vec::new()),
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            unauthorized: AtomicBool::new(false),
            rerun: AtomicBool::new(false),
            network_errors: AtomicUsize::new(0),
            other_errors: AtomicUsize::new(0),
            failed: Mutex::new(Vec::new()),
        }
    }

    fn db<R>(&self, f: impl FnOnce(&Db) -> rusqlite::Result<R>) -> DavResult<R> {
        let db = self.db.lock().unwrap();
        f(&db).map_err(|e| DavError::Io(std::io::Error::other(e)))
    }

    /// Real on-disk path for an NFC relative path.
    fn local_path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            return self.root.clone();
        }
        if let Some(p) = self.local.real.get(rel) {
            return p.clone();
        }
        match rel.rsplit_once('/') {
            Some((parent, name)) => self.local_path(parent).join(name),
            None => self.root.join(rel),
        }
    }

    fn stopped(&self) -> bool {
        self.paused.load(Ordering::SeqCst) || self.unauthorized.load(Ordering::SeqCst)
    }

    fn fail(&self, path: &str) {
        self.failed.lock().unwrap().push(path.to_string());
    }

    fn show_active(&self, active: &[(String, String)]) {
        let lines: Vec<&str> = active.iter().map(|(_, l)| l.as_str()).collect();
        (self.activity)(lines.join("\n"));
    }

    fn begin(&self, p: &str, action: Action) {
        let size = |s: Option<i64>| s.map(|n| format!(" ({})", human_size(n))).unwrap_or_default();
        let line = match action {
            Action::Upload => format!(
                "Uploading {p}{}",
                size(self.local.entries.get(p).map(|l| l.size))
            ),
            Action::Download => format!(
                "Downloading {p}{}",
                size(self.remote.get(p).map(|r| r.size))
            ),
            Action::Conflict => format!("Resolving conflict: {p}"),
            Action::MkdirLocal | Action::MkdirRemote => format!("Creating folder {p}"),
            Action::DeleteRemote => format!("Deleting on server: {p}"),
            Action::TrashLocal => format!("Moving to Trash: {p}"),
            Action::RecordOnly | Action::ForgetBase | Action::None => return,
        };
        let mut active = self.active.lock().unwrap();
        active.push((p.to_string(), line));
        self.show_active(&active);
    }

    fn finish(&self, path: &str, action: Action, res: DavResult<()>) {
        {
            let mut active = self.active.lock().unwrap();
            if let Some(i) = active.iter().position(|(p, _)| p == path) {
                active.remove(i);
                self.show_active(&active);
            }
        }
        if let Err(e) = res {
            tracing::warn!("{action:?} {path} failed: {e}");
            self.fail(path);
            match e {
                DavError::Unauthorized => self.unauthorized.store(true, Ordering::SeqCst),
                DavError::Network(_) => {
                    self.network_errors.fetch_add(1, Ordering::SeqCst);
                }
                DavError::PreconditionFailed => {
                    self.rerun.store(true, Ordering::SeqCst);
                    self.other_errors.fetch_add(1, Ordering::SeqCst);
                }
                _ => {
                    self.other_errors.fetch_add(1, Ordering::SeqCst);
                }
            }
        } else if !matches!(action, Action::RecordOnly | Action::ForgetBase) {
            tracing::info!("{action:?} {path}");
        }
        let done = self.done.fetch_add(1, Ordering::SeqCst) + 1;
        (self.progress)(done, self.total.load(Ordering::SeqCst));
    }

    /// Runs the plan: folders (parents first), transfers (4 at once), deletions (children first).
    /// Marks paths as failed up front (e.g. folders the server could not list), so parent folder
    /// etags are not recorded and the next run looks at them again.
    pub fn mark_failed(&self, paths: impl IntoIterator<Item = String>) {
        self.failed.lock().unwrap().extend(paths);
    }

    pub async fn run(self: Arc<Self>, plan: Vec<(String, Action)>) -> Outcome {
        self.total.store(plan.len(), Ordering::SeqCst);
        let acts: HashMap<String, Action> = plan.iter().cloned().collect();

        let mut phase1 = Vec::new();
        let mut transfers = Vec::new();
        let mut deletes = Vec::new();
        for (p, a) in plan {
            let type_conflict = a == Action::Conflict
                && self.local.entries.get(&p).map(|l| l.is_dir)
                    != self.remote.get(&p).map(|r| r.is_dir);
            match a {
                Action::MkdirLocal | Action::MkdirRemote | Action::RecordOnly => {
                    phase1.push((p, a))
                }
                Action::Conflict if type_conflict => phase1.push((p, a)),
                Action::Upload | Action::Download | Action::Conflict => {
                    transfers.push((p, a, false))
                }
                Action::DeleteRemote | Action::TrashLocal | Action::ForgetBase => {
                    deletes.push((p, a))
                }
                Action::None => {}
            }
        }

        // 1. Folders, parents first.
        phase1.sort_by_key(|(p, _)| depth(p));
        for (p, a) in phase1 {
            if self.stopped() {
                self.fail(&p);
                continue;
            }
            self.begin(&p, a);
            let res = match a {
                Action::MkdirLocal => self.mkdir_local(&p).await,
                Action::MkdirRemote => self.mkdir_remote(&p).await,
                Action::RecordOnly => self.record(&p),
                _ => match self.type_conflict(&p).await {
                    // Remote is a file: download it after the local folder was moved aside.
                    Ok(true) => {
                        transfers.push((p.clone(), Action::Download, true));
                        Ok(())
                    }
                    r => r.map(|_| ()),
                },
            };
            self.finish(&p, a, res);
        }

        // 2. Transfers.
        let sem = Arc::new(Semaphore::new(MAX_TRANSFERS));
        let mut set = JoinSet::new();
        for (p, a, after_type_conflict) in transfers {
            let me = self.clone();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                if me.stopped() {
                    me.fail(&p);
                    return;
                }
                me.begin(&p, a);
                let res = match a {
                    Action::Upload => me.upload(&p).await,
                    Action::Download if after_type_conflict => me.download(&p, false, None).await,
                    Action::Download => {
                        me.download(&p, false, me.local.entries.get(&p).copied())
                            .await
                    }
                    _ => {
                        me.download(&p, true, me.local.entries.get(&p).copied())
                            .await
                    }
                };
                me.finish(&p, a, res);
            });
        }
        while set.join_next().await.is_some() {}

        // 3. Deletions, children first.
        deletes.sort_by_key(|(p, _)| std::cmp::Reverse(depth(p)));
        for (p, a) in deletes {
            if self.stopped() {
                self.fail(&p);
                continue;
            }
            self.begin(&p, a);
            let res = match a {
                Action::DeleteRemote => self.delete_remote(&p).await,
                Action::TrashLocal => self.trash_local(&p).await,
                _ => self.db(|db| db.delete(&p)),
            };
            self.finish(&p, a, res);
        }

        // 4. Record folder etags where everything below succeeded.
        self.record_dirs(&acts);

        Outcome {
            failed: self.failed.lock().unwrap().clone(),
            network_errors: self.network_errors.load(Ordering::SeqCst),
            other_errors: self.other_errors.load(Ordering::SeqCst),
            unauthorized: self.unauthorized.load(Ordering::SeqCst),
            rerun: self.rerun.load(Ordering::SeqCst),
        }
    }

    fn record_dirs(&self, acts: &HashMap<String, Action>) {
        let failed = self.failed.lock().unwrap();
        let exact: HashSet<&str> = failed.iter().map(String::as_str).collect();
        // Failed paths plus all their parent folders (the dirs with `is_under(failed, dir)`).
        let blocked: HashSet<&str> = failed
            .iter()
            .flat_map(|f| {
                std::iter::once(f.as_str()).chain(f.match_indices('/').map(move |(i, _)| &f[..i]))
            })
            .collect();
        for (p, r) in self.remote.iter().filter(|(_, r)| r.is_dir) {
            let ok = match acts.get(p) {
                None | Some(Action::RecordOnly) => {
                    self.local.entries.get(p).is_some_and(|l| l.is_dir)
                }
                Some(Action::MkdirLocal) | Some(Action::Conflict) => !exact.contains(p.as_str()),
                _ => false,
            };
            if !ok {
                continue;
            }
            let etag = if blocked.contains(p.as_str()) {
                None
            } else {
                Some(r.etag.clone())
            };
            let b = self.base.get(p);
            if b.is_some_and(|b| b.is_dir && b.remote_etag == etag) && acts.get(p).is_none() {
                continue;
            }
            let e = DbEntry {
                path: p.clone(),
                is_dir: true,
                local_mtime: None,
                local_size: None,
                remote_etag: etag,
                remote_fileid: r.fileid.clone(),
            };
            if let Err(e) = self.db(|db| db.upsert(&e)) {
                tracing::error!("db: {e}");
            }
        }
    }

    fn record(&self, p: &str) -> DavResult<()> {
        let r = self.remote.get(p);
        match (self.local.entries.get(p), r) {
            // Folders are recorded in `record_dirs`.
            (Some(l), Some(r)) if !l.is_dir => self.db(|db| {
                db.upsert(&DbEntry {
                    path: p.to_string(),
                    is_dir: false,
                    local_mtime: Some(l.mtime),
                    local_size: Some(l.size),
                    remote_etag: Some(r.etag.clone()),
                    remote_fileid: r.fileid.clone(),
                })
            }),
            _ => Ok(()),
        }
    }

    fn dir_row(&self, p: &str, fileid: Option<String>) -> DbEntry {
        DbEntry {
            path: p.to_string(),
            is_dir: true,
            local_mtime: None,
            local_size: None,
            remote_etag: None,
            remote_fileid: fileid,
        }
    }

    async fn mkdir_local(&self, p: &str) -> DavResult<()> {
        let path = self.local_path(p);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(e.into()),
        }
        let row = self.dir_row(p, self.remote.get(p).and_then(|r| r.fileid.clone()));
        self.db(|db| db.upsert(&row))
    }

    async fn mkdir_remote(&self, p: &str) -> DavResult<()> {
        let fileid = self.dav.mkcol(p).await?;
        let row = self.dir_row(p, fileid);
        self.db(|db| db.upsert(&row))
    }

    /// File vs folder: moves the local item aside. Returns true if the remote file must be downloaded.
    async fn type_conflict(&self, p: &str) -> DavResult<bool> {
        let path = self.local_path(p);
        let is_dir = self.local.entries.get(p).is_some_and(|l| l.is_dir);
        let dest = conflict_path(&path, is_dir);
        tokio::fs::rename(&path, &dest).await?;
        tracing::info!("conflict: moved {} to {}", path.display(), dest.display());
        self.db(|db| db.delete_prefix(p))?;
        if self.remote.get(p).is_some_and(|r| r.is_dir) {
            self.mkdir_local(p).await?;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// Downloads to a temp file, then moves it into place. If the target changed since the scan
    /// (or `conflict` is set), the local version is kept as a conflicted copy.
    async fn download(
        &self,
        p: &str,
        conflict: bool,
        expected: Option<LocalEntry>,
    ) -> DavResult<()> {
        let r = self
            .remote
            .get(p)
            .cloned()
            .ok_or_else(|| DavError::Parse("missing remote entry".into()))?;
        let target = self.local_path(p);
        let parent = target.parent().unwrap_or(&self.root).to_path_buf();
        let tmp = parent.join(format!(".ncsync-{}.part", uuid::Uuid::new_v4()));
        let (etag, lastmod) = match self.dav.get(p, &tmp).await {
            Ok(v) => v,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        let result = self.place(p, &tmp, &target, &r, etag, lastmod, conflict, expected);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        &self,
        p: &str,
        tmp: &Path,
        target: &Path,
        r: &RemoteEntry,
        etag: Option<String>,
        lastmod: Option<i64>,
        conflict: bool,
        expected: Option<LocalEntry>,
    ) -> DavResult<()> {
        filetime::set_file_mtime(tmp, FileTime::from_unix_time(lastmod.unwrap_or(r.mtime), 0))?;
        if let Some(now) = local::stat(target)
            && (conflict || Some(now) != expected)
        {
            let dest = conflict_path(target, now.is_dir);
            std::fs::rename(target, &dest)?;
            tracing::info!("conflict: kept local version as {}", dest.display());
        }
        std::fs::rename(tmp, target)?;
        let st = local::stat(target).ok_or_else(|| DavError::Parse("file vanished".into()))?;
        self.db(|db| {
            db.upsert(&DbEntry {
                path: p.to_string(),
                is_dir: false,
                local_mtime: Some(st.mtime),
                local_size: Some(st.size),
                remote_etag: Some(etag.unwrap_or_else(|| r.etag.clone())),
                remote_fileid: r.fileid.clone(),
            })
        })
    }

    async fn upload(&self, p: &str) -> DavResult<()> {
        let path = self.local_path(p);
        let before = local::stat(&path).ok_or_else(|| DavError::Parse("file vanished".into()))?;
        if before.is_dir {
            return Err(DavError::Parse("became a folder".into()));
        }
        let cond = match self.remote.get(p) {
            Some(r) if !r.is_dir => Cond::IfMatch(r.etag.clone()),
            _ => Cond::IfNoneMatch,
        };
        let res = if before.size as u64 <= CHUNK_SIZE {
            self.dav.put(p, &path, before.mtime, &cond).await?
        } else {
            self.dav
                .chunked_upload(p, &path, before.mtime, &cond)
                .await?
        };
        if local::stat(&path) != Some(before) {
            tracing::info!("{p} changed during upload; will sync again");
            self.rerun.store(true, Ordering::SeqCst);
        }
        let etag = match res.etag {
            Some(e) => e,
            None => self
                .dav
                .propfind(p, 0)
                .await?
                .into_iter()
                .next()
                .map(|e| e.etag)
                .ok_or_else(|| DavError::Parse("no etag after upload".into()))?,
        };
        let fileid = res
            .fileid
            .or_else(|| self.remote.get(p).and_then(|r| r.fileid.clone()));
        self.db(|db| {
            db.upsert(&DbEntry {
                path: p.to_string(),
                is_dir: false,
                local_mtime: Some(before.mtime),
                local_size: Some(before.size),
                remote_etag: Some(etag),
                remote_fileid: fileid,
            })
        })
    }

    async fn delete_remote(&self, p: &str) -> DavResult<()> {
        // Files: only if unchanged since the scan, so a fresh server edit is never deleted.
        let etag = self
            .remote
            .get(p)
            .filter(|r| !r.is_dir)
            .map(|r| r.etag.as_str());
        self.dav.delete_if(p, etag).await?;
        self.db(|db| db.delete_prefix(p))
    }

    async fn trash_local(&self, p: &str) -> DavResult<()> {
        let path = self.local_path(p);
        let scanned = self.local.entries.get(p).copied();
        match local::stat(&path) {
            None => {}
            Some(now) if !now.is_dir && Some(now) != scanned => {
                self.rerun.store(true, Ordering::SeqCst);
                return Err(DavError::Parse("changed since scan; not trashed".into()));
            }
            Some(_) => {
                tokio::task::spawn_blocking(move || trash::delete(&path))
                    .await
                    .map_err(|e| DavError::Io(std::io::Error::other(e)))?
                    .map_err(|e| DavError::Io(std::io::Error::other(e)))?;
            }
        }
        self.db(|db| db.delete_prefix(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names() {
        let s = "2024-05-06 070809";
        assert_eq!(
            conflict_name("a.txt", s, false),
            "a (conflicted copy 2024-05-06 070809).txt"
        );
        assert_eq!(
            conflict_name("a.tar.gz", s, false),
            "a.tar (conflicted copy 2024-05-06 070809).gz"
        );
        assert_eq!(
            conflict_name(".bashrc", s, false),
            ".bashrc (conflicted copy 2024-05-06 070809)"
        );
        assert_eq!(
            conflict_name("noext", s, false),
            "noext (conflicted copy 2024-05-06 070809)"
        );
        assert_eq!(
            conflict_name("dir.v2", s, true),
            "dir.v2 (conflicted copy 2024-05-06 070809)"
        );
        assert!(crate::ignore::is_ignored(&conflict_name("a.txt", s, false)));
    }

    #[test]
    fn under() {
        assert!(is_under("a/b", "a"));
        assert!(is_under("a", "a"));
        assert!(!is_under("ab", "a"));
    }
}
