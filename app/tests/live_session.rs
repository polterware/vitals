//! A session against a real SSH server, run only when `VITALS_TEST_HOST` names one:
//! `VITALS_TEST_HOST=127.0.0.1 VITALS_TEST_PORT=22022 VITALS_TEST_USER=me
//! VITALS_TEST_KEY=/path/to/key cargo test -p vitals-app --test live_session -- --nocapture`
//! after `agent/build.sh`. It approves the host key in a temporary store, installs the
//! agent, and waits for the hello and two metrics frames; then it connects again with the
//! saved key and drives the Terminal tab's shell on that connection: its size, a command,
//! a resize, and `exit`.
//!
//! With `VITALS_TEST_FALLBACK=1` as well, and only against a local docker sshd (a loopback
//! host), it forces the script fallback: see [`script_fallback_streams_metrics`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use vitals_app::protocol::{Signal, Snapshot};
use vitals_app::session::{self, Reconnect, SessionEvent, SessionEvents, Status};
use vitals_app::shell::ShellEvent;
use vitals_app::ssh::{ConnectError, Connection, Target};
use vitals_app::store::{AuthMode, Store};

/// One test at a time on the host: the fallback test takes its agent folder away.
static HOST: Mutex<()> = Mutex::new(());

/// The server the `VITALS_TEST_*` variables name, with a store of its own in a temporary
/// folder.
fn live(name: &str) -> Option<(Target, Store, PathBuf)> {
    let Ok(host) = std::env::var("VITALS_TEST_HOST") else {
        eprintln!("VITALS_TEST_HOST is not set; skipped");
        return None;
    };
    let port = std::env::var("VITALS_TEST_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(22);
    let username = std::env::var("VITALS_TEST_USER").expect("VITALS_TEST_USER");
    let key = std::env::var("VITALS_TEST_KEY").ok();
    let dir = std::env::temp_dir().join(format!("vitals-live-{name}-{}", std::process::id()));
    let store = Store::new(&dir);
    let target = Target {
        host,
        port,
        username,
        auth_mode: if key.is_some() {
            AuthMode::PrivateKey
        } else {
            AuthMode::Agent
        },
        private_key_path: key,
        secret: None,
    };
    Some((target, store, dir))
}

#[test]
fn agent_streams_metrics() {
    let _host = HOST.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((target, store, dir)) = live("agent") else {
        return;
    };
    let (handle, events) = session::start(target.clone(), store.clone(), Reconnect::default());
    let mut events = events.session;
    let run = async {
        let mut metrics = 0;
        let mut hello = None;
        while let Some(event) = events.next().await {
            eprintln!("{event:?}");
            match event {
                SessionEvent::HostKey(_) => handle.approve_host_key(),
                SessionEvent::Hello(frame) => hello = Some(frame),
                SessionEvent::Metrics(_) => {
                    metrics += 1;
                    if metrics == 2 {
                        handle.disconnect();
                    }
                }
                SessionEvent::Status(Status::Error, message) => {
                    panic!("session error: {message:?}")
                }
                SessionEvent::Ended => break,
                _ => {}
            }
        }
        (hello, metrics)
    };
    let (hello, metrics) = session::runtime()
        .block_on(async { tokio::time::timeout(Duration::from_secs(60), run).await })
        .expect("the session finishes in a minute");
    assert!(hello.is_some(), "no hello");
    assert_eq!(metrics, 2);
    // The approved key is kept, so the next session connects without asking.
    let (
        handle,
        SessionEvents {
            session: mut events,
            mut shell,
        },
    ) = session::start(target, store, Reconnect::default());
    let second = session::runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            while let Some(event) = events.next().await {
                match event {
                    SessionEvent::HostKey(_) => return Err("asked for the host key again"),
                    SessionEvent::Status(Status::Connected, _) => break,
                    SessionEvent::Status(Status::Error, message) => {
                        panic!("session error: {message:?}")
                    }
                    _ => {}
                }
            }
            let mut output = String::new();
            handle.open_shell(100, 30);
            handle.shell_input(b"stty size; echo vitals-$((6*7))\r".to_vec());
            read_until(&mut shell, &mut output, "30 100").await?;
            read_until(&mut shell, &mut output, "vitals-42").await?;
            handle.resize_shell(120, 40);
            handle.shell_input(b"stty size\r".to_vec());
            read_until(&mut shell, &mut output, "40 120").await?;
            handle.shell_input(b"exit\r".to_vec());
            loop {
                match shell.next().await {
                    Some(ShellEvent::Exited) => break,
                    Some(ShellEvent::Output(_)) => {}
                    other => panic!("the shell did not exit cleanly: {other:?}"),
                }
            }
            handle.disconnect();
            Ok(())
        })
        .await
    });
    assert_eq!(second, Ok(Ok(())));
    let _ = std::fs::remove_dir_all(dir);
}

