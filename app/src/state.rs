//! What the window shows: the saved hosts, the open sessions with their latest metrics and
//! history, the page, the settings, and the dialogs. Sessions run on the SSH runtime; their
//! events land here.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use futures::StreamExt;
use gpui::{AppContext as _, Context, Entity, SharedString, Subscription, Window};
use polterware_ui::patterns::{Cell, DashboardLayout, auto_arrange};
use polterware_ui::terminal::{TerminalEvent, TerminalSize, TerminalView};
use polterware_ui::toast::{Toast, ToastTone};
use serde_json::Value;

use crate::protocol::{Hello, Signal, Snapshot};
use crate::secrets::{self, OpenSaved, Secrets};
use crate::session::{self, SessionEvent, SessionEvents, SessionHandle, Status};
use crate::settings::{self, Settings, signal_name};
use crate::shell::ShellEvent;
use crate::ssh::Target;
use crate::store::{AuthMode, Server, Store};

/// Samples of history a host keeps, one a second: the longest history window (10 minutes).
/// The charts show the last `historyWindowMinutes` of it.
pub const MAX_HISTORY: usize = 600;

/// The smoothed CPU readout's time constant, close to top's 3 second window.
const DISPLAY_SMOOTHING_SECONDS: f64 = 3.0;

/// An exponential moving average for a shown value (`smoothValue`), weighted by the real gap
/// between samples, so a late sample moves it further. With no earlier value, no gap, or a
/// gap longer than five time constants (a reconnect), it is `next`.
pub fn smooth(previous: Option<f64>, next: f64, gap_seconds: f64) -> f64 {
    match previous {
        Some(previous)
            if previous.is_finite()
                && gap_seconds > 0.0
                && gap_seconds <= DISPLAY_SMOOTHING_SECONDS * 5.0 =>
        {
            let alpha = 1.0 - (-gap_seconds / DISPLAY_SMOOTHING_SECONDS).exp();
            previous + alpha * (next - previous)
        }
        _ => next,
    }
}

/// A host's history, one sample a second: total CPU and each interface's rates.
#[derive(Default)]
pub struct History {
    pub cpu: VecDeque<f32>,
    /// The CPU headline: total CPU smoothed over the samples' real gaps.
    pub cpu_display: Option<f64>,
    collected_at: Option<Instant>,
    /// Bytes per second received and sent, by interface.
    pub interfaces: BTreeMap<String, (VecDeque<f32>, VecDeque<f32>)>,
}

fn push(series: &mut VecDeque<f32>, value: f64) {
    if series.len() == MAX_HISTORY {
        series.pop_front();
    }
    series.push_back(value as f32);
}

impl History {
    fn record(&mut self, snapshot: &Snapshot) {
        let now = Instant::now();
        let gap = self
            .collected_at
            .map_or(0.0, |last| now.duration_since(last).as_secs_f64());
        self.cpu_display = Some(smooth(self.cpu_display, snapshot.cpu_total_percent, gap));
        self.collected_at = Some(now);
        push(&mut self.cpu, snapshot.cpu_total_percent);
        for interface in &snapshot.network {
            let (rx, tx) = self.interfaces.entry(interface.name.clone()).or_default();
            push(rx, interface.rx_bytes_per_sec);
            push(tx, interface.tx_bytes_per_sec);
        }
    }
}

/// The last `count` samples of `series`, oldest first.
pub fn tail(series: &VecDeque<f32>, count: usize) -> Vec<f32> {
    series
        .iter()
        .skip(series.len().saturating_sub(count))
        .copied()
        .collect()
}

/// The dashboard's cards in their order (`CARD_IDS`), with their titles (`CARD_TITLES`).
pub const CARDS: [(&str, &str); 8] = [
    ("cpu", "CPU"),
    ("memory", "Memory"),
    ("network", "Network"),
    ("diskio", "Disk I/O"),
    ("pressure", "Pressure"),
    ("disk", "Disk"),
    ("thermal", "Temperature / GPU"),
    ("processes", "Processes"),
];

/// A card's size in cells (`SIZES`); its width is clamped to the columns as it is laid out.
pub fn card_size(id: &str) -> (u16, u16) {
    match id {
        "cpu" | "network" => (1, 2),
        "diskio" | "disk" => (2, 1),
        "processes" => (2, 2),
        _ => (1, 1),
    }
}

/// The dashboard's layout as `card-layout.json` keeps it: the hidden cards, the order the
/// cards flow in, and the cells cards were dropped on (the Electron app's
/// `vitals.cardHidden`, `vitals.cardOrder`, and `vitals.cardAnchors.v3`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardLayout {
    pub hidden: Vec<String>,
    pub order: Vec<String>,
    pub anchors: BTreeMap<String, (u16, u16)>,
}

