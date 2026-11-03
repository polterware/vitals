//! What the window shows: the saved hosts, the open sessions with their latest metrics, the
//! page, and the dialogs. Sessions run on the SSH runtime; their events land here.

use std::collections::VecDeque;

use futures::StreamExt;
use gpui::{Context, SharedString};
use polterware_ui::toast::{Toast, ToastTone};

use crate::protocol::{Hello, Snapshot};
use crate::session::{self, Reconnect, SessionEvent, SessionHandle, Status};
use crate::ssh::Target;
use crate::store::{Server, Store};

/// Seconds of CPU history a host keeps (the Electron app's default window, 2 minutes).
pub const HISTORY: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostTab {
    Metrics,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Hosts,
    Host(u64, HostTab),
    Settings,
}

pub struct HostSession {
    pub id: u64,
    pub label: String,
    pub target: Target,
    /// The saved host this session opened, if any.
    pub server_id: Option<String>,
    /// Saved once the session first connects.
    pub save: Option<Server>,
    pub status: Status,
    pub message: Option<String>,
    pub hello: Option<Hello>,
    pub snapshot: Option<Snapshot>,
    pub cpu_history: VecDeque<f32>,
    pub host_key: Option<String>,
    handle: SessionHandle,
}

impl HostSession {
    pub fn key(&self) -> String {
        format!(
            "{}@{}:{}",
            self.target.username, self.target.host, self.target.port
        )
    }
}

/// A row of the hosts list: a saved host, an open session, or both.
#[derive(Clone)]
pub struct HostRow {
    pub label: String,
    pub address: String,
    pub server: Option<Server>,
    pub session: Option<u64>,
}

pub struct AppState {
    store: Store,
    servers: Vec<Server>,
    sessions: Vec<HostSession>,
    route: Route,
    next_id: u64,
    connect_open: bool,
    /// The saved host the connect dialog edits, if any.
    connect_server: Option<Server>,
    toasts: Vec<Toast>,
    next_toast: u64,
}

impl AppState {
    pub fn new() -> Self {
        let store = Store::open();
        let servers = store.servers();
        Self {
            store,
            servers,
            sessions: Vec::new(),
            route: Route::Hosts,
            next_id: 1,
            connect_open: false,
            connect_server: None,
            toasts: Vec::new(),
            next_toast: 1,
        }
    }

    pub fn route(&self) -> Route {
        self.route
    }

    pub fn go(&mut self, route: Route) {
        self.route = route;
    }

    pub fn sessions(&self) -> &[HostSession] {
        &self.sessions
    }

