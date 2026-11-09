# NCSync: minimal Nextcloud file-sync client for macOS (Tauri 2 + Rust)

## Goal
A tiny macOS menu bar app that keeps two-way sync between the user's whole Nextcloud account and one local folder. Nothing else. It replaces the official desktop client.

## Context
- The repo is empty. Scaffold it with `bun create tauri-app` (template `vanilla-ts`, package manager `bun`), with the project in the repo root.
- Stack: Tauri 2, Rust (stable), tokio, and a vanilla TypeScript + Vite frontend (no framework, plain CSS).
- Server: Nextcloud 35 (`nextcloud:35-fpm-alpine`) on the user's VPS over HTTPS.
- Target: Apple Silicon only (`aarch64-apple-darwin`), macOS 13+, personal use.
- The user's shell is fish. Any commands given to the user must work in fish.
- Commands:
  - Dev: `bun run tauri dev`
  - Test: `cd src-tauri; cargo test`
  - Lint: `cd src-tauri; cargo clippy -- -D warnings`
  - Build: `bun run tauri build --bundles app --target aarch64-apple-darwin`
    - Output: `src-tauri/target/aarch64-apple-darwin/release/bundle/macos/NCSync.app`
- Identifiers:
  - App name: `NCSync`
  - Bundle id: `com.bryan.ncsync`
  - Keychain service: `com.bryan.ncsync`
- Where files go:
  - Config: `~/Library/Application Support/com.bryan.ncsync/config.json`
  - State DB: `~/Library/Application Support/com.bryan.ncsync/state.sqlite`
  - Logs: `~/Library/Logs/com.bryan.ncsync/`
  - Nothing is ever written inside the sync folder except synced files and temporary `.ncsync-*.part` files.

### Target layout
```
index.html, src/main.ts, src/styles.css      # settings window UI
src-tauri/
  tauri.conf.json, capabilities/default.json, Cargo.toml
  src/main.rs        # calls lib::run()
  src/lib.rs         # builder, plugins, commands, app state, ActivationPolicy::Accessory
  src/tray.rs        # tray icon + menu
  src/config.rs      # load/save config.json
  src/auth.rs        # Login Flow v2, Keychain, user id lookup
  src/webdav.rs      # PROPFIND/GET/PUT/MKCOL/DELETE/MOVE + chunked upload + XML parsing
  src/db.rs          # SQLite state
  src/ignore.rs      # ignore rules
  src/sync/mod.rs    # engine loop, triggers, status
  src/sync/local.rs  # local scan
  src/sync/remote.rs # remote scan
  src/sync/reconcile.rs # pure decision function
  src/sync/execute.rs   # performs actions
```

