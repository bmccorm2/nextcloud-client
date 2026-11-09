use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::db::DbEntry;
use crate::sync::local::LocalEntry;
use crate::webdav::RemoteEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Upload,
    Download,
    MkdirLocal,
    MkdirRemote,
    DeleteRemote,
    TrashLocal,
    Conflict,
    RecordOnly,
    ForgetBase,
}

fn local_changed(l: &LocalEntry, base: Option<&DbEntry>) -> bool {
    match base {
        None => true,
        Some(b) if b.is_dir != l.is_dir => true,
        Some(_) if l.is_dir => false,
        Some(b) => b.local_mtime != Some(l.mtime) || b.local_size != Some(l.size),
    }
}

fn remote_changed(r: &RemoteEntry, base: Option<&DbEntry>) -> bool {
    match base {
        None => true,
        Some(b) => b.is_dir != r.is_dir || b.remote_etag.as_deref() != Some(r.etag.as_str()),
    }
}

/// Decides what to do with one path. Folder-level rules are applied afterwards by [`plan`].
pub fn decide(
    local: Option<&LocalEntry>,
    remote: Option<&RemoteEntry>,
    base: Option<&DbEntry>,
) -> Action {
    match (local, remote) {
        (None, None) if base.is_some() => Action::ForgetBase,
        (None, None) => Action::None,
        // New locally, or modified locally while deleted remotely: the modification wins.
        (Some(l), None) if local_changed(l, base) => {
            if l.is_dir {
                Action::MkdirRemote
            } else {
                Action::Upload
            }
        }
        (Some(_), None) => Action::TrashLocal,
        (None, Some(r)) if r.is_dir && base.is_none_or(|b| !b.is_dir) => Action::MkdirLocal,
        // Folder deleted locally: its descendants decide in `plan`.
        (None, Some(r)) if r.is_dir => Action::DeleteRemote,
        (None, Some(r)) if remote_changed(r, base) => Action::Download,
        (None, Some(_)) => Action::DeleteRemote,
        (Some(l), Some(r)) if l.is_dir != r.is_dir => Action::Conflict,
        (Some(l), Some(r)) if l.is_dir => {
            let same =
                base.is_some_and(|b| b.is_dir && b.remote_etag.as_deref() == Some(r.etag.as_str()));
            if same {
                Action::None
            } else {
                Action::RecordOnly
            }
        }
        (Some(l), Some(r)) => match (local_changed(l, base), remote_changed(r, base)) {
            (false, false) => Action::None,
            (true, false) => Action::Upload,
            (false, true) => Action::Download,
            (true, true) if l.size == r.size && l.mtime == r.mtime => Action::RecordOnly,
            (true, true) => Action::Conflict,
        },
    }
}

fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// Builds the full plan: per-path decisions plus folder rules. Returns non-`None` actions sorted by path.
pub fn plan(
    local: &HashMap<String, LocalEntry>,
    remote: &HashMap<String, RemoteEntry>,
    base: &BTreeMap<String, DbEntry>,
) -> Vec<(String, Action)> {
    let keys: BTreeSet<&String> = local
        .keys()
        .chain(remote.keys())
        .chain(base.keys())
        .collect();
    let mut acts: BTreeMap<String, Action> = keys
        .into_iter()
        .map(|k| (k.clone(), decide(local.get(k), remote.get(k), base.get(k))))
        .collect();

    // A local folder that conflicts with a remote file is renamed away; its contents go with it.
    let renamed_dirs: Vec<String> = acts
        .iter()
        .filter(|(k, a)| **a == Action::Conflict && local.get(*k).is_some_and(|l| l.is_dir))
        .map(|(k, _)| k.clone())
        .collect();
    for d in &renamed_dirs {
        let lo = format!("{d}/");
        let hi = format!("{d}0");
        for (_, a) in acts.range_mut(lo..hi) {
            *a = Action::None;
        }
    }

    fold_deleted_dirs(
        &mut acts,
        Action::TrashLocal,
        &[Action::Upload, Action::MkdirRemote],
        Action::MkdirRemote,
    );
    fold_deleted_dirs(
        &mut acts,
        Action::DeleteRemote,
        &[Action::Download, Action::MkdirLocal],
        Action::MkdirLocal,
    );

    acts.into_iter()
        .filter(|(_, a)| *a != Action::None)
        .collect()
}

/// Folder rule for one direction. A folder marked `del` that contains `keep` work is recreated
/// (`recreate`) and only its unchanged items are deleted. Otherwise the folder is deleted as a whole
/// and its descendants' deletes are dropped.
fn fold_deleted_dirs(
    acts: &mut BTreeMap<String, Action>,
    del: Action,
    keep: &[Action],
    recreate: Action,
) {
    let keep_paths: Vec<String> = acts
        .iter()
        .filter(|(_, a)| keep.contains(a))
        .map(|(k, _)| k.clone())
        .collect();
    for p in keep_paths {
        for anc in ancestors(&p) {
            if let Some(a) = acts.get_mut(anc)
                && *a == del
            {
                *a = recreate;
            }
        }
    }
    let covered: Vec<String> = acts
        .iter()
        .filter(|(k, a)| **a == del && ancestors(k).any(|anc| acts.get(anc) == Some(&del)))
        .map(|(k, _)| k.clone())
        .collect();
    for k in covered {
        acts.insert(k, Action::None);
    }
}