    pub fn session(&self, id: u64) -> Option<&HostSession> {
        self.sessions.iter().find(|session| session.id == id)
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    pub fn connect_open(&self) -> bool {
        self.connect_open
    }

    pub fn connect_server(&self) -> Option<&Server> {
        self.connect_server.as_ref()
    }

    pub fn open_connect(&mut self, server: Option<Server>) {
        self.connect_server = server;
        self.connect_open = true;
    }

    pub fn close_connect(&mut self) {
        self.connect_open = false;
        self.connect_server = None;
    }

    /// The first session waiting for its host key to be approved.
    pub fn host_key_prompt(&self) -> Option<&HostSession> {
        self.sessions
            .iter()
            .find(|session| session.host_key.is_some())
    }

    /// Saved hosts first, in the file's order, then the open sessions of unsaved hosts.
    pub fn host_rows(&self) -> Vec<HostRow> {
        let mut rows: Vec<HostRow> = self
            .servers
            .iter()
            .map(|server| HostRow {
                label: server.display_label().to_string(),
                address: server.key(),
                server: Some(server.clone()),
                session: self
                    .sessions
                    .iter()
                    .find(|session| {
                        session.server_id.as_deref() == Some(server.id.as_str())
                            || session.key() == server.key()
                    })
                    .map(|session| session.id),
            })
            .collect();
        for session in &self.sessions {
            if rows.iter().any(|row| row.session == Some(session.id)) {
                continue;
            }
            rows.push(HostRow {
                label: session.label.clone(),
                address: session.key(),
                server: None,
                session: Some(session.id),
            });
        }
        rows
    }

    pub fn toast(&mut self, message: impl Into<SharedString>, danger: bool) {
        let id = self.next_toast;
        self.next_toast += 1;
        let tone = if danger {
            ToastTone::Danger
        } else {
            ToastTone::Info
        };
        self.toasts.push(Toast {
            id,
            message: message.into(),
            tone,
        });
    }

    pub fn dismiss_toast(&mut self, id: u64) {
        self.toasts.retain(|toast| toast.id != id);
    }

    /// Opens a session to `target` and shows its page. `save` is kept once it connects.
    pub fn connect(
        &mut self,
        label: String,
        target: Target,
        server_id: Option<String>,
        save: Option<Server>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let (handle, mut events) =
            session::start(target.clone(), self.store.clone(), Reconnect::default());
        self.sessions.push(HostSession {
            id,
            label,
            target,
            server_id,
            save,
            status: Status::Connecting,
            message: None,
            hello: None,
            snapshot: None,
            cpu_history: VecDeque::with_capacity(HISTORY),
            host_key: None,
            handle,
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this
                    .update(cx, |state, cx| {
                        state.apply(id, event);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        self.route = Route::Host(id, HostTab::Metrics);
        id
    }

    /// Connects a saved host. Its password or passphrase, which only the Electron app could
    /// unseal, comes from `secret`.
    pub fn connect_saved(
        &mut self,
        server: &Server,
        secret: Option<String>,
        cx: &mut Context<Self>,
    ) -> u64 {
        if let Some(open) = self
            .sessions
            .iter()
            .find(|session| session.server_id.as_deref() == Some(server.id.as_str()))
        {
            let id = open.id;
            self.route = Route::Host(id, HostTab::Metrics);
            return id;
        }
        let target = Target {
            host: server.host.clone(),
            port: server.port,
            username: server.username.clone(),
            auth_mode: server.auth_mode,
            private_key_path: server.private_key_path.clone(),
            secret,
        };
        self.connect(
            server.display_label().to_string(),
            target,
            Some(server.id.clone()),
            None,
            cx,
        )
    }

    fn apply(&mut self, id: u64, event: SessionEvent) {
        let mut saved = None;
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return;
        };
        match event {
            SessionEvent::Status(status, message) => {
                if status == Status::Connected
                    && let Some(server) = session.save.take()
                {
                    saved = Some(server);
                }
                if status != Status::Connecting {
                    session.host_key = None;
                }
                session.status = status;
                session.message = message;
            }
            SessionEvent::HostKey(print) => session.host_key = Some(print),
            SessionEvent::Hello(hello) => session.hello = Some(hello),
            SessionEvent::Metrics(snapshot) => {
                if session.cpu_history.len() == HISTORY {
                    session.cpu_history.pop_front();
                }
                session
                    .cpu_history
                    .push_back(snapshot.cpu_total_percent as f32);
                session.snapshot = Some(*snapshot);
            }
            SessionEvent::KillResult { ok, error, .. } => {
                if !ok {
                    let message = error.unwrap_or_else(|| "kill failed".into());
                    self.toast(format!("Could not stop the process: {message}"), true);
                }
            }
            SessionEvent::Ended => {}
        }
        if let Some(server) = saved {
            match self.store.upsert_server(server) {
                Ok(server_id) => {
                    if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                        session.server_id = Some(server_id);
                    }
                    self.servers = self.store.servers();
                }
                Err(error) => self.toast(format!("Could not save the host: {error}"), true),
            }
        }
    }

    pub fn approve_host_key(&mut self, id: u64) {
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.host_key = None;
            session.handle.approve_host_key();
        }
    }

    pub fn reject_host_key(&mut self, id: u64) {
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.host_key = None;
            session.handle.reject_host_key();
        }
        self.close_session(id);
    }

    /// Ends the session and drops its tab.
    pub fn close_session(&mut self, id: u64) {
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            let session = self.sessions.remove(index);
            session.handle.disconnect();
        }
        if matches!(self.route, Route::Host(open, _) if open == id) {
            self.route = Route::Hosts;
        }
    }

    pub fn remove_server(&mut self, id: &str) {
        if let Err(error) = self.store.remove_server(id) {
            self.toast(format!("Could not remove the host: {error}"), true);
        }
        self.servers = self.store.servers();
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
