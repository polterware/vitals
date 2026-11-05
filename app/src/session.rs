//! A host's live session, run on the background runtime: connect, approve the host key,
//! install the agent, read its frames, and restart the stream or the connection when either
//! drops, as the Electron app's `AgentSession` and `SessionManager` do. Where the agent cannot
//! be installed or never starts, the session falls back to the remote script
//! ([`crate::script`]) for good, as the Electron app's `SshSession` does: its samples become
//! the same snapshots.
//!
//! The view talks to it through [`SessionHandle`] and hears back through [`SessionEvent`]s.
//! The Terminal tab's shell ([`crate::shell`]) runs beside it on the same connection, which
//! the session hands over each time it signs in.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::channel::mpsc as events;
use tokio::sync::{mpsc, watch};

use crate::installer;
use crate::parsers::{self, DeltaState, FrameBuffer};
use crate::protocol::{self, Command, Frame, Hello, LineBuffer, Signal, Snapshot};
use crate::script;
use crate::shell::{self, ShellCommand, ShellEvent};
use crate::ssh::{ConnectError, Connection, Target};
use crate::store::Store;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
const FAILURES_BEFORE_ERROR: u32 = 3;
/// How long the script fallback waits before it starts a stopped script again.
const SCRIPT_RESTART: Duration = Duration::from_secs(2);

/// The runtime SSH runs on, apart from GPUI's executor.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("vitals-ssh")
            .enable_all()
            .build()
            .expect("the SSH runtime starts")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Connecting => "Connecting",
            Status::Connected => "Connected",
            Status::Reconnecting => "Reconnecting",
            Status::Disconnected => "Disconnected",
            Status::Error => "Error",
        }
    }

    /// The word the shared status colors know.
    pub fn tone_key(self) -> &'static str {
        match self {
            Status::Connecting | Status::Reconnecting => "running",
            Status::Connected => "live",
            Status::Disconnected => "stopped",
            Status::Error => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Status(Status, Option<String>),
    HostKey(String),
    Hello(Hello),
    /// The agent could not be installed or run, so the metrics come from the remote script
    /// from now on: why.
    Fallback(String),
    Metrics(Box<Snapshot>),
    KillResult {
        id: String,
        ok: bool,
        error: Option<String>,
    },
    /// The session ended for good.
    Ended,
}

#[derive(Debug)]
enum SessionCommand {
    ApproveHostKey,
    RejectHostKey,
    Kill {
        pid: u32,
        signal: Signal,
        id: String,
    },
    Disconnect,
}

#[derive(Clone, Copy, Debug)]
pub struct Reconnect {
    pub enabled: bool,
    pub delay: Duration,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self {
            enabled: true,
            delay: Duration::from_secs(3),
        }
    }
}

/// Sends to a running session; dropping every handle ends it.
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::UnboundedSender<SessionCommand>,
    shell: mpsc::UnboundedSender<ShellCommand>,
}

impl SessionHandle {
    pub fn approve_host_key(&self) {
        let _ = self.commands.send(SessionCommand::ApproveHostKey);
    }

    pub fn reject_host_key(&self) {
        let _ = self.commands.send(SessionCommand::RejectHostKey);
    }

    pub fn kill(&self, pid: u32, signal: Signal, id: String) {
        let _ = self.commands.send(SessionCommand::Kill { pid, signal, id });
    }

    pub fn disconnect(&self) {
        let _ = self.commands.send(SessionCommand::Disconnect);
    }

    /// Opens the shell at `cols` × `rows` once the session is signed in.
    pub fn open_shell(&self, cols: u16, rows: u16) {
        let _ = self.shell.send(ShellCommand::Open { cols, rows });
    }

    pub fn shell_input(&self, bytes: Vec<u8>) {
        let _ = self.shell.send(ShellCommand::Input(bytes));
    }

    pub fn resize_shell(&self, cols: u16, rows: u16) {
        let _ = self.shell.send(ShellCommand::Resize { cols, rows });
    }