#[cfg(test)]
mod tests {
    use super::Action::{
        Conflict, DeleteRemote, Download, ForgetBase, MkdirLocal, MkdirRemote, RecordOnly,
        TrashLocal, Upload,
    };
    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum S {
        Absent,
        Unchanged,
        Changed,
    }

    fn base_file(path: &str) -> DbEntry {
        DbEntry {
            path: path.into(),
            is_dir: false,
            local_mtime: Some(100),
            local_size: Some(10),
            remote_etag: Some("e1".into()),
            remote_fileid: None,
        }
    }

    fn lf(s: S) -> Option<LocalEntry> {
        match s {
            S::Absent => None,
            S::Unchanged => Some(LocalEntry {
                is_dir: false,
                mtime: 100,
                size: 10,
            }),
            S::Changed => Some(LocalEntry {
                is_dir: false,
                mtime: 200,
                size: 20,
            }),
        }
    }

    fn rf(path: &str, s: S) -> Option<RemoteEntry> {
        let (etag, mtime, size) = match s {
            S::Absent => return None,
            S::Unchanged => ("e1", 100, 10),
            S::Changed => ("e2", 300, 30),
        };
        Some(RemoteEntry {
            path: path.into(),
            is_dir: false,
            etag: etag.into(),
            mtime,
            size,
            fileid: None,
        })
    }

    #[test]
    fn decide_table_with_base() {
        use S::*;
        let table = [
            (Absent, Absent, ForgetBase),
            (Absent, Unchanged, DeleteRemote),
            (Absent, Changed, Download),
            (Unchanged, Absent, TrashLocal),
            (Unchanged, Unchanged, Action::None),
            (Unchanged, Changed, Download),
            (Changed, Absent, Upload),
            (Changed, Unchanged, Upload),
            (Changed, Changed, Conflict),
        ];
        let b = base_file("f");
        for (l, r, want) in table {
            let got = decide(lf(l).as_ref(), rf("f", r).as_ref(), Some(&b));
            assert_eq!(got, want, "local={l:?} remote={r:?}");
        }
    }

    #[test]
    fn decide_table_without_base() {
        use S::*;
        // Without a base, "unchanged" and "changed" both mean "present".
        let table = [
            (Absent, Absent, Action::None),
            (Absent, Unchanged, Download),
            (Absent, Changed, Download),
            (Unchanged, Absent, Upload),
            (Changed, Absent, Upload),
            (Unchanged, Unchanged, Conflict), // local 100/10 vs remote 100/10 → equal → see below
            (Unchanged, Changed, Conflict),
            (Changed, Unchanged, Conflict),
            (Changed, Changed, Conflict),
        ];
        for (l, r, want) in table {
            let got = decide(lf(l).as_ref(), rf("f", r).as_ref(), None);
            let equal = matches!((l, r), (Unchanged, Unchanged));
            let want = if equal { RecordOnly } else { want };
            assert_eq!(got, want, "local={l:?} remote={r:?}");
        }
    }

    #[test]
    fn equal_both_changed_is_record_only() {
        let l = LocalEntry {
            is_dir: false,
            mtime: 300,
            size: 30,
        };
        let r = rf("f", S::Changed).unwrap();
        assert_eq!(
            decide(Some(&l), Some(&r), Some(&base_file("f"))),
            RecordOnly
        );
    }