/// Reads the shell's output until it holds `needle`.
async fn read_until(
    shell: &mut mpsc::Receiver<ShellEvent>,
    output: &mut String,
    needle: &str,
) -> Result<(), &'static str> {
    while !output.contains(needle) {
        match shell.next().await {
            Some(ShellEvent::Output(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
            Some(ShellEvent::Failed(message)) => panic!("the shell failed: {message}"),
            Some(_) => {}
            None => return Err("the shell ended early"),
        }
    }
    eprintln!("shell printed {needle:?}");
    Ok(())
}

/// Opens a connection of the test's own, approving the host's key in the test's store the
/// first time, as the person does in the app.
async fn open(target: &Target, store: &Store) -> Connection {
    match Connection::open(target, store.clone(), None).await {
        Ok(connection) => connection,
        Err(ConnectError::NewHostKey(print)) => {
            store
                .approve_host_key(&target.host, target.port, &print)
                .expect("the host key is saved");
            Connection::open(target, store.clone(), None)
                .await
                .expect("connects with the approved key")
        }
        Err(error) => panic!("could not connect: {error}"),
    }
}

/// What a fallback session reported.
#[derive(Default)]
struct Fallback {
    reason: Option<String>,
    notice: Option<String>,
    connected: bool,
    snapshots: Vec<Snapshot>,
    kills: HashMap<String, (bool, Option<String>)>,
}

/// The script fallback, forced on a local docker sshd: the host's `~/.cache/vitals` becomes a
/// file, so the agent upload fails as it does where no folder is writable. The session must
/// say so, read three samples from the script with the host's own memory and core count,
/// and stop processes through `kill` (one that runs, pid 0, and one no process has). The
/// folder comes back afterwards. It changes the host, so it runs only when
/// `VITALS_TEST_FALLBACK=1` is set and the host is a loopback address.
#[test]
fn script_fallback_streams_metrics() {
    let _host = HOST.lock().unwrap_or_else(PoisonError::into_inner);
    if std::env::var("VITALS_TEST_FALLBACK").as_deref() != Ok("1") {
        eprintln!("VITALS_TEST_FALLBACK is not 1; skipped");
        return;
    }
    let Some((target, store, dir)) = live("fallback") else {
        return;
    };
    assert!(
        matches!(target.host.as_str(), "127.0.0.1" | "::1" | "localhost"),
        "the fallback test changes the host, so it runs only against a local docker sshd"
    );
    let runtime = session::runtime();
    let host = runtime.block_on(open(&target, &store));
    let sh = |command: &str| {
        let done = runtime
            .block_on(host.exec_capture(command))
            .expect("the command runs");
        done.stdout.trim().to_string()
    };
    sh(r#"rm -rf "$HOME/.cache/vitals" && mkdir -p "$HOME/.cache" && : > "$HOME/.cache/vitals""#);
    let memory: f64 = sh("awk '/^MemTotal:/ { print $2 }' /proc/meminfo")
        .parse()
        .expect("MemTotal");
    let cores: usize = sh("grep -c '^cpu[0-9]' /proc/stat").parse().expect("cores");
    let sleeper: u32 = sh("sleep 300 </dev/null >/dev/null 2>&1 & echo $!")
        .parse()
        .expect("a pid");

    let (handle, events) = session::start(target, store, Reconnect::default());
    let mut events = events.session;
    let run = async {
        let mut seen = Fallback::default();
        while let Some(event) = events.next().await {
            match event {
                SessionEvent::Metrics(snapshot) => {
                    eprintln!(
                        "Metrics: cpu {:.1}% over {} cores, {} interfaces, {} disks, {} processes",
                        snapshot.cpu_total_percent,
                        snapshot.cpu_cores.len(),
                        snapshot.network.len(),
                        snapshot.disks.len(),
                        snapshot.processes.len()
                    );
                    seen.snapshots.push(*snapshot);
                    if seen.snapshots.len() == 3 {
                        handle.kill(sleeper, Signal::Term, "sleeper".into());
                        handle.kill(0, Signal::Term, "zero".into());
                        handle.kill(i32::MAX as u32, Signal::Kill, "missing".into());
                    }
                    continue;
                }
                ref other => eprintln!("{other:?}"),
            }
            match event {
                SessionEvent::HostKey(_) => return Err("asked for the host key again".into()),
                SessionEvent::Hello(_) => return Err("the agent started".to_string()),
                SessionEvent::Fallback(reason) => seen.reason = Some(reason),
                SessionEvent::Status(Status::Connecting, Some(message))
                    if message.starts_with("Agent unavailable") =>
                {
                    seen.notice = Some(message)
                }
                SessionEvent::Status(Status::Connected, _) => seen.connected = true,
                SessionEvent::Status(Status::Error, message) => {
                    return Err(format!("session error: {message:?}"));
                }
                SessionEvent::KillResult { id, ok, error } => {
                    seen.kills.insert(id, (ok, error));
                    if seen.kills.len() == 3 {
                        handle.disconnect();
                    }
                }
                SessionEvent::Ended => break,
                _ => {}
            }
        }
        Ok(seen)
    };
    let seen = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(60), run).await });
    sh(&format!(
        r#"rm -f "$HOME/.cache/vitals"; kill {sleeper} 2>/dev/null; true"#
    ));
    runtime.block_on(host.close());
    let _ = std::fs::remove_dir_all(dir);

    let seen = seen
        .expect("the session finishes in a minute")
        .expect("the fallback session");
    let reason = seen.reason.expect("the session fell back");
    assert!(reason.starts_with("Agent upload failed"), "{reason}");
    assert_eq!(
        seen.notice.as_deref(),
        Some(format!("Agent unavailable; using script fallback ({reason})").as_str())
    );
    assert!(seen.connected);
    assert_eq!(seen.snapshots.len(), 3);
    let first = &seen.snapshots[0];
    assert_eq!(first.memory.total_kb, memory);
    assert!(first.memory.used_kb > 0.0 && first.memory.used_kb <= memory);
    assert_eq!(first.cpu_cores.len(), cores);
    assert!(first.uptime_seconds > 0.0);
    assert!(!first.disks.is_empty(), "df runs on the first sample");
    assert!(first.network.iter().all(|interface| interface.name != "lo"));
    for snapshot in &seen.snapshots[1..] {
        assert!((0.0..=100.0).contains(&snapshot.cpu_total_percent));
        assert_eq!(
            snapshot.disks, first.disks,
            "df only every 10s, kept between"
        );
    }
    assert!(seen.snapshots[2].server_timestamp > first.server_timestamp);
    assert_eq!(seen.kills["sleeper"], (true, None));
    assert_eq!(seen.kills["zero"], (false, Some("Invalid pid".into())));
    let (ok, error) = &seen.kills["missing"];
    assert!(
        !ok && error
            .as_deref()
            .unwrap_or("")
            .starts_with("kill exited with")
    );
}
