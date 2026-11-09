//! End-to-end test against a real Nextcloud. Opt-in:
//! `NCSYNC_E2E_URL=http://localhost:8080 cargo test e2e -- --ignored --nocapture`
//! Uses admin/admin (see the test server note in todo.md). Remote deletes in this test
//! move small `e2e-*` files to the macOS Trash.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::webdav::{Cond, RemoteEntry, http_client};

fn params(root: &Path, statuses: Arc<Mutex<Vec<Status>>>, confirm: bool) -> Option<EngineParams> {
    let url = std::env::var("NCSYNC_E2E_URL").ok()?;
    let dav = Arc::new(Dav::new(http_client(), &url, "admin", "admin", "admin"));
    let paused = Arc::new(AtomicBool::new(false));
    let p2 = paused.clone();
    Some(EngineParams {
        dav,
        db: Arc::new(Mutex::new(
            Db::open(&root.with_extension("sqlite")).unwrap(),
        )),
        root: root.to_path_buf(),
        paused,
        status: Arc::new(move |s| statuses.lock().unwrap().push(s)),
        set_paused: Arc::new(move |b| p2.store(b, Ordering::SeqCst)),
        confirm: Arc::new(move |_| confirm),
        activity: Arc::new(|_| {}),
    })
}

async fn last(p: &EngineParams, statuses: &Arc<Mutex<Vec<Status>>>, full: bool) -> Status {
    let r = run_once(p, full).await;
    assert!(!r.unauthorized);
    statuses.lock().unwrap().last().cloned().unwrap()
}

async fn remote_file(p: &EngineParams, path: &str) -> Option<RemoteEntry> {
    p.dav.propfind(path, 0).await.ok()?.into_iter().next()
}

