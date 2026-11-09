use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};

use crate::sync::Status;

pub struct Tray {
    pub status: MenuItem<Wry>,
    pub pause: MenuItem<Wry>,
}

pub fn pause_label(paused: bool) -> &'static str {
    if paused { "Resume" } else { "Pause" }
}

/// Distance from point (x, y) to segment a-b.
fn seg(x: f32, y: f32, (ax, ay): Pt, (bx, by): Pt) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
}

type Pt = (f32, f32);

fn near(x: f32, y: f32, segs: &[(Pt, Pt)], w: f32) -> bool {
    segs.iter().any(|&(a, b)| seg(x, y, a, b) <= w / 2.0)
}

/// Monochrome cloud with a status badge cut out, drawn at runtime
/// (always white, not a template image).
pub fn icon(status: &Status) -> Image<'static> {
    const S: usize = 36;
    const SS: usize = 4; // supersampling
    let circles = [
        (11.0f32, 21.0f32, 7.0f32),
        (19.0, 15.0, 9.0),
        (27.0, 21.0, 6.0),
    ];
    let cloud = |x: f32, y: f32| {
        circles
            .iter()
            .any(|(cx, cy, r)| (x - cx).powi(2) + (y - cy).powi(2) <= r * r)
            || ((11.0..=27.0).contains(&x) && (15.0..=27.0).contains(&y))
    };
    // (cut out of the cloud, drawn on top)
    let badge = |x: f32, y: f32| -> (bool, bool) {
        match status {
            Status::Idle => (
                near(
                    x,
                    y,
                    &[((14.0, 19.5), (17.5, 23.0)), ((17.5, 23.0), (24.0, 15.5))],
                    3.0,
                ),
                false,
            ),
            Status::Syncing { .. } => {
                let up = [
                    ((15.5, 14.0), (15.5, 24.5)),
                    ((12.5, 17.0), (15.5, 14.0)),
                    ((18.5, 17.0), (15.5, 14.0)),
                ];
                let down = [
                    ((22.5, 14.0), (22.5, 24.5)),
                    ((19.5, 21.5), (22.5, 24.5)),
                    ((25.5, 21.5), (22.5, 24.5)),
                ];
                (near(x, y, &up, 2.4) || near(x, y, &down, 2.4), false)
            }
            Status::Paused => (
                near(
                    x,
                    y,
                    &[((16.0, 15.0), (16.0, 23.5)), ((22.0, 15.0), (22.0, 23.5))],
                    3.0,
                ),
                false,
            ),
            Status::Error(_) | Status::LoginRequired => (
                near(x, y, &[((19.0, 13.5), (19.0, 19.0))], 3.0)
                    || (x - 19.0).powi(2) + (y - 23.5).powi(2) <= 1.8f32.powi(2),
                false,
            ),
            Status::Offline => {
                let slash = [((5.0, 5.0), (31.0, 31.0))];
                (near(x, y, &slash, 5.5), near(x, y, &slash, 2.5))
            }
            Status::NotConfigured => (false, false),
        }
    };
    let mut rgba = vec![0u8; S * S * 4];
    for py in 0..S {
        for px in 0..S {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    let (cut, add) = badge(x, y);
                    if (cloud(x, y) && !cut) || add {
                        hits += 1;
                    }
                }
            }
            let i = (py * S + px) * 4;
            rgba[i..i + 3].fill(255);
            rgba[i + 3] = (hits * 255 / (SS * SS)) as u8;
        }
    }
    Image::new_owned(rgba, S as u32, S as u32)
}

/// Swap the tray icon to match the status.
pub fn set_icon(app: &AppHandle, status: &Status) {
    if let Some(t) = app.tray_by_id("main") {
        let _ = t.set_icon(Some(icon(status)));
    }
}

pub fn create(app: &AppHandle, paused: bool) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let sync = MenuItem::with_id(app, "sync_now", "Sync now", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", pause_label(paused), true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open_folder", "Open folder", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &sync,
            &pause,
            &open,
            &settings,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    TrayIconBuilder::with_id("main")
        .icon(icon(&Status::NotConfigured))
        .tooltip("NCSync")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "sync_now" => crate::sync_now(app),
            "pause" => crate::toggle_pause(app),
            "open_folder" => crate::open_folder(app),
            "settings" => crate::show_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    app.manage(Tray { status, pause });
    Ok(())
}
