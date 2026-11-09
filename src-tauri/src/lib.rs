mod auth;
mod config;
mod db;
mod ignore;
mod sync;
mod tray;
mod webdav;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

use config::Config;
use db::Db;
use sync::{Engine, EngineParams, Status, StatusDto};
use webdav::Dav;

pub struct AppState {
    config_dir: PathBuf,
    config: Mutex<Config>,
    db: Arc<Mutex<Db>>,
    status: Mutex<Status>,
    engine: Mutex<Option<Engine>>,
    login: Mutex<Option<tokio::task::AbortHandle>>,
    paused: Arc<AtomicBool>,
    client: reqwest::Client,
}

impl AppState {
    fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    fn update_config(&self, f: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut cfg = self.config.lock().unwrap();
        f(&mut cfg);
        cfg.save(&self.config_dir)
            .map_err(|e| format!("Cannot save config: {e}"))
    }
}

fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}

// ---------- status / tray ----------

fn set_status(app: &AppHandle, s: Status) {
    let st = state(app);
    let kind_changed = {
        let mut cur = st.status.lock().unwrap();
        if *cur == s {
            return;
        }
        let changed = std::mem::discriminant(&*cur) != std::mem::discriminant(&s);
        *cur = s.clone();
        changed
    };
    if let Some(t) = app.try_state::<tray::Tray>() {
        let _ = t.status.set_text(s.label());
    }
    // The icon depends only on the kind of status, not on progress counts.
    if kind_changed {
        tray::set_icon(app, &s);
    }
    let _ = app.emit("status", s.dto());
}

fn set_paused(app: &AppHandle, paused: bool) {
    let st = state(app);
    st.paused.store(paused, Ordering::SeqCst);
    if let Err(e) = st.update_config(|c| c.paused = paused) {
        tracing::error!("{e}");
    }
    if let Some(t) = app.try_state::<tray::Tray>() {
        let _ = t.pause.set_text(tray::pause_label(paused));
    }
}

pub fn sync_now(app: &AppHandle) {
    if let Some(e) = state(app).engine.lock().unwrap().as_ref() {
        e.sync_now();
    }
}

pub fn toggle_pause(app: &AppHandle) {
    let paused = !state(app).paused.load(Ordering::SeqCst);
    set_paused(app, paused);
    if paused {
        // In-flight items finish; the engine reports Paused when its run ends.
        if !matches!(*state(app).status.lock().unwrap(), Status::Syncing { .. }) {
            set_status(app, Status::Paused);
        }
    } else {
        sync_now(app);
    }
}

pub fn open_folder(app: &AppHandle) {
    if let Some(root) = state(app).config().local_root
        && let Err(e) = app.opener().open_path(root.to_string_lossy(), None::<&str>)
    {
        tracing::error!("open folder: {e}");
    }
}

/// While the settings window is open the app is a regular app (Dock icon, ⌘Tab).
fn set_regular(app: &AppHandle, regular: bool) {
    #[cfg(target_os = "macos")]
    {
        let policy = if regular {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };
        let _ = app.set_activation_policy(policy);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, regular);
}

pub fn show_settings(app: &AppHandle) {
    set_regular(app, true);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// ---------- engine lifecycle ----------

fn stop_engine(app: &AppHandle) {
    // Dropping the engine aborts its task.
    state(app).engine.lock().unwrap().take();
}

async fn restart_engine(app: &AppHandle) {
    stop_engine(app);
    let cfg = state(app).config();
    let (Some(server), Some(login), Some(user_id), Some(root)) = (
        cfg.server_url.clone(),
        cfg.login_name.clone(),
        cfg.user_id.clone(),
        cfg.local_root.clone(),
    ) else {
        set_status(app, Status::NotConfigured);
        return;
    };
    let (l, s) = (login.clone(), server.clone());
    let password = match tokio::task::spawn_blocking(move || auth::get_password(&l, &s)).await {
        Ok(Ok(pw)) => pw,
        other => {
            tracing::error!("keychain: {other:?}");
            set_status(app, Status::LoginRequired);
            return;
        }
    };
    let st = state(app);
    let dav = Arc::new(Dav::new(
        st.client.clone(),
        &server,
        &user_id,
        &login,
        &password,
    ));
    let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app.clone());
    let params = EngineParams {
        dav,
        db: st.db.clone(),
        root,
        paused: st.paused.clone(),
        status: Arc::new(move |s| set_status(&a1, s)),
        set_paused: Arc::new(move |p| set_paused(&a2, p)),
        confirm: Arc::new(move |msg| {
            a3.dialog()
                .message(msg)
                .title("NCSync")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Continue".into(),
                    "Stay paused".into(),
                ))
                .blocking_show()
        }),
        activity: Arc::new(move |s| {
            let _ = a4.emit("activity", s);
        }),
    };
    *st.engine.lock().unwrap() = Some(Engine::start(params));
    tracing::info!("engine started for {login} on {server}");
}

// ---------- commands ----------

#[derive(Serialize)]
struct StateDto {
    server_url: Option<String>,
    login_name: Option<String>,
    host: Option<String>,
    local_root: Option<String>,
    autostart: bool,
    status: StatusDto,
}

fn state_dto(app: &AppHandle) -> StateDto {
    let st = state(app);
    let cfg = st.config();
    let logged_in = cfg.is_logged_in();
    StateDto {
        host: cfg
            .server_url
            .as_deref()
            .and_then(|s| reqwest::Url::parse(s).ok())
            .and_then(|u| u.host_str().map(str::to_string)),
        server_url: cfg.server_url,
        login_name: if logged_in { cfg.login_name } else { None },
        local_root: cfg.local_root.map(|p| p.to_string_lossy().into_owned()),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        status: st.status.lock().unwrap().dto(),
    }
}