impl CardLayout {
    /// The layout in a `card-layout.json`, normalized as the Electron app's loaders do: known
    /// ids only, once each; the order with every card the file does not name appended; and
    /// anchors of whole non-negative cells.
    pub fn from_json(value: &Value) -> Self {
        let known = |id: &str| CARDS.iter().any(|(card, _)| *card == id);
        let ids = |key: &str| -> Vec<String> {
            let mut seen = Vec::new();
            for id in value
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if known(id) && !seen.iter().any(|kept| kept == id) {
                    seen.push(id.to_string());
                }
            }
            seen
        };
        let hidden = CARDS
            .iter()
            .map(|(card, _)| card.to_string())
            .filter(|card| ids("hidden").contains(card))
            .collect();
        let mut order = ids("order");
        for (card, _) in CARDS {
            if !order.iter().any(|kept| kept == card) {
                order.push(card.to_string());
            }
        }
        let cell = |value: &Value| -> Option<(u16, u16)> {
            let at = |axis: &str| {
                value
                    .get(axis)?
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
            };
            Some((at("x")?, at("y")?))
        };
        let anchors = value
            .get("anchors")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(id, _)| known(id))
            .filter_map(|(id, at)| Some((id.clone(), cell(at)?)))
            .collect();
        Self {
            hidden,
            order,
            anchors,
        }
    }

    pub fn to_json(&self) -> Value {
        let anchors: serde_json::Map<String, Value> = self
            .anchors
            .iter()
            .map(|(id, (x, y))| (id.clone(), serde_json::json!({ "x": x, "y": y })))
            .collect();
        serde_json::json!({ "hidden": self.hidden, "order": self.order, "anchors": anchors })
    }

    /// The default layout: every card, in `CARD_IDS` order, none anchored.
    pub fn reset() -> Self {
        Self::from_json(&Value::Null)
    }

    /// The order and anchors the grid packs by.
    pub fn grid(&self) -> DashboardLayout {
        DashboardLayout {
            order: self.order.iter().cloned().map(SharedString::from).collect(),
            anchors: self
                .anchors
                .iter()
                .map(|(id, (x, y))| (SharedString::from(id.clone()), Cell { x: *x, y: *y }))
                .collect(),
        }
    }

    /// Takes the grid's order and anchors, normalized.
    pub fn set_grid(&mut self, layout: &DashboardLayout) {
        let anchors: serde_json::Map<String, Value> = layout
            .anchors
            .iter()
            .map(|(id, cell)| {
                (
                    id.to_string(),
                    serde_json::json!({ "x": cell.x, "y": cell.y }),
                )
            })
            .collect();
        let order: Vec<&str> = layout.order.iter().map(|id| id.as_ref()).collect();
        let next = Self::from_json(&serde_json::json!({ "order": order, "anchors": anchors }));
        self.order = next.order;
        self.anchors = next.anchors;
    }

    /// `autoLayout`: the shown cards (`shown`, in `CARD_IDS` order) in the order that packs
    /// them best on `columns`, the rest after, and no anchors.
    pub fn auto_layout(&mut self, shown: &[&str], columns: u16) {
        let sized: Vec<(&str, u16, u16)> = shown
            .iter()
            .map(|id| {
                let (w, h) = card_size(id);
                (*id, w, h)
            })
            .collect();
        let arranged = auto_arrange(&sized, columns);
        let rest: Vec<String> = self
            .order
            .iter()
            .filter(|id| !arranged.contains(&id.as_str()))
            .cloned()
            .collect();
        self.order = arranged
            .into_iter()
            .map(str::to_string)
            .chain(rest)
            .collect();
        self.anchors.clear();
    }
}

fn read_card_layout(dir: &std::path::Path) -> CardLayout {
    let value = std::fs::read_to_string(dir.join("card-layout.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    CardLayout::from_json(&value)
}

fn write_card_layout(dir: &std::path::Path, layout: &CardLayout) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(&layout.to_json()).map_err(std::io::Error::other)?;
    let tmp = dir.join("card-layout.json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, dir.join("card-layout.json"))
}

/// How many recent hosts the start page lists (`RECENT_LIMIT`).
pub const RECENT_LIMIT: usize = 4;

/// A connect from the dialog (the Electron app's `attempts`): its session stays out of the
/// tabs until it connects, and the dialog shows how far it got or why it failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectAttempt {
    /// The session connecting, while it runs.
    pub session: Option<u64>,
    /// The latest progress its session reported.
    pub progress: Option<String>,
    /// Why the last one failed.
    pub errors: Vec<String>,
}

fn read_list(dir: &std::path::Path, file: &str, key: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(file))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get(key).cloned())
        .and_then(|list| serde_json::from_value(list).ok())
        .unwrap_or_default()
}

fn write_list(
    dir: &std::path::Path,
    file: &str,
    key: &str,
    list: &[String],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(&serde_json::json!({ key: list }))
        .map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{file}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, dir.join(file))
}

