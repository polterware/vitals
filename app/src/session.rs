//! A host's live session, run on the background runtime: connect, approve the host key,
//! install the agent, read its frames, and restart the stream or the connection when either
//! drops, as the Electron app's `AgentSession` and `SessionManager` do.
//!
//! The view talks to it through [`SessionHandle`] and hears back through [`SessionEvent`]s.

use std::sync::OnceLock;
use std::time::Duration;

use futures::channel::mpsc as events;
use tokio::sync::mpsc;

use crate::installer;
use crate::protocol::{self, Command, Frame, Hello, LineBuffer, Signal, Snapshot};
use crate::ssh::{ConnectError, Connection, Target};
use crate::store::Store;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
const FAILURES_BEFORE_ERROR: u32 = 3;

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
}

pub fn start(
    target: Target,
    store: Store,
    reconnect: Reconnect,
) -> (SessionHandle, events::UnboundedReceiver<SessionEvent>) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = events::unbounded();
    runtime().spawn(async move {
        let mut session = Session {
            target,
            store,
            reconnect,
            commands: command_rx,
            events: event_tx,
        };
        session.run().await;
        session.emit(SessionEvent::Ended);
    });
    (SessionHandle { commands }, event_rx)
}

struct Session {
    target: Target,
    store: Store,
    reconnect: Reconnect,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    events: events::UnboundedSender<SessionEvent>,
}

enum StreamEnd {
    Disconnect,
    /// The agent never sent its hello.
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

    async fn run(&mut self) {
        let mut approved: Option<String> = None;
        let mut ever_connected = false;
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
                Ok(connection) => connection,
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

            self.status(Status::Connecting, Some("Installing metrics agent…"));
            let installed = tokio::select! {
                installed = installer::install(&connection) => installed,
                _ = self.until_disconnect() => {
                    connection.close().await;
                    self.status(Status::Disconnected, None);
                    return;
                }
            };
            let command = match installed {
                Ok(installed) => installed.command,
                Err(error) => {
                    // The Electron app falls back to a shell script here; that path is not
                    // ported yet.
                    let message = format!("Agent unavailable: {error:#}");
                    self.status(Status::Error, Some(&message));
                    self.until_disconnect().await;
                    connection.close().await;
                    self.status(Status::Disconnected, None);
                    return;
                }
            };

            match self
                .stream_agent(&connection, &command, &mut ever_connected)
                .await
            {
                StreamEnd::Disconnect => {
                    connection.close().await;
                    self.status(Status::Disconnected, None);
                    return;
                }
                StreamEnd::FailedStart(message) => {
                    self.status(
                        Status::Error,
                        Some(&format!("Agent failed to start: {message}")),
                    );
                    self.until_disconnect().await;
                    connection.close().await;
                    self.status(Status::Disconnected, None);
                    return;
                }
                StreamEnd::Lost(message) => {
                    connection.close().await;
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