    pub fn close_shell(&self) {
        let _ = self.shell.send(ShellCommand::Close);
    }

    /// A handle whose commands go nowhere, for tests that feed a session's events by hand.
    #[cfg(test)]
    pub fn detached() -> Self {
        let (commands, _) = mpsc::unbounded_channel();
        let (shell, _) = mpsc::unbounded_channel();
        Self { commands, shell }
    }
}

/// What a started session reports: its own events, and its shell's.
pub struct SessionEvents {
    pub session: events::UnboundedReceiver<SessionEvent>,
    pub shell: events::Receiver<ShellEvent>,
}

pub fn start(target: Target, store: Store, reconnect: Reconnect) -> (SessionHandle, SessionEvents) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = events::unbounded();
    let (shell, shell_rx) = mpsc::unbounded_channel();
    let (shell_tx, shell_events) = events::channel(shell::IN_FLIGHT);
    let (published, connections) = watch::channel(None);
    runtime().spawn(shell::run(shell_rx, connections, shell_tx));
    runtime().spawn(async move {
        let mut session = Session {
            target,
            store,
            reconnect,
            commands: command_rx,
            events: event_tx,
            published,
        };
        session.run().await;
        session.emit(SessionEvent::Ended);
    });
    (
        SessionHandle { commands, shell },
        SessionEvents {
            session: event_rx,
            shell: shell_events,
        },
    )
}

struct Session {
    target: Target,
    store: Store,
    reconnect: Reconnect,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    events: events::UnboundedSender<SessionEvent>,
    /// The signed-in connection, for the shell.
    published: watch::Sender<Option<Arc<Connection>>>,
}

enum StreamEnd {
    Disconnect,
    /// The metrics never started: the agent sent no hello, or the host cannot run the script.
    FailedStart(String),
    /// The connection dropped.
    Lost(String),
}

impl Session {
    fn emit(&self, event: SessionEvent) {
        let _ = self.events.unbounded_send(event);
    }

    fn status(&self, status: Status, message: Option<&str>) {
        self.emit(SessionEvent::Status(status, message.map(str::to_string)));
    }

    /// Takes the connection back from the shell and closes it.
    async fn close(&self, connection: &Connection) {
        self.published.send_replace(None);
        connection.close().await;
    }

    async fn run(&mut self) {
        let mut approved: Option<String> = None;
        let mut ever_connected = false;
        // Why the agent is unavailable, once the session fell back to the script.
        let mut fallback: Option<String> = None;
        loop {
            self.status(
                if ever_connected {
                    Status::Reconnecting
                } else {
                    Status::Connecting
                },
                None,
            );
            let (target, store) = (self.target.clone(), self.store.clone());
            let opened = tokio::select! {
                opened = Connection::open(&target, store, approved.clone()) => opened,
                _ = self.until_disconnect() => {
                    self.status(Status::Disconnected, None);
                    return;
                }
            };
            let connection = match opened {
                Ok(connection) => Arc::new(connection),
                Err(ConnectError::NewHostKey(print)) => {
                    self.emit(SessionEvent::HostKey(print.clone()));
                    self.status(Status::Connecting, Some("Waiting for host key approval"));
                    if self.await_approval().await {
                        if let Err(error) =
                            self.store
                                .approve_host_key(&self.target.host, self.target.port, &print)
                        {
                            self.status(
                                Status::Error,
                                Some(&format!("Could not save the host key: {error}")),
                            );
                            self.until_disconnect().await;
                            return;
                        }
                        approved = Some(print);
                        continue;
                    }
                    self.status(Status::Disconnected, None);
                    return;
                }
                Err(ConnectError::Other(message)) if ever_connected => {
                    if self.wait_to_reconnect(&message).await {
                        continue;
                    }
                    return;
                }
                // A session that never signed in fails for good instead of retrying.
                Err(error) => {
                    self.status(Status::Error, Some(&error.to_string()));
                    self.until_disconnect().await;
                    self.status(Status::Disconnected, None);
                    return;
                }
            };

            self.published.send_replace(Some(connection.clone()));
            let end = if fallback.is_some() {
                // A session that fell back stays on the script, across reconnects too.
                self.stream_script(&connection, &mut ever_connected).await
            } else {
                match self.run_agent(&connection, &mut ever_connected).await {
                    Ok(end) => end,
                    Err(reason) => {
                        self.emit(SessionEvent::Fallback(reason.clone()));
                        self.status(
                            Status::Connecting,
                            Some(&format!(
                                "Agent unavailable; using script fallback ({reason})"
                            )),
                        );
                        fallback = Some(reason);
                        self.stream_script(&connection, &mut ever_connected).await
                    }
                }
            };

            match end {
                StreamEnd::Disconnect => {
                    self.close(&connection).await;
                    self.status(Status::Disconnected, None);
                    return;
                }
                StreamEnd::FailedStart(message) => {
                    self.status(Status::Error, Some(&message));
                    self.until_disconnect().await;
                    self.close(&connection).await;
                    self.status(Status::Disconnected, None);
                    return;
                }
                StreamEnd::Lost(message) => {
                    self.close(&connection).await;
                    if ever_connected && self.wait_to_reconnect(&message).await {
                        continue;
                    }
                    if !ever_connected {
                        self.status(Status::Error, Some(&message));
                        self.until_disconnect().await;
                    }
                    self.status(Status::Disconnected, None);
                    return;
                }
            }
        }
    }

