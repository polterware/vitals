//! The vitals-agent: a small read-only process Vitals starts on a host over SSH. It prints
//! NDJSON frames on stdout (protocol v1: `hello`, `metrics`, `kill_result`, `pong`, `warn`)
//! and reads `kill` and `ping` commands on stdin, one JSON object a line.
//!
//! [`serve`] is the stream and knows no platform; each host module (`/proc` on Linux, Mach,
//! libproc and IOKit on macOS, the Win32 API on Windows) is a [`serve::Collector`].

pub mod commands;
pub mod gpu;
pub mod hub;
pub mod metrics;
pub mod protocol;
pub mod serve;

mod host;

use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serve::Event;

/// How often a metrics frame goes out.
pub const FAST_EVERY: Duration = Duration::from_secs(1);

/// Runs the agent on this host's stdin and stdout until stdin closes or stdout breaks.
pub fn run() -> io::Result<()> {
    let (collector, kill) = host::open()?;
    let (events, receiver) = mpsc::channel();
    start_ticker(events.clone(), FAST_EVERY);
    serve::serve(io::stdin(), io::stdout(), collector, kill, events, receiver)
}

/// Sends a tick every `every`, on a schedule that does not drift with the work it starts,
/// until the stream stops listening.
fn start_ticker(events: mpsc::Sender<Event>, every: Duration) {
    std::thread::spawn(move || {
        let mut next = Instant::now();
        loop {
            if events.send(Event::Tick).is_err() {
                return;
            }
            next += every;
            let now = Instant::now();
            match next.checked_duration_since(now) {
                Some(wait) => std::thread::sleep(wait),
                // Behind (the machine slept): start the schedule again from now.
                None => next = now,
            }
        }
    });
}
