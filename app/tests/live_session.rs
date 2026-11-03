//! A session against a real SSH server, run only when `VITALS_TEST_HOST` names one:
//! `VITALS_TEST_HOST=127.0.0.1 VITALS_TEST_PORT=22022 VITALS_TEST_USER=me
//! VITALS_TEST_KEY=/path/to/key cargo test -p vitals-app --test live_session -- --nocapture`
//! after `agent/build.sh`. It approves the host key in a temporary store, installs the
//! agent, and waits for the hello and two metrics frames.

use std::time::Duration;

use futures::StreamExt;
use vitals_app::session::{self, Reconnect, SessionEvent, Status};
use vitals_app::ssh::Target;
use vitals_app::store::{AuthMode, Store};

#[test]
fn agent_streams_metrics() {
    let Ok(host) = std::env::var("VITALS_TEST_HOST") else {
        eprintln!("VITALS_TEST_HOST is not set; skipped");
        return;
    };
    let port = std::env::var("VITALS_TEST_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(22);
    let username = std::env::var("VITALS_TEST_USER").expect("VITALS_TEST_USER");
    let key = std::env::var("VITALS_TEST_KEY").ok();
    let dir = std::env::temp_dir().join(format!("vitals-live-{}", std::process::id()));
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
    let (handle, mut events) = session::start(target.clone(), store.clone(), Reconnect::default());
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
    let (handle, mut events) = session::start(target, store, Reconnect::default());
    let second = session::runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            while let Some(event) = events.next().await {
                match event {
                    SessionEvent::HostKey(_) => return false,
                    SessionEvent::Status(Status::Connected, _) => {
                        handle.disconnect();
                        return true;
                    }
                    SessionEvent::Status(Status::Error, message) => {
                        panic!("session error: {message:?}")
                    }
                    _ => {}
                }
            }
            false
        })
        .await
    });
    assert_eq!(second, Ok(true));
    let _ = std::fs::remove_dir_all(dir);
}
