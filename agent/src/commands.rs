//! The commands on stdin: `{"cmd":"ping"}` and `{"cmd":"kill","pid":42,"signal":"TERM",
//! "id":"k1"}`. A kill is allowed only for a pid above 1 that runs as the agent's own user,
//! with TERM or KILL; the host checks the owner.

use serde::Deserialize;
use serde_json::Value;

use crate::protocol::{self, KillResult, Pong};

/// Stops a process: `(pid, signal)`, an error to report when it cannot.
pub type KillFn = std::sync::Arc<dyn Fn(i64, &str) -> Result<(), String> + Send + Sync>;

/// TERM or KILL, for a pid > 1 owned by the agent's user. `owner` is the pid's owner, or why
/// it could not be read.
pub fn authorize_kill(
    pid: i64,
    agent_uid: u32,
    owner: Result<u32, String>,
    signal: &str,
) -> Result<(), String> {
    if pid <= 1 {
        return Err("pid must be greater than 1".into());
    }
    if signal != "TERM" && signal != "KILL" {
        return Err("signal must be TERM or KILL".into());
    }
    if owner? != agent_uid {
        return Err("pid belongs to another user".into());
    }
    Ok(())
}

/// An integer pid: `2.5`, `"42"`, `1e3`, and `null` are not.
pub fn strict_pid(raw: Option<&Value>) -> Result<i64, String> {
    raw.and_then(Value::as_i64)
        .ok_or_else(|| "pid must be an integer".to_string())
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    pid: Option<Value>,
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

/// One command line: the reply to write, if any, and a warning when the line is not a
/// command. Unknown commands are ignored, so newer apps can talk to this agent.
pub fn dispatch(
    line: &[u8],
    kill: &dyn Fn(i64, &str) -> Result<(), String>,
) -> (Option<Vec<u8>>, Option<&'static str>) {
    let Ok(raw) = serde_json::from_slice::<Raw>(line) else {
        return (None, Some("invalid command"));
    };
    match raw.cmd.as_deref().unwrap_or("") {
        "ping" => (Some(protocol::line(&Pong { kind: "pong" })), None),
        "kill" => {
            let mut result = KillResult {
                kind: "kill_result",
                id: raw.id.unwrap_or_default(),
                ok: false,
                error: String::new(),
            };
            let outcome = strict_pid(raw.pid.as_ref())
                .and_then(|pid| kill(pid, raw.signal.as_deref().unwrap_or("")));
            match outcome {
                Ok(()) => result.ok = true,
                Err(error) => result.error = error,
            }
            (Some(protocol::line(&result)), None)
        }
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn kills_are_authorized() {
        type Case = (i64, u32, Result<u32, String>, &'static str, bool);
        let missing = || Err("process not found".to_string());
        let cases: [Case; 7] = [
            (1, 0, Ok(0), "TERM", false),
            (0, 0, Ok(0), "KILL", false),
            (42, 10, Ok(10), "HUP", false),
            (42, 10, Ok(0), "TERM", false),
            (42, 10, missing(), "TERM", false),
            (42, 10, Ok(10), "TERM", true),
            (42, 10, Ok(10), "KILL", true),
        ];
        for (pid, agent, owner, signal, allowed) in cases {
            assert_eq!(
                authorize_kill(pid, agent, owner, signal).is_ok(),
                allowed,
                "{pid} {signal}"
            );
        }
    }

    #[test]
    fn commands_dispatch() {
        let called = RefCell::new(None);
        let kill = |pid: i64, signal: &str| {
            *called.borrow_mut() = Some((pid, signal.to_string()));
            authorize_kill(pid, 10, Ok(10), signal)
        };
        let reply = |line: &str| {
            let (reply, warn) = dispatch(line.as_bytes(), &kill);
            (
                reply.map(|reply| serde_json::from_slice::<Value>(&reply).unwrap()),
                warn,
            )
        };

        let (pong, warn) = reply(r#"{"cmd":"ping"}"#);
        assert_eq!(pong.unwrap()["type"], "pong");
        assert!(warn.is_none());

        let (done, _) = reply(r#"{"cmd":"kill","pid":42,"signal":"TERM","id":"abc"}"#);
        let done = done.unwrap();
        assert_eq!(done["type"], "kill_result");
        assert_eq!(done["ok"], true);
        assert_eq!(done["id"], "abc");
        assert!(done.get("error").is_none());
        assert_eq!(*called.borrow(), Some((42, "TERM".to_string())));

        *called.borrow_mut() = None;
        let (fraction, _) = reply(r#"{"cmd":"kill","pid":2.5,"signal":"KILL","id":"x"}"#);
        assert_eq!(fraction.unwrap()["error"], "pid must be an integer");
        assert!(
            called.borrow().is_none(),
            "a bad pid never reaches the host"
        );
        for pid in [r#""42""#, "1e3", "null"] {
            let line = format!(r#"{{"cmd":"kill","pid":{pid},"signal":"KILL","id":"x"}}"#);
            assert_eq!(reply(&line).0.unwrap()["ok"], false, "{pid}");
        }

        let (first, _) = reply(r#"{"cmd":"kill","pid":1,"signal":"TERM","id":"x"}"#);
        assert_eq!(first.unwrap()["error"], "pid must be greater than 1");

        assert_eq!(reply(r#"{"cmd":"nope"}"#), (None, None));
        assert_eq!(reply("{"), (None, Some("invalid command")));
        assert_eq!(reply(r#"{"cmd":5}"#), (None, Some("invalid command")));
        assert_eq!(reply("[1]"), (None, Some("invalid command")));
    }
}