    /// Installs the agent and reads its stream. `Err` is why the agent is unavailable on this
    /// host (it could not be installed, or it sent no hello), for the script fallback.
    async fn run_agent(
        &mut self,
        connection: &Connection,
        ever_connected: &mut bool,
    ) -> Result<StreamEnd, String> {
        self.status(Status::Connecting, Some("Installing metrics agent…"));
        let installed = tokio::select! {
            installed = installer::install(connection) => installed,
            _ = self.until_disconnect() => return Ok(StreamEnd::Disconnect),
        };
        let command = match installed {
            Ok(installed) => installed.command,
            Err(error) if connection.is_closed() => {
                return Ok(StreamEnd::Lost(format!("{error:#}")));
            }
            Err(error) => return Err(format!("{error:#}")),
        };
        match self
            .stream_agent(connection, &command, ever_connected)
            .await
        {
            StreamEnd::FailedStart(message) => Err(message),
            end => Ok(end),
        }
    }

    /// Reads the agent's stream, restarting it with backoff while the connection holds.
    async fn stream_agent(
        &mut self,
        connection: &Connection,
        command: &str,
        ever_connected: &mut bool,
    ) -> StreamEnd {
        let mut failures: u32 = 0;
        let mut ready = false;
        loop {
            let channel = match connection.open_channel().await {
                Ok(channel) => channel,
                Err(error) if connection.is_closed() => return StreamEnd::Lost(error.to_string()),
                Err(error) if !ready => return StreamEnd::FailedStart(error.to_string()),
                Err(error) => {
                    failures += 1;
                    if let Some(end) = self.backoff(failures, &error.to_string()).await {
                        return end;
                    }
                    continue;
                }
            };
            if let Err(error) = channel.exec(true, command).await
                && !ready
            {
                return StreamEnd::FailedStart(error.to_string());
            }
            let (mut reader, writer) = channel.split();
            let mut lines = LineBuffer::default();
            let mut snapshot = Snapshot::default();
            let mut awaiting_hello = true;
            let failure = loop {
                tokio::select! {
                    message = reader.wait() => {
                        let Some(message) = message else {
                            break "Agent stream closed".to_string();
                        };
                        let russh::ChannelMsg::Data { data } = message else {
                            continue;
                        };
                        for line in lines.push(&data) {
                            let frame = protocol::parse_frame(&line);
                            if awaiting_hello {
                                let hello = match &frame {
                                    Ok(frame) => protocol::check_hello(frame).cloned(),
                                    Err(_) => Err("Invalid agent hello".to_string()),
                                };
                                match hello {
                                    Ok(hello) => {
                                        awaiting_hello = false;
                                        ready = true;
                                        *ever_connected = true;
                                        failures = 0;
                                        self.emit(SessionEvent::Hello(hello));
                                        self.status(Status::Connected, None);
                                    }
                                    Err(message) => return StreamEnd::FailedStart(message),
                                }
                                continue;
                            }
                            match frame {
                                Ok(Frame::Metrics(metrics)) => {
                                    failures = 0;
                                    snapshot = snapshot.next(*metrics);
                                    self.emit(SessionEvent::Metrics(Box::new(snapshot.clone())));
                                }
                                Ok(Frame::KillResult { id, ok, error }) => {
                                    self.emit(SessionEvent::KillResult { id, ok, error });
                                }
                                // Bad lines after the hello are skipped; the stream stays.
                                _ => {}
                            }
                        }
                    }
                    command = self.commands.recv() => match command {
                        None | Some(SessionCommand::Disconnect) => {
                            let _ = writer.close().await;
                            return StreamEnd::Disconnect;
                        }
                        Some(SessionCommand::Kill { pid, signal, id }) => {
                            let line = Command::Kill { pid, signal, id: id.clone() }.line();
                            if awaiting_hello || writer.data_bytes(line.into_bytes()).await.is_err() {
                                self.emit(SessionEvent::KillResult {
                                    id,
                                    ok: false,
                                    error: Some("Agent is not ready".into()),
                                });
                            }
                        }
                        Some(_) => {}
                    }
                }
            };
            if connection.is_closed() {
                return StreamEnd::Lost("Connection closed".into());
            }
            if !ready {
                return StreamEnd::FailedStart(failure);
            }
            failures += 1;
            if let Some(end) = self.backoff(failures, &failure).await {
                return end;
            }
        }
    }

