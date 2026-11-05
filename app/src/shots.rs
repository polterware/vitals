//! `VITALS_SHOTS=<dir>`: a tour that captures each page to a PNG and quits. It shows the
//! hosts list, connects every saved host that needs no password, and captures the list with
//! its metrics, each host's tabs, the connect dialog, a kill's confirmation (asked, never
//! sent), each section of Settings, and then the connect dialog's own connect to the first of
//! those hosts: connecting, its host key asked over it when the data folder has not seen the
//! key (rejected once, so the dialog shows the failure, then trusted), and done.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, AsyncWindowContext, Entity, Window};
use polterware_ui::window::capture::save_window_png;

use crate::ssh::Target;
use crate::state::{AppState, Confirm, HostTab, Route, SettingsSection};
use crate::store::{AuthMode, Server};

pub fn shots_dir() -> Option<PathBuf> {
    std::env::var_os("VITALS_SHOTS").map(PathBuf::from)
}

/// `VITALS_HOLD=<seconds>`: connects every saved host that needs no password, shows the first
/// one's metrics, and quits after that long, so a run can be measured from outside.
pub fn hold_seconds() -> Option<u64> {
    std::env::var("VITALS_HOLD").ok()?.parse().ok()
}

pub fn start_hold(seconds: u64, state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let _ = cx.update(|_, app| {
                state.update(app, |state, cx| {
                    connect_saved(state, cx);
                    cx.notify();
                })
            });
            settle(cx, seconds * 1000).await;
            let _ = cx.update(|_, app| app.quit());
        })
        .detach();
}

/// Connects every saved host that needs no password, showing the first.
fn connect_saved(state: &mut AppState, cx: &mut gpui::Context<AppState>) {
    let servers: Vec<_> = state
        .servers()
        .iter()
        .filter(|server| server.auth_mode != AuthMode::Password)
        .cloned()
        .collect();
    let first = servers
        .iter()
        .map(|server| state.connect_saved(server, None, cx))
        .next();
    for server in servers.iter().skip(1) {
        state.connect_saved(server, None, cx);
    }
    if let Some(id) = first {
        state.go(Route::Host(id, HostTab::Metrics));
    }
}

pub fn start_tour(dir: PathBuf, state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let _ = std::fs::create_dir_all(&dir);
            settle(cx, 1500).await;
            shot(cx, &dir, "01-hosts").await;
            let _ = cx.update(|_, app| {
                state.update(app, |state, cx| {
                    connect_saved(state, cx);
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
            if let Some(id) = ids.first().copied() {
                go(cx, &state, Route::Host(id, HostTab::Metrics));
                let pid = cx
                    .update(|_, app| {
                        state
                            .read(app)
                            .session(id)
                            .and_then(|session| session.snapshot.as_ref())
                            .and_then(|snapshot| snapshot.processes.first())
                            .map(|process| process.pid)
                    })
                    .ok()
                    .flatten();
                if let Some(pid) = pid {
                    let _ = cx.update(|_, app| {
                        state.update(app, |state, cx| {
                            state.ask(Confirm::Kill { session: id, pid });
                            cx.notify();
                        })
                    });
                    shot(cx, &dir, "05-kill-confirm").await;
                    let _ = cx.update(|_, app| {
                        state.update(app, |state, cx| {
                            state.cancel_confirm();
                            cx.notify();
                        })
                    });
                }
            }
            for (index, section) in SettingsSection::ALL.into_iter().enumerate() {
                let _ = cx.update(|_, app| {
                    state.update(app, |state, cx| {
                        state.open_settings(section);
                        cx.notify();
                    })
                });
                shot(cx, &dir, &format!("06-settings-{index}-{}", section.id())).await;
            }
            let _ = cx.update(|_, app| {
                state.update(app, |state, cx| {
                    state.close_settings();
                    cx.notify();
                })
            });
            attempt_shots(cx, &dir, &state).await;
            let _ = cx.update(|_, app| app.quit());
        })
        .detach();
}

/// The connect dialog connecting to the first saved host that needs no password.
async fn attempt_shots(cx: &mut AsyncWindowContext, dir: &Path, state: &Entity<AppState>) {
    let server = cx
        .update(|_, app| {
            state
                .read(app)
                .servers()
                .iter()
                .find(|server| server.auth_mode != AuthMode::Password)
                .cloned()
        })
        .ok()
        .flatten();
    let Some(server) = server else {
        return;
    };
    // The tabs are done with, so the only session is the dialog's.
    let _ = cx.update(|_, app| {
        state.update(app, |state, cx| {
            let open: Vec<u64> = state.tabs().map(|session| session.id).collect();
            for id in open {
                state.close_session(id);
            }
            cx.notify();
        })
    });
    begin(cx, state, &server);
    quick_shot(cx, dir, "07-connect-connecting").await;
    if wait_for_key(cx, state).await {
        shot(cx, dir, "07-connect-host-key").await;
        answer_key(cx, state, false);
        shot(cx, dir, "07-connect-failed").await;
        begin(cx, state, &server);
        if wait_for_key(cx, state).await {
            answer_key(cx, state, true);
            quick_shot(cx, dir, "07-connect-trusted").await;
        }
    }
    for _ in 0..200 {
        let connecting = cx
            .update(|_, app| state.read(app).connecting())
            .unwrap_or(false);
        if !connecting {
            break;
        }
        settle(cx, 100).await;
    }
    shot(cx, dir, "07-connect-done").await;
}

/// Opens the dialog on `server` and connects from it, as Connect does.
fn begin(cx: &mut AsyncWindowContext, state: &Entity<AppState>, server: &Server) {
    let _ = cx.update(|_, app| {
        state.update(app, |state, cx| {
            state.open_connect(Some(server.clone()));
            let target = Target {
                host: server.host.clone(),
                port: server.port,
                username: server.username.clone(),
                auth_mode: server.auth_mode,
                private_key_path: server.private_key_path.clone(),
                secret: None,
            };
            let label = server.display_label().to_string();
            state.begin_attempt(label, target, Some(server.id.clone()), None, cx);
            cx.notify();
        })
    });
}

/// Waits up to 15s for the attempt to ask for its host key, or to end.
async fn wait_for_key(cx: &mut AsyncWindowContext, state: &Entity<AppState>) -> bool {
    for _ in 0..150 {
        let (asked, connecting) = cx
            .update(|_, app| {
                let state = state.read(app);
                (state.host_key_prompt().is_some(), state.connecting())
            })
            .unwrap_or((false, false));
        if asked || !connecting {
            return asked;
        }
        settle(cx, 100).await;
    }
    false
}

fn answer_key(cx: &mut AsyncWindowContext, state: &Entity<AppState>, trust: bool) {
    let _ = cx.update(|_, app| {
        state.update(app, |state, cx| {
            if let Some(id) = state.host_key_prompt().map(|session| session.id) {
                if trust {
                    state.approve_host_key(id);
                } else {
                    state.reject_host_key(id);
                }
            }
            cx.notify();
        })
    });
}

/// A shot after a short settle, to catch a step that passes quickly.
async fn quick_shot(cx: &mut AsyncWindowContext, dir: &Path, name: &str) {
    settle(cx, 150).await;
    let path = dir.join(format!("{name}.png"));
    let _ = cx.update(|window, _| {
        window.refresh();
        if let Err(error) = save_window_png(window, &path, None) {
            eprintln!("shot {}: {error}", path.display());
        }
    });
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