/// An action that asks first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Kill { session: u64, pid: u32 },
    Close { session: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Monitoring,
    Connections,
    Advanced,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 4] = [
        SettingsSection::General,
        SettingsSection::Monitoring,
        SettingsSection::Connections,
        SettingsSection::Advanced,
    ];

    pub fn id(self) -> &'static str {
        match self {
            SettingsSection::General => "general",
            SettingsSection::Monitoring => "monitoring",
            SettingsSection::Connections => "connections",
            SettingsSection::Advanced => "advanced",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::General => "General",
            SettingsSection::Monitoring => "Monitoring",
            SettingsSection::Connections => "Connections",
            SettingsSection::Advanced => "Advanced",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostTab {
    Metrics,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Hosts,
    Host(u64, HostTab),
}

/// A session's Terminal tab, made the first time it shows and kept, with its scrollback,
/// while the session is open (the Electron app's `terminalSessions`).
pub struct HostTerminal {
    pub view: Entity<TerminalView>,
    /// A shell is open, or asked for.
    opened: bool,
    /// The next size the view lays out opens a shell.
    open_on_size: bool,
    size: Option<TerminalSize>,
    _events: Subscription,
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
    /// Why the agent is unavailable, once the metrics come from the script fallback (the
    /// Electron app's `mode: "fallback"`).
    pub fallback: Option<String>,
    pub snapshot: Option<Snapshot>,
    pub history: History,
    pub host_key: Option<String>,
    pub terminal: Option<HostTerminal>,
    /// A connect from the dialog that has not connected yet: no tab, no page.
    pub pending: bool,
    /// Kills sent and not answered yet: their pid and signal, by command id.
    kills: HashMap<String, (u32, Signal)>,
    handle: SessionHandle,
}

impl HostSession {
    /// A session just started: its label, target, saved host, and the host to save once it
    /// connects.
    fn new(
        id: u64,
        (label, target, server_id, save): (String, Target, Option<String>, Option<Server>),
        pending: bool,
        handle: SessionHandle,
    ) -> Self {
        Self {
            id,
            label,
            target,
            server_id,
            save,
            status: Status::Connecting,
            message: None,
            hello: None,
            fallback: None,
            snapshot: None,
            history: History::default(),
            host_key: None,
            terminal: None,
            pending,
            kills: HashMap::new(),
            handle,
        }
    }

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
    /// The saved hosts' passwords and passphrases, in the Keychain.
    secrets: Secrets,
    /// Saved hosts whose secret the Keychain is being asked for, by id.
    reading_secret: HashSet<String>,
    servers: Vec<Server>,
    sessions: Vec<HostSession>,
    route: Route,
    next_id: u64,
    next_command: u64,
    connect_open: bool,
    /// The saved host the connect dialog edits, if any.
    connect_server: Option<Server>,
    /// Counts the dialog's openings, so it loads its fields once each.
    connect_opened: u64,
    attempt: ConnectAttempt,
    settings: Settings,
    /// `settings.json` as it was read, so fields this app does not know are written back.
    settings_file: Value,
    /// The open settings section, while the dialog is open.
    settings_open: Option<SettingsSection>,
    known_hosts: Vec<(String, String)>,
    /// `user@host:port` of the hosts connected last, newest first.
    recents: Vec<String>,
    /// The dashboard's hidden cards, order, and anchors.
    cards: CardLayout,
    confirm: Option<Confirm>,
    toasts: Vec<Toast>,
    next_toast: u64,
}

impl AppState {
    pub fn new() -> Self {
        let store = Store::open();
        let servers = store.servers();
        let secrets = Secrets::open(store.dir());
        forget_unlisted_secrets(&secrets, &store);
        let (settings, settings_file) = settings::load(store.dir());
        let recents = read_list(store.dir(), "recents.json", "recents");
        let cards = read_card_layout(store.dir());
        Self {
            store,
            secrets,
            reading_secret: HashSet::new(),
            servers,
            sessions: Vec::new(),
            route: Route::Hosts,
            next_id: 1,
            next_command: 1,
            connect_open: false,
            connect_server: None,
            connect_opened: 0,
            attempt: ConnectAttempt::default(),
            settings,
            settings_file,
            settings_open: None,
            known_hosts: Vec::new(),
            recents,
            cards,
            confirm: None,
            toasts: Vec::new(),
            next_toast: 1,
        }
    }

    pub fn recents(&self) -> &[String] {
        &self.recents
    }

    fn push_recent(&mut self, key: String) {
        self.recents.retain(|recent| recent != &key);
        self.recents.insert(0, key);
        self.recents.truncate(RECENT_LIMIT * 2);
        let _ = write_list(self.store.dir(), "recents.json", "recents", &self.recents);
    }

    pub fn clear_recents(&mut self) {
        self.recents.clear();
        match write_list(self.store.dir(), "recents.json", "recents", &self.recents) {
            Ok(()) => self.toast("Recent hosts cleared.", false),
            Err(error) => self.toast(format!("Could not clear the recent hosts: {error}"), true),
        }
    }

    pub fn card_hidden(&self, id: &str) -> bool {
        self.cards.hidden.iter().any(|hidden| hidden == id)
    }

    /// The order and anchors the dashboard packs by.
    pub fn card_layout(&self) -> DashboardLayout {
        self.cards.grid()
    }

    /// Keeps where the person moved the cards.
    pub fn set_card_layout(&mut self, layout: &DashboardLayout) {
        self.cards.set_grid(layout);
        self.save_cards();
    }

    /// Packs `shown` (in `CARD_IDS` order) with no holes on `columns`, and drops the anchors.
    pub fn auto_layout_cards(&mut self, shown: &[&str], columns: u16) {
        self.cards.auto_layout(shown, columns);
        self.save_cards();
    }

    /// Shows or hides a card; the last card shown stays.
    pub fn toggle_card(&mut self, id: &str, available: &[&str]) {
        if self.card_hidden(id) {
            self.cards.hidden.retain(|hidden| hidden != id);
        } else {
            let shown = available
                .iter()
                .filter(|card| !self.card_hidden(card))
                .count();
            if shown <= 1 {
                return;
            }
            self.cards.hidden.push(id.to_string());
        }
        self.save_cards();
    }

    /// Every card back, in the default order, none anchored.
    pub fn reset_cards(&mut self) {
        self.cards = CardLayout::reset();
        self.save_cards();
    }

    fn save_cards(&mut self) {
        if let Err(error) = write_card_layout(self.store.dir(), &self.cards) {
            self.toast(format!("Could not save the card layout: {error}"), true);
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Changes the settings and saves them.
    pub fn update_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        let mut next = self.settings;
        change(&mut next);
        if next == self.settings {
            return;
        }
        self.settings = next;
        match settings::save(self.store.dir(), &self.settings, &self.settings_file) {
            Ok(written) => self.settings_file = written,
            Err(error) => self.toast(format!("Could not save the settings: {error}"), true),
        }
    }

    pub fn settings_open(&self) -> Option<SettingsSection> {
        self.settings_open
    }

    pub fn open_settings(&mut self, section: SettingsSection) {
        if self.settings_open.is_none() {
            self.known_hosts = self.store.known_host_list();
        }
        self.settings_open = Some(section);
    }

    pub fn close_settings(&mut self) {
        self.settings_open = None;
    }

    pub fn known_hosts(&self) -> &[(String, String)] {
        &self.known_hosts
    }

    pub fn remove_known_host(&mut self, key: &str) {
        match self.store.remove_known_host(key) {
            Ok(()) => self.toast(format!("{key} will ask for its key again."), false),
            Err(error) => self.toast(format!("Could not remove the host key: {error}"), true),
        }
        self.known_hosts = self.store.known_host_list();
    }

    pub fn confirm(&self) -> Option<Confirm> {
        self.confirm
    }

    /// Asks `confirm` without running it, as the tour does to capture the dialog.
    pub fn ask(&mut self, confirm: Confirm) {
        self.confirm = Some(confirm);
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    /// Runs the action waiting for its confirmation.
    pub fn accept_confirm(&mut self) {
        match self.confirm.take() {
            Some(Confirm::Kill { session, pid }) => self.kill(session, pid),
            Some(Confirm::Close { session }) => self.close_session(session),
            None => {}
        }
    }

    /// Stops `pid` on the session's host with the default signal, asking first when the
    /// settings say so.
    pub fn request_kill(&mut self, session: u64, pid: u32) {
        if self.settings.confirm_before_kill {
            self.confirm = Some(Confirm::Kill { session, pid });
        } else {
            self.kill(session, pid);
        }
    }

    /// Closes the session, asking first when the settings say so and it is still live.
    pub fn request_close(&mut self, session: u64) {
        let live = self
            .session(session)
            .is_some_and(|open| !matches!(open.status, Status::Error | Status::Disconnected));
        if live && self.settings.confirm_before_close_session {
            self.confirm = Some(Confirm::Close { session });
        } else {
            self.close_session(session);
        }
    }

    fn kill(&mut self, session: u64, pid: u32) {
        let signal = self.settings.default_kill_signal;
        let id = format!("kill-{}", self.next_command);
        self.next_command += 1;
        let Some(open) = self.sessions.iter_mut().find(|open| open.id == session) else {
            return;
        };
        if open.status != Status::Connected {
            self.toast("The session is not connected.", true);
            return;
        }
        open.kills.insert(id.clone(), (pid, signal));
        open.handle.kill(pid, signal, id);
    }

    pub fn route(&self) -> Route {
        self.route
    }

    pub fn go(&mut self, route: Route) {
        if route != self.route
            && let Route::Host(id, HostTab::Terminal) = route
        {
            self.reopen_shell(id);
        }
        self.route = route;
    }

    /// The session's terminal, made now if this is its first time.
    pub fn ensure_terminal(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let session = self.sessions.iter_mut().find(|session| session.id == id)?;
        if let Some(terminal) = &session.terminal {
            return Some(terminal.view.clone());
        }
        let view = cx.new(|cx| TerminalView::new(window, cx));
        let events = cx.subscribe(&view, move |state, _, event: &TerminalEvent, _| {
            state.terminal_event(id, event);
        });
        session.terminal = Some(HostTerminal {
            view: view.clone(),
            opened: false,
            open_on_size: true,
            size: None,
            _events: events,
        });
        Some(view)
    }

    /// Typing goes to an open shell; the first size opens one, and later ones resize it.
    fn terminal_event(&mut self, id: u64, event: &TerminalEvent) {
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return;
        };
        let Some(terminal) = session.terminal.as_mut() else {
            return;
        };
        match event {
            TerminalEvent::Input(bytes) => {
                if terminal.opened {
                    session.handle.shell_input(bytes.clone());
                }
            }
            TerminalEvent::Resize(size) => {
                terminal.size = Some(*size);
                if terminal.opened {
                    session.handle.resize_shell(size.cols, size.rows);
                } else if terminal.open_on_size {
                    terminal.open_on_size = false;
                    terminal.opened = true;
                    session.handle.open_shell(size.cols, size.rows);
                }
            }
            TerminalEvent::Title(_) => {}
        }
    }

    /// Showing the tab again starts a new shell when the last one ended.
    fn reopen_shell(&mut self, id: u64) {
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return;
        };
        let Some(terminal) = session
            .terminal
            .as_mut()
            .filter(|terminal| !terminal.opened)
        else {
            return;
        };
        match terminal.size {
            Some(size) => {
                terminal.opened = true;
                session.handle.open_shell(size.cols, size.rows);
            }
            None => terminal.open_on_size = true,
        }
    }

    /// The shell's output and what happened to it, written into the terminal as the Electron
    /// app wrote them.
    fn apply_shell(&mut self, id: u64, event: ShellEvent, cx: &mut Context<Self>) {
        let Some(terminal) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .and_then(|session| session.terminal.as_mut())
        else {
            return;
        };
        let bytes = match event {
            ShellEvent::Output(bytes) => bytes,
            ShellEvent::Reconnected => b"\r\n[reconnected]\r\n".to_vec(),
            ShellEvent::Exited => {
                terminal.opened = false;
                "\r\n[shell closed \u{2014} reopen the Terminal tab to start a new one]\r\n"
                    .as_bytes()
                    .to_vec()
            }
            ShellEvent::Failed(message) => {
                terminal.opened = false;
                format!("\r\n[{message}]\r\n").into_bytes()
            }
        };
        terminal.view.update(cx, |view, cx| view.feed(&bytes, cx));
    }

    pub fn sessions(&self) -> &[HostSession] {
        &self.sessions
    }

    /// The sessions with a tab: every one but a connect the dialog is still waiting on.
    pub fn tabs(&self) -> impl Iterator<Item = &HostSession> {
        self.sessions.iter().filter(|session| !session.pending)
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
        if self.connect_open {
            return;
        }
        self.connect_server = server;
        self.connect_open = true;
        self.connect_opened += 1;
        self.attempt.errors.clear();
    }

    /// Closes the dialog, ending a connect it is waiting on.
    pub fn close_connect(&mut self) {
        self.cancel_attempt();
        self.attempt.errors.clear();
        self.connect_open = false;
        self.connect_server = None;
    }

    /// How many times the dialog has opened.
    pub fn connect_opened(&self) -> u64 {
        self.connect_opened
    }

    pub fn attempt(&self) -> &ConnectAttempt {
        &self.attempt
    }

    /// The dialog is waiting on a connect.
    pub fn connecting(&self) -> bool {
        self.attempt.session.is_some()
    }

    /// Connects from the dialog, which stays open until the host connects or fails.
    pub fn begin_attempt(
        &mut self,
        label: String,
        target: Target,
        server_id: Option<String>,
        save: Option<Server>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.cancel_attempt();
        let id = self.start(label, target, server_id, save, true, cx);
        self.attempt = ConnectAttempt {
            session: Some(id),
            progress: None,
            errors: Vec::new(),
        };
        id
    }

    /// Ends the connect the dialog waits on; the dialog stays open.
    pub fn cancel_attempt(&mut self) {
        if let Some(id) = self.attempt.session.take() {
            self.drop_session(id);
        }
        self.attempt.progress = None;
    }

    /// The person changed a field: the last failure no longer applies.
    pub fn clear_attempt_errors(&mut self) {
        self.attempt.errors.clear();
    }

    /// The dialog's connect failed: its session goes, and the dialog says why.
    fn fail_attempt(&mut self, id: u64, message: Option<String>) {
        self.attempt = ConnectAttempt {
            session: None,
            progress: None,
            errors: vec![message.unwrap_or_else(|| "Connection failed".into())],
        };
        self.drop_session(id);
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
                    .tabs()
                    .find(|session| {
                        session.server_id.as_deref() == Some(server.id.as_str())
                            || session.key() == server.key()
                    })
                    .map(|session| session.id),
            })
            .collect();
        for session in self.tabs() {
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
        self.push_recent(format!(
            "{}@{}:{}",
            target.username, target.host, target.port
        ));
        let id = self.start(label, target, server_id, save, false, cx);
        self.route = Route::Host(id, HostTab::Metrics);
        id
    }

    /// Starts a session to `target`; a `pending` one has no tab until it connects.
    fn start(
        &mut self,
        label: String,
        target: Target,
        server_id: Option<String>,
        save: Option<Server>,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let (
            handle,
            SessionEvents {
                session: mut events,
                shell: mut shell_events,
            },
        ) = session::start(
            target.clone(),
            self.store.clone(),
            self.settings.reconnect(),
        );
        self.sessions.push(HostSession::new(
            id,
            (label, target, server_id, save),
            pending,
            handle,
        ));
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this
                    .update(cx, |state, cx| {
                        state.apply(id, event, cx);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        // The shell's output goes to the terminal alone; the rest of the window stays.
        cx.spawn(async move |this, cx| {
            while let Some(event) = shell_events.next().await {
                if this
                    .update(cx, |state, cx| state.apply_shell(id, event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        id
    }

    /// Opens a saved host's session, or connects it with its secret from the Keychain. With
    /// no saved password, or no access to it, the connect dialog asks for it.
    pub fn open_saved(&mut self, server: &Server, cx: &mut Context<Self>) {
        if server.auth_mode == AuthMode::Agent
            || self
                .tabs()
                .any(|session| session.server_id.as_deref() == Some(server.id.as_str()))
        {
            self.connect_saved(server, None, cx);
            return;
        }
        // The Keychain may be showing its prompt for the last click.
        if !self.reading_secret.insert(server.id.clone()) {
            return;
        }
        let (secrets, id) = (self.secrets.clone(), server.id.clone());
        let lookup = cx.background_spawn(async move { secrets.get(&id) });
        let id = server.id.clone();
        cx.spawn(async move |this, cx| {
            let lookup = lookup.await;
            let _ = this.update(cx, |state, cx| {
                state.reading_secret.remove(&id);
                // The list has the host as it is now, if it was edited or removed meanwhile.
                let Some(server) = state.servers.iter().find(|server| server.id == id).cloned()
                else {
                    return;
                };
                match secrets::open_saved(server.auth_mode, lookup) {
                    OpenSaved::Connect(secret) => {
                        state.connect_saved(&server, secret, cx);
                    }
                    OpenSaved::Ask(reason) => {
                        if let Some(reason) = reason {
                            state.toast(reason, true);
                        }
                        state.open_connect(Some(server));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Saves, off the main thread, the secret a saved host signed in with, or forgets the
    /// one saved before when it signed in with none.
    fn remember_secret(
        &self,
        id: String,
        label: String,
        secret: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let secrets = self.secrets.clone();
        let saved =
            cx.background_spawn(async move { secrets.remember(&id, &label, secret.as_deref()) });
        cx.spawn(async move |this, cx| {
            if let Err(error) = saved.await {
                let _ = this.update(cx, |state, cx| {
                    state.toast(format!("Could not update the Keychain: {error}"), true);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Connects a saved host, with its password or passphrase in `secret`.
    pub fn connect_saved(
        &mut self,
        server: &Server,
        secret: Option<String>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let open = self
            .tabs()
            .find(|session| session.server_id.as_deref() == Some(server.id.as_str()))
            .map(|open| open.id);
        if let Some(id) = open {
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

    fn apply(&mut self, id: u64, event: SessionEvent, cx: &mut Context<Self>) {
        let mut saved = None;
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return;
        };
        // The dialog's connect: its progress and failure are the dialog's; once it connects,
        // it is a tab like any other, and takes the page.
        if session.pending
            && let SessionEvent::Status(status, message) = &event
        {
            match status {
                Status::Connected => {
                    session.pending = false;
                    let (key, server) = (session.key(), session.server_id.clone());
                    self.attempt = ConnectAttempt::default();
                    self.connect_open = false;
                    self.connect_server = None;
                    self.push_recent(key);
                    self.route = Route::Host(id, HostTab::Metrics);
                    // An edited saved host's new session takes the place of its old tab.
                    let replaced: Vec<u64> = self
                        .tabs()
                        .filter(|open| open.id != id && open.server_id.is_some())
                        .filter(|open| open.server_id == server)
                        .map(|open| open.id)
                        .collect();
                    for old in replaced {
                        self.drop_session(old);
                    }
                }
                Status::Error | Status::Disconnected => {
                    self.fail_attempt(id, message.clone());
                    return;
                }
                Status::Connecting | Status::Reconnecting => {
                    if message.is_some() {
                        self.attempt.progress = message.clone();
                    }
                }
            }
        }
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
            SessionEvent::Fallback(reason) => session.fallback = Some(reason),
            SessionEvent::Metrics(snapshot) => {
                session.history.record(&snapshot);
                session.snapshot = Some(*snapshot);
            }
            SessionEvent::KillResult { id, ok, error } => {
                let sent = session.kills.remove(&id);
                if ok {
                    if let Some((pid, signal)) = sent {
                        self.toast(
                            format!("Sent SIG{} to process {pid}.", signal_name(signal)),
                            false,
                        );
                    }
                } else {
                    let message = error.unwrap_or_else(|| "kill failed".into());
                    self.toast(format!("Could not stop the process: {message}"), true);
                }
            }
            SessionEvent::Ended => {}
        }
        if let Some(server) = saved {
            let label = format!("Vitals: {}", server.key());
            match self.store.upsert_server(server) {
                Ok(server_id) => {
                    // The secret it signed in with goes to the Keychain, never to the file.
                    let secret = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == id)
                        .map(|session| {
                            session.server_id = Some(server_id.clone());
                            session.target.secret.clone()
                        });
                    self.servers = self.store.servers();
                    if let Some(secret) = secret {
                        self.remember_secret(server_id, label, secret, cx);
                    }
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
        // The dialog's connect ends like a disconnect, which says the connection failed.
        if self.attempt.session == Some(id) {
            self.fail_attempt(id, None);
            return;
        }
        self.close_session(id);
    }

    /// Ends the session and drops its tab, with no question.
    pub fn close_session(&mut self, id: u64) {
        if matches!(self.confirm, Some(Confirm::Kill { session, .. } | Confirm::Close { session }) if session == id)
        {
            self.confirm = None;
        }
        if self.attempt.session == Some(id) {
            self.attempt.session = None;
            self.attempt.progress = None;
        }
        self.drop_session(id);
        if matches!(self.route, Route::Host(open, _) if open == id) {
            self.route = Route::Hosts;
        }
    }

    /// Ends the session and forgets it.
    fn drop_session(&mut self, id: u64) {
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            let session = self.sessions.remove(index);
            session.handle.disconnect();
        }
    }

    /// Forgets a saved host and its secret.
    pub fn remove_server(&mut self, id: &str, cx: &mut Context<Self>) {
        match self.store.remove_server(id) {
            Ok(()) => self.remember_secret(id.to_string(), String::new(), None, cx),
            Err(error) => self.toast(format!("Could not remove the host: {error}"), true),
        }
        self.servers = self.store.servers();
    }
}

/// Forgets, off the main thread, the secrets of hosts the data folder no longer lists.
fn forget_unlisted_secrets(secrets: &Secrets, store: &Store) {
    let (secrets, store) = (secrets.clone(), store.clone());
    std::thread::spawn(move || {
        if let Err(error) = secrets.forget_unlisted(&store) {
            eprintln!("vitals: could not forget the Keychain secrets of removed hosts: {error}");
        }
    });
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl AppState {
    /// A connect from the dialog with no network behind it, whose events a test feeds.
    pub(crate) fn begin_test_attempt(&mut self, label: &str, target: Target) -> u64 {
        self.cancel_attempt();
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.push(HostSession::new(
            id,
            (label.to_string(), target, None, None),
            true,
            SessionHandle::detached(),
        ));
        self.attempt = ConnectAttempt {
            session: Some(id),
            progress: None,
            errors: Vec::new(),
        };
        id
    }

    /// Feeds `event` to the session `id` as its runtime would.
    pub(crate) fn feed(&mut self, id: u64, event: SessionEvent, cx: &mut Context<Self>) {
        self.apply(id, event, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};
    use serde_json::json;

    use super::*;

    #[test]
    fn smoothing_follows_the_gap() {
        assert_eq!(smooth(None, 40.0, 1.0), 40.0);
        assert_eq!(smooth(Some(10.0), 40.0, 0.0), 40.0);
        assert_eq!(smooth(Some(10.0), 40.0, 16.0), 40.0);
        let one = smooth(Some(10.0), 40.0, 1.0);
        let three = smooth(Some(10.0), 40.0, 3.0);
        assert!(one > 10.0 && one < three && three < 40.0);
        assert!((three - (10.0 + 30.0 * (1.0 - (-1.0f64).exp()))).abs() < 1e-9);
    }

    fn ids(order: &[String]) -> Vec<&str> {
        order.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_card_layout_is_normalized_as_it_loads() {
        let layout = CardLayout::from_json(&json!({
            "hidden": ["disk", "nope", "disk"],
            "order": ["processes", "cpu", "processes", "nope"],
            "anchors": {
                "cpu": { "x": 1, "y": 0 },
                "memory": { "x": -1, "y": 0 },
                "nope": { "x": 0, "y": 0 },
                "disk": { "x": 2 },
            },
        }));
        assert_eq!(layout.hidden, ["disk"]);
        // The named cards first, once each, then every other card in `CARD_IDS` order.
        assert_eq!(
            ids(&layout.order),
            [
                "processes",
                "cpu",
                "memory",
                "network",
                "diskio",
                "pressure",
                "disk",
                "thermal"
            ]
        );
        assert_eq!(
            layout.anchors.into_iter().collect::<Vec<_>>(),
            [("cpu".to_string(), (1, 0))]
        );
        assert_eq!(CardLayout::from_json(&json!("broken")), CardLayout::reset());
    }

    #[test]
    fn a_card_layout_survives_its_file() {
        let mut layout = CardLayout::reset();
        layout.hidden = vec!["thermal".into()];
        layout.set_grid(&DashboardLayout {
            order: ["memory", "cpu"].map(SharedString::from).to_vec(),
            anchors: [(SharedString::from("cpu"), Cell { x: 2, y: 1 })].into(),
        });
        assert_eq!(&ids(&layout.order)[..3], ["memory", "cpu", "network"]);
        assert_eq!(layout.anchors.get("cpu"), Some(&(2, 1)));
        let dir = std::env::temp_dir().join(format!(
            "vitals-card-layout-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        write_card_layout(&dir, &layout).expect("the layout is written");
        assert_eq!(read_card_layout(&dir), layout);
        let grid = read_card_layout(&dir).grid();
        assert_eq!(grid.anchors.get("cpu"), Some(&Cell { x: 2, y: 1 }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_layout_packs_the_shown_cards_and_drops_the_anchors() {
        let mut layout = CardLayout::reset();
        layout.anchors.insert("cpu".into(), (2, 3));
        let shown = ["cpu", "memory", "diskio", "processes"];
        layout.auto_layout(&shown, 3);
        assert!(layout.anchors.is_empty());
        let packed: Vec<&str> = ids(&layout.order)[..4].to_vec();
        let mut sorted = packed.clone();
        sorted.sort();
        assert_eq!(sorted, ["cpu", "diskio", "memory", "processes"]);
        // On 3 columns the four cards fill two rows and a half, with no hole before the end.
        let sized: Vec<(&str, u16, u16)> = packed
            .iter()
            .map(|id| (*id, card_size(id).0, card_size(id).1))
            .collect();
        let rects = polterware_ui::patterns::pack_cards(&sized, 3, &HashMap::new());
        let rows = rects.values().map(|rect| rect.y + rect.h).max();
        assert_eq!(rows, Some(3));
        // The cards it was not given keep their order after.
        assert_eq!(
            &ids(&layout.order)[4..],
            ["network", "pressure", "disk", "thermal"]
        );
    }

    fn target() -> Target {
        Target {
            host: "10.0.0.5".into(),
            port: 2222,
            username: "root".into(),
            auth_mode: AuthMode::Agent,
            private_key_path: None,
            secret: None,
        }
    }

    fn status(status: Status, message: Option<&str>) -> SessionEvent {
        SessionEvent::Status(status, message.map(str::to_string))
    }

    #[gpui::test]
    fn the_dialog_s_connect_reports_in_the_dialog(cx: &mut TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.open_connect(None);
            let id = state.begin_test_attempt("box", target());
            assert!(state.connecting());
            assert_eq!(state.tabs().count(), 0, "no tab while it connects");
            state.feed(id, status(Status::Connecting, Some("Authenticating…")), cx);
            assert_eq!(state.attempt().progress.as_deref(), Some("Authenticating…"));
            // A message-less step keeps the last one.
            state.feed(id, status(Status::Connecting, None), cx);
            assert_eq!(state.attempt().progress.as_deref(), Some("Authenticating…"));
            state.feed(id, status(Status::Error, Some("Permission denied")), cx);
            assert!(!state.connecting());
            assert!(state.connect_open(), "the dialog stays open");
            assert_eq!(state.attempt().errors, ["Permission denied"]);
            assert_eq!(state.attempt().progress, None);
            assert!(state.session(id).is_none(), "the failed session goes");
            assert_eq!(state.route(), Route::Hosts);
            // With no message, it says the connection failed.
            let again = state.begin_test_attempt("box", target());
            assert!(
                state.attempt().errors.is_empty(),
                "a new attempt clears them"
            );
            state.feed(again, status(Status::Disconnected, None), cx);
            assert_eq!(state.attempt().errors, ["Connection failed"]);
            state.clear_attempt_errors();
            assert!(state.attempt().errors.is_empty());
        });
    }

    #[gpui::test]
    fn a_connected_attempt_becomes_a_tab_and_closes_the_dialog(cx: &mut TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.open_connect(None);
            let id = state.begin_test_attempt("box", target());
            state.feed(id, status(Status::Connected, None), cx);
            assert!(!state.connect_open());
            assert!(!state.connecting());
            assert_eq!(state.tabs().map(|tab| tab.id).collect::<Vec<_>>(), [id]);
            assert_eq!(state.route(), Route::Host(id, HostTab::Metrics));
            assert_eq!(
                state.recents().first().map(String::as_str),
                Some("root@10.0.0.5:2222")
            );
            // Later drops are the tab's, not the dialog's.
            state.feed(id, status(Status::Reconnecting, Some("Retrying")), cx);
            assert!(state.attempt().progress.is_none());
            assert_eq!(
                state.session(id).map(|s| s.status),
                Some(Status::Reconnecting)
            );
        });
    }

    #[gpui::test]
    fn cancel_and_a_rejected_key_end_the_attempt(cx: &mut TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.open_connect(None);
            let id = state.begin_test_attempt("box", target());
            state.feed(
                id,
                status(Status::Connecting, Some("Connecting to 10.0.0.5")),
                cx,
            );
            state.cancel_attempt();
            assert!(state.connect_open(), "Cancel keeps the dialog");
            assert!(!state.connecting());
            assert!(state.attempt().progress.is_none());
            assert!(state.session(id).is_none());
            let id = state.begin_test_attempt("box", target());
            state.feed(id, SessionEvent::HostKey("SHA256:abc".into()), cx);
            assert_eq!(state.host_key_prompt().map(|s| s.id), Some(id));
            state.reject_host_key(id);
            assert_eq!(state.attempt().errors, ["Connection failed"]);
            assert!(state.connect_open());
            assert!(state.host_key_prompt().is_none());
            // Closing the dialog ends a connect it waits on.
            let id = state.begin_test_attempt("box", target());
            state.close_connect();
            assert!(!state.connect_open());
            assert!(state.session(id).is_none());
            assert!(state.attempt().errors.is_empty());
        });
    }
}