    /// The script fallback: one exec runs [`script::SCRIPT`], which prints a sample a second,
    /// started again [`SCRIPT_RESTART`] after it stops while the connection holds. The session
    /// reports connected on the first sample, and again on the first one after an error.
    async fn stream_script(
        &mut self,
        connection: &Arc<Connection>,
        ever_connected: &mut bool,
    ) -> StreamEnd {
        let command = script::command();
        let mut delta: Option<DeltaState> = None;
        let mut snapshot = Snapshot::default();
        loop {
            let started = match connection.open_channel().await {
                Ok(channel) => match channel.exec(true, command.as_str()).await {
                    Ok(()) => Ok(channel),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error.to_string()),
            };
            let failure = match started {
                Err(message) => message,
                Ok(channel) => {
                    let (mut reader, writer) = channel.split();
                    let mut frames = FrameBuffer::default();
                    let mut live = false;
                    loop {
                        tokio::select! {
                            message = reader.wait() => {
                                let Some(message) = message else {
                                    break "Metrics script stopped".to_string();
                                };
                                let russh::ChannelMsg::Data { data } = message else {
                                    continue;
                                };
                                for frame in frames.push(&data) {
                                    let sample =
                                        parsers::build_metrics(&frame, delta.as_ref(), unix_ms());
                                    if !sample.supported {
                                        let _ = writer.close().await;
                                        return StreamEnd::FailedStart(
                                            "Script fallback needs a Linux host with /proc".into(),
                                        );
                                    }
                                    delta = Some(sample.state);
                                    snapshot = snapshot.next(sample.metrics);
                                    if !live {
                                        live = true;
                                        *ever_connected = true;
                                        self.status(Status::Connected, None);
                                    }
                                    self.emit(SessionEvent::Metrics(Box::new(snapshot.clone())));
                                }
                            }
                            command = self.commands.recv() => {
                                if let Some(end) = self.script_command(connection, command) {
                                    let _ = writer.close().await;
                                    return end;
                                }
                            }
                        }
                    }
                }
            };
            if connection.is_closed() {
                return StreamEnd::Lost("Connection closed".into());
            }
            self.status(Status::Error, Some(&failure));
            let restart = tokio::time::sleep(SCRIPT_RESTART);
            tokio::pin!(restart);
            loop {
                tokio::select! {
                    _ = &mut restart => break,
                    command = self.commands.recv() => {
                        if let Some(end) = self.script_command(connection, command) {
                            return end;
                        }
                    }
                }
            }
        }
    }

