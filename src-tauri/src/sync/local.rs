use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::ignore::is_ignored;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalEntry {
    pub is_dir: bool,
    pub mtime: i64,
    pub size: i64,
}

#[derive(Debug, Default)]
pub struct LocalScan {
    /// NFC relative path → entry.
    pub entries: HashMap<String, LocalEntry>,
    /// NFC relative path → real on-disk path (may be NFD).
    pub real: HashMap<String, PathBuf>,
}

pub fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn entry_of(md: &std::fs::Metadata) -> LocalEntry {
    LocalEntry {
        is_dir: md.is_dir(),
        mtime: mtime_secs(md),
        size: if md.is_dir() { 0 } else { md.len() as i64 },
    }
}

/// Stats a path without following symlinks.
pub fn stat(path: &Path) -> Option<LocalEntry> {
    std::fs::symlink_metadata(path).ok().map(|m| entry_of(&m))
}

fn is_stale_part(name: &str) -> bool {
    name.starts_with(".ncsync-") && name.ends_with(".part")
}

/// Walks `root`. Any walk error aborts the scan: a skipped folder would look deleted.
pub fn scan(root: &Path) -> anyhow::Result<LocalScan> {
    let mut out = LocalScan::default();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if is_stale_part(&name) && e.file_type().is_file() {
                // Leftover from an interrupted download; only one run is active at a time.
                let _ = std::fs::remove_file(e.path());
            }
            !is_ignored(&name)
        });
    for item in walker {
        let item = item?;
        if item.file_type().is_symlink() {
            tracing::info!("skipping symlink {}", item.path().display());
            continue;
        }
        let rel = item.path().strip_prefix(root)?;
        let Some(rel_str) = rel.to_str() else {
            tracing::warn!("skipping non-UTF-8 name {}", item.path().display());
            continue;
        };
        let key: String = rel_str.nfc().collect();
        let md = item.metadata()?;
        if out.entries.contains_key(&key) {
            tracing::warn!(
                "skipping {}: same name after Unicode normalization",
                item.path().display()
            );
            continue;
        }
        out.entries.insert(key.clone(), entry_of(&md));
        out.real.insert(key, item.path().to_path_buf());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_and_normalizes() {
        let root = std::env::temp_dir().join(format!("ncsync-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("dir")).unwrap();
        let nfd = "Cafe\u{301}.txt";
        std::fs::write(root.join("dir").join(nfd), b"hello").unwrap();
        std::fs::write(root.join(".DS_Store"), b"x").unwrap();
        std::fs::write(root.join(".ncsync-old.part"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("dir"), root.join("link")).unwrap();

        let s = scan(&root).unwrap();
        let mut keys: Vec<_> = s.entries.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["dir", "dir/Caf\u{e9}.txt"]);
        assert_eq!(s.entries["dir/Caf\u{e9}.txt"].size, 5);
        assert!(s.entries["dir"].is_dir);
        assert!(s.real["dir/Caf\u{e9}.txt"].exists());
        assert!(!root.join(".ncsync-old.part").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
