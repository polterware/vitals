//! `VITALS_SHOTS=<dir>`: a tour that captures each page to a PNG and quits. It shows the
//! hosts list, connects every saved host that needs no password, and captures the list with
//! its metrics, each host's tabs, the connect dialog, and Settings.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, AsyncWindowContext, Entity, Window};
use polterware_ui::window::capture::save_window_png;

use crate::state::{AppState, HostTab, Route};
use crate::store::AuthMode;

pub fn shots_dir() -> Option<PathBuf> {
    std::env::var_os("VITALS_SHOTS").map(PathBuf::from)
}

pub fn start_tour(dir: PathBuf, state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let _ = std::fs::create_dir_all(&dir);
            settle(cx, 1500).await;
            shot(cx, &dir, "01-hosts").await;
            let _ = cx.update(|_, app| {
                state.update(app, |state, cx| {
                    let servers: Vec<_> = state
                        .servers()
                        .iter()
                        .filter(|server| server.auth_mode != AuthMode::Password)
                        .cloned()
                        .collect();
                    for server in &servers {
                        state.connect_saved(server, None, cx);
                    }
                    state.go(Route::Hosts);
                    cx.notify();
                })
            });
            settle(cx, 5000).await;
            shot(cx, &dir, "02-hosts-live").await;
            let ids: Vec<u64> = cx
                .update(|_, app| state.read(app).sessions().iter().map(|s| s.id).collect())
                .unwrap_or_default();
            for (index, id) in ids.iter().enumerate() {
                for (tab, name) in [
                    (HostTab::Metrics, "metrics"),
                    (HostTab::Terminal, "terminal"),
                ] {
                    go(cx, &state, Route::Host(*id, tab));
                    shot(cx, &dir, &format!("03-host{index}-{name}")).await;
                }
            }
            go(cx, &state, Route::Hosts);
            let _ = cx.update(|_, app| {
                state.update(app, |state, cx| {
                    state.open_connect(None);
                    cx.notify();
                })
            });
            shot(cx, &dir, "04-connect").await;
            let _ = cx.update(|_, app| {
                state.update(app, |state, cx| {
                    state.close_connect();
                    cx.notify();
                })
            });
            go(cx, &state, Route::Settings);
            shot(cx, &dir, "05-settings").await;
            let _ = cx.update(|_, app| app.quit());
        })
        .detach();
}

fn go(cx: &mut AsyncWindowContext, state: &Entity<AppState>, route: Route) {
    let _ = cx.update(|_, app| {
        state.update(app, |state, cx| {
            state.go(route);
            cx.notify();
        })
    });
}

async fn shot(cx: &mut AsyncWindowContext, dir: &Path, name: &str) {
    let _ = cx.update(|window, app| {
        app.activate(true);
        window.activate_window();
        window.refresh();
    });
    settle(cx, 1200).await;
    let path = dir.join(format!("{name}.png"));
    let _ = cx.update(|window, _| {
        if let Err(error) = save_window_png(window, &path, None) {
            eprintln!("shot {}: {error}", path.display());
        }
    });
}

async fn settle(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}