    /// A command while the script fallback runs; `Some` ends the stream. A kill runs as its
    /// own `kill` exec (the Electron app's `SshSession.killProcess`) and answers as the
    /// agent's `kill_result` does.
    fn script_command(
        &self,
        connection: &Arc<Connection>,
        command: Option<SessionCommand>,
    ) -> Option<StreamEnd> {
        let (pid, signal, id) = match command {
            None | Some(SessionCommand::Disconnect) => return Some(StreamEnd::Disconnect),
            Some(SessionCommand::Kill { pid, signal, id }) => (pid, signal, id),
            Some(_) => return None,
        };
        let (connection, events) = (connection.clone(), self.events.clone());
        runtime().spawn(async move {
            let error = match script::kill_command(pid, signal) {
                None => Some("Invalid pid".to_string()),
                Some(command) => match connection.exec_capture(&command).await {
                    Ok(done) if done.code == 0 => None,
                    Ok(done) => Some(format!("kill exited with {}", done.code)),
                    Err(error) => Some(error.to_string()),
                },
            };
            let _ = events.unbounded_send(SessionEvent::KillResult {
                id,
                ok: error.is_none(),
                error,
            });
        });
        None
    }

    /// Waits before the next stream, reporting an error after a few failures in a row.
    async fn backoff(&mut self, failures: u32, message: &str) -> Option<StreamEnd> {
        if failures >= FAILURES_BEFORE_ERROR {
            self.status(Status::Error, Some(message));
        }
        let delay =
            (INITIAL_BACKOFF * 2u32.pow(failures.saturating_sub(1).min(4))).min(MAX_BACKOFF);
        tokio::select! {
            _ = tokio::time::sleep(delay) => None,
            _ = self.until_disconnect() => Some(StreamEnd::Disconnect),
        }
    }

    /// After a lost connection: waits the reconnect delay, or ends when reconnecting is off
    /// or the view disconnects. True to connect again.
    async fn wait_to_reconnect(&mut self, message: &str) -> bool {
        if !self.reconnect.enabled {
            self.status(Status::Disconnected, Some(message));
            return false;
        }
        let seconds = self.reconnect.delay.as_secs();
        self.status(
            Status::Reconnecting,
            Some(&format!("Reconnecting in {seconds}s…")),
        );
        tokio::select! {
            _ = tokio::time::sleep(self.reconnect.delay) => true,
            _ = self.until_disconnect() => {
                self.status(Status::Disconnected, None);
                false
            }
        }
    }

    /// True when the host key is approved, false when it is rejected or the session ends.
    async fn await_approval(&mut self) -> bool {
        loop {
            match self.commands.recv().await {
                Some(SessionCommand::ApproveHostKey) => return true,
                Some(SessionCommand::RejectHostKey | SessionCommand::Disconnect) | None => {
                    return false;
                }
                Some(SessionCommand::Kill { id, .. }) => self.emit(SessionEvent::KillResult {
                    id,
                    ok: false,
                    error: Some("Agent is not ready".into()),
                }),
            }
        }
    }

    /// Returns once the view disconnects or drops the session.
    async fn until_disconnect(&mut self) {
        loop {
            match self.commands.recv().await {
                Some(SessionCommand::Disconnect | SessionCommand::RejectHostKey) | None => return,
                Some(SessionCommand::Kill { id, .. }) => self.emit(SessionEvent::KillResult {
                    id,
                    ok: false,
                    error: Some("Agent is not ready".into()),
                }),
                Some(SessionCommand::ApproveHostKey) => {}
            }
        }
    }
}

/// Now, in Unix milliseconds: when a script sample arrived.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}