    fn ld() -> LocalEntry {
        LocalEntry {
            is_dir: true,
            mtime: 5,
            size: 0,
        }
    }
    fn rd(path: &str, etag: &str) -> RemoteEntry {
        RemoteEntry {
            path: path.into(),
            is_dir: true,
            etag: etag.into(),
            mtime: 0,
            size: 0,
            fileid: None,
        }
    }
    fn bd(path: &str, etag: Option<&str>) -> DbEntry {
        DbEntry {
            path: path.into(),
            is_dir: true,
            remote_etag: etag.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn decide_dirs() {
        let b = bd("d", Some("d1"));
        assert_eq!(
            decide(Some(&ld()), Some(&rd("d", "d1")), Some(&b)),
            Action::None
        );
        assert_eq!(
            decide(Some(&ld()), Some(&rd("d", "d2")), Some(&b)),
            RecordOnly
        );
        assert_eq!(decide(Some(&ld()), Some(&rd("d", "d1")), None), RecordOnly);
        assert_eq!(decide(Some(&ld()), None, None), MkdirRemote);
        assert_eq!(decide(None, Some(&rd("d", "d1")), None), MkdirLocal);
        assert_eq!(decide(Some(&ld()), None, Some(&b)), TrashLocal);
        assert_eq!(decide(None, Some(&rd("d", "d2")), Some(&b)), DeleteRemote);
        assert_eq!(decide(None, None, Some(&b)), ForgetBase);
        // Type changes.
        assert_eq!(
            decide(Some(&ld()), rf("d", S::Unchanged).as_ref(), None),
            Conflict
        );
        assert_eq!(
            decide(lf(S::Unchanged).as_ref(), Some(&rd("d", "x")), None),
            Conflict
        );
        assert_eq!(
            decide(None, Some(&rd("d", "x")), Some(&base_file("d"))),
            MkdirLocal
        );
        assert_eq!(
            decide(Some(&ld()), None, Some(&base_file("d"))),
            MkdirRemote
        );
        assert_eq!(
            decide(None, rf("d", S::Unchanged).as_ref(), Some(&b)),
            Download
        );
        assert_eq!(decide(lf(S::Unchanged).as_ref(), None, Some(&b)), Upload);
    }

    struct World {
        local: HashMap<String, LocalEntry>,
        remote: HashMap<String, RemoteEntry>,
        base: BTreeMap<String, DbEntry>,
    }

    impl World {
        fn new() -> World {
            World {
                local: HashMap::new(),
                remote: HashMap::new(),
                base: BTreeMap::new(),
            }
        }
        /// A synced folder present on both sides.
        fn dir(&mut self, p: &str) {
            self.local.insert(p.into(), ld());
            self.remote.insert(p.into(), rd(p, "d1"));
            self.base.insert(p.into(), bd(p, Some("d1")));
        }
        /// A synced file present on both sides.
        fn file(&mut self, p: &str) {
            self.local.insert(p.into(), lf(S::Unchanged).unwrap());
            self.remote.insert(p.into(), rf(p, S::Unchanged).unwrap());
            self.base.insert(p.into(), base_file(p));
        }
        fn plan(&self) -> Vec<(String, Action)> {
            plan(&self.local, &self.remote, &self.base)
        }
    }

    fn p(v: &[(&str, Action)]) -> Vec<(String, Action)> {
        v.iter().map(|(a, b)| (a.to_string(), *b)).collect()
    }

    #[test]
    fn remote_deleted_dir_unchanged_is_trashed_whole() {
        let mut w = World::new();
        w.dir("d");
        w.dir("d/s");
        w.file("d/a");
        w.file("d/s/b");
        w.remote.retain(|k, _| !(k == "d" || k.starts_with("d/")));
        assert_eq!(w.plan(), p(&[("d", TrashLocal)]));
    }

    #[test]
    fn remote_deleted_dir_with_local_changes_keeps_changes() {
        let mut w = World::new();
        w.dir("d");
        w.dir("d/s");
        w.file("d/a");
        w.file("d/s/b");
        w.file("d/s/c");
        w.remote.retain(|k, _| !(k == "d" || k.starts_with("d/")));
        w.local.insert("d/s/b".into(), lf(S::Changed).unwrap());
        w.local.insert("d/new".into(), lf(S::Unchanged).unwrap());
        assert_eq!(
            w.plan(),
            p(&[
                ("d", MkdirRemote),
                ("d/a", TrashLocal),
                ("d/new", Upload),
                ("d/s", MkdirRemote),
                ("d/s/b", Upload),
                ("d/s/c", TrashLocal),
            ])
        );
    }

    #[test]
    fn local_deleted_dir_unchanged_is_deleted_whole() {
        let mut w = World::new();
        w.dir("d");
        w.file("d/a");
        w.local.clear();
        assert_eq!(w.plan(), p(&[("d", DeleteRemote)]));
    }

    #[test]
    fn local_deleted_dir_with_remote_changes_keeps_changes() {
        let mut w = World::new();
        w.dir("d");
        w.file("d/a");
        w.file("d/b");
        w.local.clear();
        w.remote.insert("d".into(), rd("d", "d2"));
        w.remote
            .insert("d/b".into(), rf("d/b", S::Changed).unwrap());
        assert_eq!(
            w.plan(),
            p(&[("d", MkdirLocal), ("d/a", DeleteRemote), ("d/b", Download)])
        );
    }

    #[test]
    fn local_dir_vs_remote_file_drops_children() {
        let mut w = World::new();
        w.dir("d");
        w.file("d/a");
        w.remote.clear();
        w.remote.insert("d".into(), rf("d", S::Changed).unwrap());
        assert_eq!(w.plan(), p(&[("d", Conflict)]));
    }

    #[test]
    fn initial_sync_into_empty_folder() {
        let mut w = World::new();
        w.remote.insert("d".into(), rd("d", "x"));
        w.remote
            .insert("d/a".into(), rf("d/a", S::Unchanged).unwrap());
        assert_eq!(w.plan(), p(&[("d", MkdirLocal), ("d/a", Download)]));
    }

    #[test]
    fn everything_in_sync_is_empty() {
        let mut w = World::new();
        w.dir("d");
        w.file("d/a");
        w.file("b");
        assert!(w.plan().is_empty());
    }
}