### Crates
- Tauri plugins: `tauri` (feature `tray-icon`), `tauri-plugin-autostart`, `tauri-plugin-dialog`, `tauri-plugin-opener`, `tauri-plugin-single-instance`.
- HTTP and data: `reqwest` (features `rustls-tls`, `stream`, `json`; no default features), `quick-xml` (feature `serialize`), `serde`, `serde_json`, `rusqlite` (feature `bundled`).
- Filesystem: `notify`, `notify-debouncer-full`, `walkdir`, `filetime`, `trash`.
- Other: `keyring` (feature `apple-native`), `tokio` (full), `chrono`, `httpdate`, `unicode-normalization`, `percent-encoding`, `uuid`, `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `tracing-appender`.
- Add no other dependencies.

## Decisions
- **Scope:** one account, one sync pair: the whole remote account root ↔ one local folder the user picks. Sync is two-way.
- **Auth:** Nextcloud Login Flow v2 in the default browser. Store the app password in the Keychain under service `com.bryan.ncsync`, account `<loginName>@<host>`. Never store the password in `config.json`.
- **UI:**
  - Menu bar only, with no Dock icon (`ActivationPolicy::Accessory`). Single instance.
  - Tray menu, in order:
    1. Status line (disabled item)
    2. `Sync now`
    3. `Pause` / `Resume`
    4. `Open folder`
    5. `Settings…`
    6. `Quit`
  - Settings window contains:
    - Server URL field and a `Log in` button. When logged in, show "Logged in as X on host" and a `Log out` button instead.
    - Local folder path with a `Choose…` button.
    - A `Start at login` checkbox.
  - Closing the window hides it; it does not quit the app. The window opens automatically on launch if setup is incomplete.
- **Start at login:** `tauri-plugin-autostart` with `MacosLauncher::LaunchAgent`. Off by default.
- **Triggers (these match the official client):**
  - Local FS events, debounced 2 s.
  - A remote poll every 30 s. The poll is a Depth-0 PROPFIND of the root; run a sync only if the root etag changed or there are local changes.
  - A forced full sync every 1 h.
  - `Sync now`.
  - Only one sync run at a time. A trigger that arrives during a run sets a "run again" flag.
- **Deletes:**
  - Deleted on the server → move the local file or folder to the macOS Trash (`trash` crate).
  - Deleted locally → `DELETE` on the server (the server keeps it in its own trash bin).
- **Conflicts (same as the official client):**
  - When both sides changed a file and the contents differ, the server version keeps the original name.
  - The local version is renamed to `<stem> (conflicted copy YYYY-MM-DD HHMMSS)<.ext>`, using local time.
  - Conflicted copies stay local only and are never uploaded (ignore pattern).
  - If both sides have equal size and mtime, it's not a conflict; just record the state.
  - Modify vs delete: the modification wins (re-upload or re-download it).
  - File vs directory mismatch: treat as a conflict; rename the local item, then download the remote one.
- **Uploads:**
  - Files ≤ 10 MiB use a single `PUT`. Larger files use chunked upload v2 with 10 MiB chunks.
  - Always send `X-OC-Mtime: <unix secs>`.
  - Send `If-Match: <etag>` when overwriting a known file, and `If-None-Match: *` for new files. A `412` means the server changed underneath; mark the item dirty and resync.
- **Ignored (hardcoded, no UI):** `.DS_Store`, `._*`, `.~lock*`, `*.part`, `Icon\r`, `.ncsync-*`, `* (conflicted copy *`, and symlinks (skip and log).
- **Renames:** no rename detection. A rename is handled as delete + create.
- **Distribution:** unsigned/ad-hoc (`bundle.macOS.signingIdentity = "-"`). No notarization, no auto-update.
- **Server URL:**
  - Require `https://`. Allow `http://` only for `localhost` and `127.0.0.1`.
  - Strip the trailing slash.
- **User-Agent:** `Mozilla/5.0 (Macintosh) NCSync/<version>`. Nextcloud shows this as the device name.

## Tasks
Tick each box as you finish it.

### Scaffold
- [x] 1. Scaffold with `bun create tauri-app` (vanilla-ts) in the repo root.
  - Set productName `NCSync`, identifier `com.bryan.ncsync`, `bundle.macOS.minimumSystemVersion = "13.0"`, and `signingIdentity = "-"`.
  - Add a `.gitignore` covering `node_modules`, `dist`, and `src-tauri/target`.
- [x] 2. Add the crates and plugins listed above. Set up `capabilities/default.json` with only the permissions used: dialog open, autostart, opener, and event listen.
- [x] 3. `lib.rs`:
  - Register the plugins. Single-instance should focus the settings window.
  - Set `ActivationPolicy::Accessory`.
  - Set up `tracing` to log to `~/Library/Logs/com.bryan.ncsync/ncsync.log` with daily rotation, keeping 7 files.
  - The settings window starts hidden. Intercept `CloseRequested` → hide instead.

### Config and auth
- [x] 4. `config.rs`: `Config { server_url: Option<String>, login_name: Option<String>, user_id: Option<String>, local_root: Option<PathBuf>, paused: bool }` with `load()` and `save()`. Write atomically: write a temp file, then rename.
- [x] 5. `auth.rs`, Login Flow v2:
  1. `POST {server}/index.php/login/v2` → `{poll:{token,endpoint}, login}`.
  2. Open `login` with the opener plugin.
  3. `POST endpoint` with form `token=<token>` every 2 s. Stop on `200` (`{server, loginName, appPassword}`), on a 20 min timeout, or on cancel.
  4. Then fetch the user id: `GET {server}/ocs/v2.php/cloud/user?format=json` with header `OCS-APIRequest: true` and basic auth. Use `ocs.data.id`. The WebDAV path needs this id; it can differ from `loginName`.
  5. Save the password to the Keychain and the rest to config.
  - `logout()`:
    - Try `DELETE {server}/ocs/v2.php/core/apppassword` (`OCS-APIRequest: true`) and ignore errors.
    - Delete the Keychain entry, clear auth fields in config, and wipe the state DB.
    - Never touch local files.

### WebDAV
- [x] 6. `webdav.rs`:
  - Shared `reqwest::Client` with basic auth (loginName + app password) and the User-Agent.
  - Files base URL: `{server}/remote.php/dav/files/{user_id}/`. Paths are percent-encoded per segment.
  - `propfind(path, depth: 0|1) -> Vec<RemoteEntry { path, is_dir, etag, mtime (secs), size, fileid }>`
    - Props: `d:getetag`, `d:getlastmodified`, `d:getcontentlength`, `d:resourcetype`, `oc:fileid` (`xmlns:oc="http://owncloud.org/ns"`).
    - Strip quotes from etags. Decode hrefs back to relative NFC paths.
  - `get(path, dest_tmp) -> etag`: streams to disk and returns the `ETag` response header.
  - `put(path, file, mtime, cond)`: returns `OC-ETag` and `OC-FileId`.
  - `mkcol`, `delete`.
  - `chunked_upload(path, file, mtime, cond)`:
    1. `MKCOL {server}/remote.php/dav/uploads/{user_id}/ncsync-<uuid>` with a `Destination: <files url>` header.
    2. `PUT` chunks `00001`, `00002`, … (10 MiB each), each with the `Destination` header.
    3. `MOVE …/.file` with `Destination`, `X-OC-Mtime`, `OC-Total-Length`, and the condition header.
    4. On failure, `DELETE` the upload dir.
  - Typed errors: `Unauthorized` (401), `PreconditionFailed` (412), `InsufficientStorage` (507), `Network`, `Other(status)`.

### State and rules
- [x] 7. `db.rs`, SQLite:
  ```sql
  CREATE TABLE entries (
    path TEXT PRIMARY KEY,   -- relative, '/'-separated, NFC, no leading slash
    is_dir INTEGER NOT NULL,
    local_mtime INTEGER,     -- unix secs
    local_size INTEGER,
    remote_etag TEXT,
    remote_fileid TEXT
  );
  CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);  -- root_etag
  ```
  Provide get/upsert/delete/all/clear, plus `delete_prefix(dir)` for folder removals.
- [x] 8. `ignore.rs`: `fn is_ignored(name: &str) -> bool` implementing the hardcoded list. Unit test every pattern.

### Sync engine
- [x] 9. `sync/local.rs`: walk `local_root` with `walkdir` (no symlink following; skip symlinks and ignored names) → `HashMap<path, LocalEntry { is_dir, mtime, size }>`. Paths are NFC-normalized.
- [x] 10. `sync/remote.rs`:
  - Recursive Depth-1 PROPFIND starting at root. Only descend into directories whose etag differs from the DB. For unchanged directories, reuse the DB rows under that prefix as the remote state.
  - Two remote names that differ only by case: keep the first, log a warning, and skip the rest (APFS is case-insensitive).
- [x] 11. `sync/reconcile.rs`: a pure function `fn decide(local: Option<&LocalEntry>, remote: Option<&RemoteEntry>, base: Option<&DbEntry>) -> Action`.
  - Change detection:
    - Local changed = no base, or mtime or size differs from base.
    - Remote changed = no base, or etag differs from base.
  - `Action`: `None`, `Upload`, `Download`, `MkdirLocal`, `MkdirRemote`, `DeleteRemote`, `TrashLocal`, `Conflict`, `RecordOnly`, `ForgetBase`.
  - Apply the rules from Decisions.
  - Directory rule: a remote-deleted directory that contains local changes or new local files is not trashed. Re-upload the changed and new local files, and trash only the unchanged ones.
  - Write table-driven unit tests covering every combination of {absent, unchanged, changed} × local/remote/base, plus the dir cases.
- [x] 12. `sync/execute.rs`:
  - Order of work:
    1. Create directories, parents first.
    2. Transfers, up to 4 at once (tokio `Semaphore`).
    3. Deletions, children first.
  - Update the DB after each successful item.
  - Downloads:
    1. Write `<dir>/.ncsync-<uuid>.part`.
    2. Before renaming over the target, re-stat it. If it changed since the scan → conflict path.
    3. Set mtime with `filetime`.
    4. Record the GET etag.
  - Uploads: re-stat the file after upload. If mtime or size changed during upload, flag a re-run.
  - A per-item error is logged and skipped. A 401 aborts the run with status `Login required`.
- [x] 13. `sync/mod.rs`, the engine:
  - A tokio task owning the triggers from Decisions. `notify-debouncer-full` watches `local_root`; ignored names don't trigger anything.
  - `Status` enum: `NotConfigured`, `Idle` ("Up to date"), `Syncing{done,total}`, `Paused`, `Offline`, `LoginRequired`, `Error(String)`.
  - Push every status change to the tray and emit `status` to the frontend.
  - Safety checks:
    - If `local_root` doesn't exist → `Error("Sync folder missing")` and do nothing. Never recreate it.
    - If a run would delete every tracked entry on either side (and there are tracked entries) → pause and show a dialog-plugin confirm: "All files would be deleted on <side>. Continue?" Proceed only on yes.
  - Network errors → `Offline`; retry on the next tick.
  - Pause stops new work after the in-flight items finish. Persist `paused` in config.
- [x] 14. `tray.rs`:
  - Monochrome template icon (`icon_as_template(true)`) and the menu from Decisions.
  - The status line updates live. `Pause`/`Resume` toggles its label. `Open folder` uses the opener plugin.

### Settings window
- [x] 15. Tauri commands in `lib.rs`:
  - `get_state() -> { server_url, login_name, local_root, autostart, status }`
  - `start_login(server_url)`
  - `cancel_login()`
  - `logout()`
  - `set_local_root(path)`: if it differs from the current folder, confirm in the UI, stop the engine, `db.clear()`, save, restart. Never move or delete files in the old folder.
  - `set_autostart(bool)`
- [x] 16. Frontend (`index.html`, `src/main.ts`, `src/styles.css`): the settings form from Decisions.
  - Use `@tauri-apps/plugin-dialog` `open({directory:true})` for `Choose…`.
  - Show the live status from the `status` event.
  - Show errors inline.
  - Use native-looking system fonts and keep it small.

### Docs
- [x] 17. Write `README.md`:
  - Build and install steps (copy `NCSync.app` to `/Applications`), with fish-compatible commands.
  - Where logs, config, and state live.
  - The known limitations listed below.

## Acceptance criteria
- `cargo test` and `cargo clippy -- -D warnings` pass. `bun run tauri build --bundles app --target aarch64-apple-darwin` produces `NCSync.app`.
- On launch, there's no Dock icon, a tray icon appears, and the settings window opens on first run.
- Log in via the browser → it shows "Logged in as …". The app password appears in Keychain Access under `com.bryan.ncsync`, and the device shows up in Nextcloud → Settings → Security.
- Choose an empty folder → the whole account downloads. File mtimes match the server.
- Create, edit, or delete locally → reflected on the server within about 5 s. Deleted files are in the server trash bin.
- Create, edit, or delete on the web UI → reflected locally within about 35 s. Deleted files are in the macOS Trash.
- A file over 100 MB uploads via chunks (`/remote.php/dav/uploads/` requests appear in the log) and arrives intact (same size and checksum).
- Edit the same file on both sides while paused, then resume → the server version keeps the name and `name (conflicted copy …).ext` exists locally only.
- Rename or move the sync folder while the app runs → `Sync folder missing` and nothing is deleted on the server.
- Toggling `Start at login` creates or removes `~/Library/LaunchAgents/NCSync.plist`, and the app launches after logout/login.
- Pull the network → `Offline`; restore it → resumes on its own.
- Log out → sync stops, the Keychain entry is gone, and local files are untouched.

## Out of scope
- Selective sync, multiple accounts or folders, virtual files / Files On Demand, Finder badges or extensions.
- Sharing, Talk, activity, notifications, `notify_push`, and E2E encryption.
- Rename/move detection, bandwidth limits, proxy settings, and an ignore-list UI.
- Signing, notarization, auto-update, Intel/universal builds, and a DMG installer.
- Any dependency not listed in Context.

## Notes / risks
- **Data loss:** never delete without the base record proving the file was synced before. Remote deletes always go to the macOS Trash, never `remove_file`. Keep the safety checks in task 13.
- **Unicode:** macOS may return NFD names and Nextcloud expects NFC. Always normalize to NFC for DB keys and remote paths, and map back to the real on-disk name for local file operations.
- **Mtime:** compare in whole seconds; the server stores seconds.
- **Keychain prompts:** an ad-hoc signature changes on every rebuild, so macOS may ask to allow Keychain access after a rebuild. This is expected; note it in the README.
- **Autostart:** the LaunchAgent stores the app's path. Install to `/Applications` before enabling it.
- **Renames:** with no rename detection, renaming a big folder re-uploads it and puts the old copy in the server trash. Document this as a known limitation.
- **Test server (optional):** for a local test instance, run `docker run -d -p 8080:80 -e SQLITE_DATABASE=nc -e NEXTCLOUD_ADMIN_USER=admin -e NEXTCLOUD_ADMIN_PASSWORD=admin nextcloud:35-apache` and use `http://localhost:8080`.