#[tauri::command]
fn get_state(app: AppHandle) -> StateDto {
    state_dto(&app)
}

#[tauri::command]
async fn start_login(app: AppHandle, server_url: String) -> Result<StateDto, String> {
    let server = auth::normalize_server_url(&server_url)?;
    if state(&app).config().is_logged_in() {
        return Err("Log out first".into());
    }
    state(&app).update_config(|c| c.server_url = Some(server.clone()))?;

    let client = state(&app).client.clone();
    let (a, s) = (app.clone(), server.clone());
    let task = tokio::spawn(async move {
        auth::login_flow(&client, &s, |url| {
            a.opener()
                .open_url(url, None::<&str>)
                .map_err(|e| format!("Cannot open browser: {e}"))
        })
        .await
    });
    if let Some(old) = state(&app)
        .login
        .lock()
        .unwrap()
        .replace(task.abort_handle())
    {
        old.abort();
    }
    let creds = match task.await {
        Ok(r) => r?,
        Err(e) if e.is_cancelled() => return Err("Login cancelled".into()),
        Err(e) => return Err(e.to_string()),
    };
    state(&app).login.lock().unwrap().take();

    let (l, s, pw) = (
        creds.login_name.clone(),
        server.clone(),
        creds.app_password.clone(),
    );
    tokio::task::spawn_blocking(move || auth::set_password(&l, &s, &pw))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Cannot save to Keychain: {e}"))?;
    state(&app).update_config(|c| {
        c.server_url = Some(server.clone());
        c.login_name = Some(creds.login_name.clone());
        c.user_id = Some(creds.user_id.clone());
    })?;
    tracing::info!(
        "logged in as {} (id {}) on {server}",
        creds.login_name,
        creds.user_id
    );
    restart_engine(&app).await;
    Ok(state_dto(&app))
}

#[tauri::command]
fn cancel_login(app: AppHandle) {
    if let Some(h) = state(&app).login.lock().unwrap().take() {
        h.abort();
    }
}

#[tauri::command]
async fn logout(app: AppHandle) -> Result<StateDto, String> {
    cancel_login(app.clone());
    stop_engine(&app);
    let cfg = state(&app).config();
    if let (Some(server), Some(login)) = (cfg.server_url.clone(), cfg.login_name.clone()) {
        let (l, s) = (login.clone(), server.clone());
        if let Ok(Ok(pw)) = tokio::task::spawn_blocking(move || auth::get_password(&l, &s)).await {
            auth::revoke(&state(&app).client, &server, &login, &pw).await;
        }
        let (l, s) = (login.clone(), server.clone());
        match tokio::task::spawn_blocking(move || auth::delete_password(&l, &s)).await {
            Ok(Ok(())) => {}
            other => tracing::error!("keychain delete: {other:?}"),
        }
    }
    state(&app).update_config(|c| {
        c.login_name = None;
        c.user_id = None;
    })?;
    state(&app)
        .db
        .lock()
        .unwrap()
        .clear()
        .map_err(|e| e.to_string())?;
    tracing::info!("logged out");
    set_status(&app, Status::NotConfigured);
    Ok(state_dto(&app))
}

#[tauri::command]
async fn set_local_root(app: AppHandle, path: String) -> Result<StateDto, String> {
    let p = PathBuf::from(&path);
    if !p.is_absolute() || !p.is_dir() {
        return Err("Folder does not exist".into());
    }
    if state(&app).config().local_root.as_deref() == Some(Path::new(&p)) {
        return Ok(state_dto(&app));
    }
    stop_engine(&app);
    state(&app)
        .db
        .lock()
        .unwrap()
        .clear()
        .map_err(|e| e.to_string())?;
    state(&app).update_config(|c| c.local_root = Some(p.clone()))?;
    tracing::info!("sync folder set to {}", p.display());
    restart_engine(&app).await;
    Ok(state_dto(&app))
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<StateDto, String> {
    let al = app.autolaunch();
    if enabled { al.enable() } else { al.disable() }.map_err(|e| e.to_string())?;
    Ok(state_dto(&app))
}

// ---------- setup ----------

static LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

fn init_logging(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(appender) = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("ncsync")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
    else {
        return;
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = LOG_GUARD.set(guard);
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_settings(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            init_logging(&app.path().app_log_dir()?);
            tracing::info!("NCSync {} starting", env!("CARGO_PKG_VERSION"));

            let config_dir = app.path().app_config_dir()?;
            let config = Config::load(&config_dir);
            let db = Db::open(&config_dir.join("state.sqlite"))?;
            let needs_setup = !config.is_logged_in() || config.local_root.is_none();
            let paused = config.paused;
            app.manage(AppState {
                config_dir,
                config: Mutex::new(config),
                db: Arc::new(Mutex::new(db)),
                status: Mutex::new(Status::NotConfigured),
                engine: Mutex::new(None),
                login: Mutex::new(None),
                paused: Arc::new(AtomicBool::new(paused)),
                client: webdav::http_client(),
            });
            tray::create(app.handle(), paused)?;
            if let Some(t) = app.try_state::<tray::Tray>() {
                let _ = t.status.set_text(Status::NotConfigured.label());
            }
            if needs_setup {
                show_settings(app.handle());
            }
            let h = app.handle().clone();
            tauri::async_runtime::spawn(async move { restart_engine(&h).await });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                set_regular(window.app_handle(), false);
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            start_login,
            cancel_login,
            logout,
            set_local_root,
            set_autostart
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
