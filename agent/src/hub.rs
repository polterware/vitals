//! The only stdout writer. Control lines (hello, replies, warnings) go out in order; metrics
//! frames are not queued: a newer frame replaces one that has not been written yet, so a slow
//! SSH channel reads the latest state instead of a backlog.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Condvar, Mutex};

#[derive(Default)]
struct State {
    ctrl: VecDeque<Vec<u8>>,
    metric: Option<Vec<u8>>,
    closed: bool,
}

#[derive(Default)]
pub struct Hub {
    state: Mutex<State>,
    ready: Condvar,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_ctrl(&self, line: Vec<u8>) {
        let mut state = self.state.lock().expect("hub lock");
        if state.closed {
            return;
        }
        state.ctrl.push_back(line);
        self.ready.notify_one();
    }

    pub fn push_metric(&self, line: Vec<u8>) {
        let mut state = self.state.lock().expect("hub lock");
        if state.closed {
            return;
        }
        state.metric = Some(line);
        self.ready.notify_one();
    }

    /// No more lines are taken; the writer finishes the queued control lines and stops.
    pub fn close(&self) {
        let mut state = self.state.lock().expect("hub lock");
        state.closed = true;
        self.ready.notify_all();
    }

    /// Writes lines until the hub is closed and drained, or a write fails.
    pub fn run(&self, out: &mut impl Write) -> io::Result<()> {
        loop {
            let line = {
                let mut state = self.state.lock().expect("hub lock");
                while state.ctrl.is_empty() && state.metric.is_none() && !state.closed {
                    state = self.ready.wait(state).expect("hub lock");
                }
                match state.ctrl.pop_front() {
                    Some(line) => line,
                    None if state.closed => return Ok(()),
                    None => state.metric.take().expect("a metric is waiting"),
                }
            };
            let written = write_line(out, line);
            if written.is_err() {
                self.close();
                return written;
            }
        }
    }
}

fn write_line(out: &mut impl Write, mut line: Vec<u8>) -> io::Result<()> {
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Blocks its first write until the test lets it go.
    struct Gate {
        started: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
                self.release.recv().unwrap();
            }
            let text = String::from_utf8_lossy(buf).trim_end().to_string();
            self.lines.lock().unwrap().push(text);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_hub_drops_intermediate_metrics() {
        let hub = Arc::new(Hub::new());
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let mut gate = Gate {
            started: Some(started_tx),
            release: release_rx,
            lines: lines.clone(),
        };
        let writer = {
            let hub = hub.clone();
            std::thread::spawn(move || hub.run(&mut gate))
        };
        hub.push_metric(b"A".to_vec());
        started
            .recv_timeout(Duration::from_secs(2))
            .expect("the write started");
        hub.push_metric(b"B".to_vec());
        hub.push_metric(b"C".to_vec());
        hub.push_ctrl(b"D".to_vec());
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        hub.close();
        writer.join().unwrap().unwrap();
        assert_eq!(*lines.lock().unwrap(), ["A", "D", "C"]);
    }

    #[test]
    fn a_closed_hub_still_writes_its_control_lines() {
        let hub = Hub::new();
        hub.push_ctrl(b"one".to_vec());
        hub.push_metric(b"late".to_vec());
        hub.close();
        hub.push_ctrl(b"after".to_vec());
        let mut out = Vec::new();
        hub.run(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "one\n");
    }
}
