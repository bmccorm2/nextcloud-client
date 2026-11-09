# NCSync

A tiny macOS menu bar app that keeps two-way sync between your whole Nextcloud account and one local folder. Tauri 2 + Rust. Apple Silicon, macOS 13+.

## Build and install

Needs [Rust](https://rustup.rs) and [Bun](https://bun.sh).

```fish
bun install
bun run tauri build --bundles app --target aarch64-apple-darwin
cp -R src-tauri/target/aarch64-apple-darwin/release/bundle/macos/NCSync.app /Applications/
open /Applications/NCSync.app
```

The app is ad-hoc signed and not notarized. If Gatekeeper blocks it, run:

```fish
xattr -dr com.apple.quarantine /Applications/NCSync.app
```

On first launch the settings window opens. Enter your server URL, click **Log in**, approve in the browser, then pick a local folder.

Install to `/Applications` **before** turning on **Start at login**. The LaunchAgent (`~/Library/LaunchAgents/NCSync.plist`) stores the app's path.

## Development

```fish
bun run tauri dev                              # run
cd src-tauri; cargo test                       # unit tests
cd src-tauri; cargo clippy -- -D warnings      # lint
```

End-to-end tests (optional) run against a throwaway local server:

```fish
docker run -d --name ncsync-test -p 8080:80 -e SQLITE_DATABASE=nc -e NEXTCLOUD_ADMIN_USER=admin -e NEXTCLOUD_ADMIN_PASSWORD=admin nextcloud:35-apache
cd src-tauri; env NCSYNC_E2E_URL=http://localhost:8080 cargo test e2e -- --ignored --nocapture
```

These tests move a few small `e2e-*` files to your macOS Trash.

## Where things live

| What | Path |
|---|---|
| Config | `~/Library/Application Support/com.bryan.ncsync/config.json` |
| Sync state | `~/Library/Application Support/com.bryan.ncsync/state.sqlite` |
| Logs (daily, 7 kept) | `~/Library/Logs/com.bryan.ncsync/` |
| App password | Keychain, service `com.bryan.ncsync`, account `<login>@<host>` |

Nothing is written into the sync folder except synced files and temporary `.ncsync-*.part` files.

## How it syncs

- **Triggers:** local changes (2 s debounce), a server check every 30 s, a full sync every hour, and **Sync now**.
- **Deleted on the server:** the local copy goes to the macOS Trash.
- **Deleted locally:** deleted on the server, where it lands in Nextcloud's trash bin.
- **Edited on both sides:** the server version keeps the name. Your version is kept as `name (conflicted copy YYYY-MM-DD HHMMSS).ext`, which stays local only.
- **Edit vs delete:** the edit wins.
- **Safety:** if the sync folder is missing, nothing happens. If a sync would delete every file on one side, NCSync pauses and asks first.
- **Never synced:** `.DS_Store`, `._*`, `.~lock*`, `*.part`, `Icon\r`, `.ncsync-*`, conflicted copies, and symlinks.

## Known limitations

- One account and one folder. No selective sync, virtual files, Finder badges, sharing, or E2E encryption.
- **No rename detection.** A rename or move is a delete plus a create. Renaming a big folder re-uploads it and puts the old copy in the server trash bin.
- If two server items differ only by letter case, only the first is synced (APFS is case-insensitive).
- **Keychain prompts after rebuilds:** the ad-hoc signature changes on every build, so macOS may ask to allow Keychain access again. Click **Always Allow**.
- After a `401`, automatic syncing stops until you click **Sync now** (or log out and in again), to avoid server login throttling.
- Apple Silicon only. No auto-update, no notarization.
