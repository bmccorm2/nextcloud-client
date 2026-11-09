use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::db::DbEntry;
use crate::ignore::is_ignored;
use crate::webdav::{Dav, DavError, DavResult, RemoteEntry};

pub struct RemoteScan {
    pub root_etag: String,
    pub entries: HashMap<String, RemoteEntry>,
    /// Folders the server failed to list. Nothing at or below them is synced this run.
    pub skipped: Vec<(String, DavError)>,
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Rows strictly below `dir`.
pub fn below<'a>(
    base: &'a BTreeMap<String, DbEntry>,
    dir: &str,
) -> impl Iterator<Item = &'a DbEntry> + 'a {
    let lo = format!("{dir}/");
    let hi = format!("{dir}0");
    base.range(lo..hi).map(|(_, e)| e)
}

/// Tries to rebuild the remote state of an unchanged folder from the DB.
/// Fails (None) if any row lacks a recorded etag.
fn reuse(base: &BTreeMap<String, DbEntry>, dir: &str) -> Option<Vec<RemoteEntry>> {
    below(base, dir)
        .map(|e| {
            Some(RemoteEntry {
                path: e.path.clone(),
                is_dir: e.is_dir,
                etag: e.remote_etag.clone()?,
                mtime: e.local_mtime.unwrap_or(0),
                size: e.local_size.unwrap_or(0),
                fileid: e.remote_fileid.clone(),
            })
        })
        .collect()
}

/// Folder listings in flight at once.
const MAX_LISTINGS: usize = 6;

/// Recursive Depth-1 PROPFIND, several folders at once. With `full`, every folder is listed;
/// otherwise folders whose etag matches the DB reuse the DB rows.
pub async fn scan(
    dav: &Arc<Dav>,
    base: &BTreeMap<String, DbEntry>,
    full: bool,
    on_dir: &(dyn Fn(&str) + Send + Sync),
) -> DavResult<RemoteScan> {
    let mut entries = HashMap::new();
    let mut root_etag = String::new();
    let mut skipped = Vec::new();
    let mut queue = VecDeque::from([String::new()]);
    let mut set = JoinSet::new();
    loop {
        while set.len() < MAX_LISTINGS {
            let Some(dir) = queue.pop_front() else { break };
            on_dir(&dir);
            let dav = dav.clone();
            set.spawn(async move {
                let res = dav.propfind(&dir, 1).await;
                (dir, res)
            });
        }
        // Returning early drops the set, which aborts the other listings.
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (dir, res) = joined.map_err(|e| DavError::Parse(e.to_string()))?;
        let list = match res {
            Ok(l) => l,
            // The root, auth and network errors abort the scan.
            Err(e @ (DavError::Other(_) | DavError::Parse(_))) if !dir.is_empty() => {
                tracing::warn!("cannot list remote folder {dir}: {e}; skipping it");
                skipped.push((dir, e));
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut seen: HashSet<String> = HashSet::new();
        for e in list {
            if e.path == dir {
                if dir.is_empty() {
                    root_etag = e.etag.clone();
                }
                continue;
            }
            let name = name_of(&e.path);
            if is_ignored(name) {
                continue;
            }
            if !seen.insert(name.to_lowercase()) {
                tracing::warn!(
                    "skipping remote {}: differs only by case from another item",
                    e.path
                );
                continue;
            }
            if e.is_dir {
                let unchanged = !full
                    && base
                        .get(&e.path)
                        .is_some_and(|b| b.is_dir && b.remote_etag.as_deref() == Some(&e.etag));
                match unchanged.then(|| reuse(base, &e.path)).flatten() {
                    Some(rows) => {
                        for r in rows {
                            entries.insert(r.path.clone(), r);
                        }
                    }
                    None => queue.push_back(e.path.clone()),
                }
            }
            entries.insert(e.path.clone(), e);
        }
    }
    Ok(RemoteScan {
        root_etag,
        entries,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_rows() {
        let mut base = BTreeMap::new();
        for (p, d, etag) in [
            ("a", true, Some("1")),
            ("a/x", false, Some("2")),
            ("a0", false, None),
        ] {
            base.insert(
                p.to_string(),
                DbEntry {
                    path: p.into(),
                    is_dir: d,
                    remote_etag: etag.map(Into::into),
                    ..Default::default()
                },
            );
        }
        let r = reuse(&base, "a").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path, "a/x");
        base.get_mut("a/x").unwrap().remote_etag = None;
        assert!(reuse(&base, "a").is_none());
    }
}
