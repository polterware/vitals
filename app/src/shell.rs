//! The Terminal tab's shell: an interactive channel with a PTY on the session's own SSH
//! connection (the Electron app's `ShellSession` kept a second client for it). It opens once
//! the view asks and a signed-in connection is there, follows the view's size, and after a
//! lost connection opens again on the next one the session makes, saying so. Keystrokes typed
//! while it is down are dropped, as the Electron app's were; a PTY merges stderr into the
//! output.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::SinkExt;
use futures::channel::mpsc as events;
use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::sync::{mpsc, watch};

use crate::ssh::Connection;

/// The terminal type the shell runs under, as the Electron app asked for.
pub const TERM: &str = "xterm-256color";
/// How long a closed channel waits to tell a shell that ended from a connection that dropped.
const EXIT_GRACE: Duration = Duration::from_millis(50);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// Output chunks on their way to the view. Past them the channel stops reading, and the SSH
/// window holds the host back until the view catches up.
pub const IN_FLIGHT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Open { cols: u16, rows: u16 },
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    Output(Vec<u8>),
    /// The shell opened again on a new connection.
    Reconnected,
    /// The shell ended on the host (`exit`).
    Exited,
    /// The host would not open the shell.
    Failed(String),
}

/// The session's signed-in connection, while it has one.
pub type Connections = watch::Receiver<Option<Arc<Connection>>>;

enum End {
    /// The view closed the shell.
    Closed,
    /// The session moved off this connection.
    Lost,
    /// The channel ended; `exited` when the host said the shell finished.
    Ended { exited: bool },
}

/// Runs the view's shell until the session ends.
pub async fn run(
    mut commands: mpsc::UnboundedReceiver<ShellCommand>,
    mut connections: Connections,
    mut events: events::Sender<ShellEvent>,
) {
    let mut wanted: Option<(u16, u16)> = None;
    let mut was_open = false;
    loop {
        let connection = loop {
            if wanted.is_some()
                && let Some(connection) = connections
                    .borrow_and_update()
                    .clone()
                    .filter(|connection| !connection.is_closed())
            {
                break connection;
            }
            tokio::select! {
                command = commands.recv() => match command {
                    None => return,
                    Some(ShellCommand::Open { cols, rows }) => wanted = Some((cols, rows)),
                    Some(ShellCommand::Resize { cols, rows }) => {
                        if wanted.is_some() {
                            wanted = Some((cols, rows));
                        }
                    }
                    Some(ShellCommand::Close) => {
                        wanted = None;
                        was_open = false;
                    }
                    Some(ShellCommand::Input(_)) => {}
                },
                changed = connections.changed() => if changed.is_err() {
                    return;
                },
            }
        };
        let (cols, rows) = wanted.unwrap_or((80, 24));
        let (channel, early) = match open(&connection, cols, rows).await {
            Ok(opened) => opened,
            // The connection went with it: open again on the next one.
            Err(_) if connection.is_closed() => continue,
            Err(error) => {
                wanted = None;
                was_open = false;
                if events
                    .send(ShellEvent::Failed(error.to_string()))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        if was_open && events.send(ShellEvent::Reconnected).await.is_err() {
            return;
        }
        was_open = true;
        if !early.is_empty() && events.send(ShellEvent::Output(early)).await.is_err() {
            return;
        }
        let (mut reader, writer) = channel.split();
        let mut exited = false;
        let end = loop {
            tokio::select! {
                message = reader.wait() => match message {
                    Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                        if events.send(ShellEvent::Output(data.to_vec())).await.is_err() {
                            let _ = writer.close().await;
                            return;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { .. } | ChannelMsg::ExitSignal { .. } | ChannelMsg::Eof) => {
                        exited = true;
                    }
                    Some(ChannelMsg::Close) | None => break End::Ended { exited },
                    Some(_) => {}
                },
                command = commands.recv() => match command {
                    None => {
                        let _ = writer.close().await;
                        return;
                    }
                    Some(ShellCommand::Input(bytes)) => {
                        let _ = writer.data_bytes(bytes).await;
                    }
                    Some(ShellCommand::Open { cols, rows } | ShellCommand::Resize { cols, rows }) => {
                        wanted = Some((cols, rows));
                        let _ = writer.window_change(cols.into(), rows.into(), 0, 0).await;
                    }
                    Some(ShellCommand::Close) => {
                        let _ = writer.close().await;
                        wanted = None;
                        was_open = false;
                        break End::Closed;
                    }
                },
                changed = connections.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let same = connections
                        .borrow_and_update()
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &connection));
                    if !same {
                        break End::Lost;
                    }
                }
            }
        };
        match end {
            End::Closed | End::Lost => {}
            End::Ended { exited } => {
                tokio::time::sleep(EXIT_GRACE).await;
                // No word from the shell and the connection gone: it dropped under the shell.
                if !exited && connection.is_closed() {
                    continue;
                }
                wanted = None;
                was_open = false;
                if events.send(ShellEvent::Exited).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Opens a session channel with a PTY of `cols` × `rows` and a shell in it, waiting for the
/// host to accept both. Output that comes before the second answer is returned with it.
async fn open(connection: &Connection, cols: u16, rows: u16) -> Result<(Channel<Msg>, Vec<u8>)> {
    let mut channel = connection.open_channel().await?;
    channel
        .request_pty(true, TERM, cols.into(), rows.into(), 0, 0, &[])
        .await?;
    channel.request_shell(true).await?;
    let mut early = Vec::new();
    let answers = async {
        let mut accepted = 0;
        while accepted < 2 {
            match channel.wait().await {
                Some(ChannelMsg::Success) => accepted += 1,
                Some(ChannelMsg::Failure) if accepted == 0 => {
                    return Err(anyhow!("The host refused a terminal"));
                }
                Some(ChannelMsg::Failure) => return Err(anyhow!("The host refused a shell")),
                Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                    early.extend_from_slice(&data);
                }
                Some(ChannelMsg::Close) | None => {
                    return Err(anyhow!("The shell channel closed"));
                }
                Some(_) => {}
            }
        }
        Ok(())
    };
    tokio::time::timeout(OPEN_TIMEOUT, answers)
        .await
        .map_err(|_| anyhow!("Timed out opening the shell"))??;
    Ok((channel, early))
}