async fn put_remote(p: &EngineParams, path: &str, content: &[u8], mtime: i64) {
    let tmp = std::env::temp_dir().join(format!("e2e-up-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, content).unwrap();
    p.dav.put(path, &tmp, mtime, &Cond::IfNoneMatch).await.ok();
    let etag = remote_file(p, path).await.unwrap().etag;
    p.dav
        .put(path, &tmp, mtime, &Cond::IfMatch(etag))
        .await
        .unwrap();
    std::fs::remove_file(tmp).unwrap();
}

async fn get_remote(p: &EngineParams, path: &str) -> Vec<u8> {
    let tmp = std::env::temp_dir().join(format!("e2e-get-{}", uuid::Uuid::new_v4()));
    p.dav.get(path, &tmp).await.unwrap();
    let v = std::fs::read(&tmp).unwrap();
    std::fs::remove_file(tmp).unwrap();
    v
}

#[tokio::test]
#[ignore]
async fn e2e() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let root = std::env::temp_dir().join(format!("ncsync-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let Some(p) = params(&root, statuses.clone(), false) else {
        eprintln!("NCSYNC_E2E_URL not set; skipping");
        return;
    };
    // Clean leftovers from earlier runs.
    for d in [
        "e2e-dir",
        "e2e-big.bin",
        "e2e-conflict.txt",
        "e2e-remote.txt",
        "e2e-gone",
    ] {
        p.dav.delete(d).await.unwrap();
    }

    // 1. Initial sync downloads the account; mtimes match.
    assert_eq!(last(&p, &statuses, true).await, Status::Idle);
    let remote_all = remote::scan(&p.dav, &Default::default(), true, &|_| {})
        .await
        .unwrap();
    let local_all = local::scan(&root).unwrap();
    for (path, r) in &remote_all.entries {
        let l = local_all
            .entries
            .get(path)
            .unwrap_or_else(|| panic!("missing locally: {path}"));
        assert_eq!(l.is_dir, r.is_dir, "{path}");
        if !r.is_dir {
            assert_eq!((l.size, l.mtime), (r.size, r.mtime), "{path}");
        }
    }
    eprintln!("initial: {} entries", remote_all.entries.len());

    // 2. Local create (with an NFD name) → server.
    let dir = root.join("e2e-dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("Cafe\u{301}.txt"), b"local").unwrap();
    std::fs::write(dir.join(".DS_Store"), b"junk").unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert_eq!(get_remote(&p, "e2e-dir/Caf\u{e9}.txt").await, b"local");
    assert!(remote_file(&p, "e2e-dir/.DS_Store").await.is_none());

    // 3. Remote create + edit → local.
    put_remote(&p, "e2e-remote.txt", b"v1", 1_600_000_000).await;
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert_eq!(std::fs::read(root.join("e2e-remote.txt")).unwrap(), b"v1");
    assert_eq!(
        local::stat(&root.join("e2e-remote.txt")).unwrap().mtime,
        1_600_000_000
    );
    put_remote(&p, "e2e-remote.txt", b"v2 longer", 1_600_000_100).await;
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert_eq!(
        std::fs::read(root.join("e2e-remote.txt")).unwrap(),
        b"v2 longer"
    );

    // 4. Local edit → server, nothing downloaded back.
    std::fs::write(root.join("e2e-remote.txt"), b"v3 from mac").unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert_eq!(get_remote(&p, "e2e-remote.txt").await, b"v3 from mac");
    let plan_after = run_once(&p, false).await;
    assert!(!plan_after.dirty);

    // 5. Conflict: both sides change.
    std::fs::write(root.join("e2e-conflict.txt"), b"base").unwrap();
    last(&p, &statuses, false).await;
    std::fs::write(root.join("e2e-conflict.txt"), b"local edit").unwrap();
    put_remote(&p, "e2e-conflict.txt", b"server edit!", 1_700_000_000).await;
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert_eq!(
        std::fs::read(root.join("e2e-conflict.txt")).unwrap(),
        b"server edit!"
    );
    let copies: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("e2e-conflict (conflicted copy "))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(std::fs::read(root.join(&copies[0])).unwrap(), b"local edit");
    assert!(remote_file(&p, &copies[0]).await.is_none());
    std::fs::remove_file(root.join(&copies[0])).unwrap();

    // 6. Big file → chunked upload, intact.
    let big: Vec<u8> = (0..25 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("e2e-big.bin"), &big).unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert!(get_remote(&p, "e2e-big.bin").await == big);
    // Overwrite via chunks: wrong etag → 412, right etag → ok.
    let path = root.join("e2e-big.bin");
    let r = p
        .dav
        .chunked_upload("e2e-big.bin", &path, 1, &Cond::IfMatch("nope".into()))
        .await;
    assert!(matches!(r, Err(DavError::PreconditionFailed)), "{r:?}");
    let r = p
        .dav
        .chunked_upload("e2e-big.bin", &path, 1, &Cond::IfNoneMatch)
        .await;
    assert!(matches!(r, Err(DavError::PreconditionFailed)), "{r:?}");
    let mut big2 = big.clone();
    big2[0] = 42;
    std::fs::write(&path, &big2).unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert!(get_remote(&p, "e2e-big.bin").await == big2);

    let wrong = p.dav.delete_if("e2e-big.bin", Some("nope")).await;
    assert!(
        matches!(wrong, Err(DavError::PreconditionFailed)),
        "{wrong:?}"
    );

    // 7. Local delete → server delete.
    std::fs::remove_file(root.join("e2e-big.bin")).unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert!(remote_file(&p, "e2e-big.bin").await.is_none());

    // 8. Remote-deleted folder with a local change: change survives, unchanged file trashed.
    std::fs::create_dir(root.join("e2e-gone")).unwrap();
    std::fs::write(root.join("e2e-gone/keep.txt"), b"a").unwrap();
    std::fs::write(root.join("e2e-gone/old.txt"), b"b").unwrap();
    last(&p, &statuses, false).await;
    p.dav.delete("e2e-gone").await.unwrap();
    std::fs::write(root.join("e2e-gone/keep.txt"), b"changed").unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert!(root.join("e2e-gone/keep.txt").exists());
    assert!(!root.join("e2e-gone/old.txt").exists());
    assert_eq!(get_remote(&p, "e2e-gone/keep.txt").await, b"changed");

    // 9. Remote delete of a whole folder → local folder trashed.
    p.dav.delete("e2e-gone").await.unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Idle);
    assert!(!root.join("e2e-gone").exists());

    // 10. Emptying the local folder asks first; declining pauses and deletes nothing.
    let moved = root.with_extension("moved");
    std::fs::rename(&root, &moved).unwrap();
    assert_eq!(
        last(&p, &statuses, false).await,
        Status::Error("Sync folder missing".into())
    );
    std::fs::create_dir(&root).unwrap();
    assert_eq!(last(&p, &statuses, false).await, Status::Paused);
    assert!(p.paused.load(Ordering::SeqCst));
    assert!(remote_file(&p, "e2e-remote.txt").await.is_some());

    // Cleanup.
    for d in ["e2e-dir", "e2e-conflict.txt", "e2e-remote.txt"] {
        p.dav.delete(d).await.unwrap();
    }
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&moved).ok();
    std::fs::remove_file(root.with_extension("sqlite")).ok();

    // 11. Engine loop: a local change reaches the server via the watcher. Sequential, because
    // tests share one server account.
    engine_watcher().await;
}

async fn engine_watcher() {
    let root = std::env::temp_dir().join(format!("ncsync-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let Some(p) = params(&root, statuses.clone(), false) else {
        return;
    };
    let dav = p.dav.clone();
    dav.delete("e2e-watch.txt").await.unwrap();
    let engine = Engine::start(p);
    let wait_idle = async {
        while statuses.lock().unwrap().last() != Some(&Status::Idle) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(60), wait_idle)
        .await
        .unwrap();

    let t = std::time::Instant::now();
    std::fs::write(root.join("e2e-watch.txt"), b"watched").unwrap();
    loop {
        if dav.propfind("e2e-watch.txt", 0).await.is_ok() {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "not uploaded within 10 s"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!("local change reached server in {:?}", t.elapsed());
    drop(engine);
    dav.delete("e2e-watch.txt").await.unwrap();
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_file(root.with_extension("sqlite")).ok();
}
