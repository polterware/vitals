//! The vitals-agent wire format: NDJSON frames on the agent's stdout (protocol v1), the
//! commands written to its stdin, and the snapshot a metrics frame becomes.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub v: u32,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub cpu_count: u32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CpuCore {
    pub name: String,
    pub usage_percent: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub total_kb: f64,
    pub available_kb: f64,
    pub used_kb: f64,
    pub swap_total_kb: f64,
    pub swap_free_kb: f64,
    pub swap_used_kb: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    pub name: String,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiskIo {
    pub device: String,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub util_percent: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Pressure {
    pub cpu: f64,
    pub memory: f64,
    pub io: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub filesystem: String,
    pub size_kb: f64,
    pub used_kb: f64,
    pub avail_kb: f64,
    pub use_percent: f64,
    pub mount_point: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    pub pid: u32,
    pub user: String,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub command: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Thermal {
    pub id: String,
    pub celsius: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub index: u32,
    pub name: String,
    pub temperature_c: f64,
    pub utilization_percent: f64,
    pub memory_used_mb: f64,
    pub memory_total_mb: f64,
}

/// Fast fields come on every tick; disks, processes, thermals, and gpus only on the tick
/// that collected them again, and pressure only where `/proc/pressure` exists.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub ts_unix_ms: u64,
    #[serde(default)]
    pub mono_ns: u64,
    pub cpu_total_percent: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub cpu_cores: Vec<CpuCore>,
    #[serde(default, deserialize_with = "nullable")]
    pub load: Vec<f64>,
    #[serde(default)]
    pub uptime_seconds: f64,
    pub memory: Memory,
    #[serde(default, deserialize_with = "nullable")]
    pub network: Vec<NetworkInterface>,
    #[serde(default, deserialize_with = "nullable")]
    pub disk_io: Vec<DiskIo>,
    pub pressure: Option<Pressure>,
    pub disks: Option<Vec<Disk>>,
    pub processes: Option<Vec<Process>>,
    pub thermals: Option<Vec<Thermal>>,
    pub gpus: Option<Vec<Gpu>>,
}

/// The Go agent, still on hosts that have not updated, writes an empty slice as `null`.
fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Hello(Hello),
    Metrics(Box<Metrics>),
    KillResult {
        id: String,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    Pong,
    Warn {
        #[serde(default)]
        collector: String,
        #[serde(default)]
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum Signal {
    #[serde(rename = "TERM")]
    Term,
    #[serde(rename = "KILL")]
    Kill,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Kill {
        pid: u32,
        signal: Signal,
        id: String,
    },
    Ping,
}

impl Command {
    /// The command as one NDJSON line.
    pub fn line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("commands serialize");
        line.push('\n');
        line
    }
}

#[derive(Debug, PartialEq)]
pub enum FrameError {
    Invalid,
    /// A frame whose `type` this app does not know; skipped after the hello.
    Unknown(String),
}

pub fn parse_frame(line: &str) -> Result<Frame, FrameError> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| FrameError::Invalid)?;
    let Some(kind) = value.get("type").and_then(|kind| kind.as_str()) else {
        return Err(FrameError::Invalid);
    };
    let kind = kind.to_string();
    serde_json::from_value(value).map_err(|_| match kind.as_str() {
        "hello" | "metrics" | "kill_result" | "pong" | "warn" => FrameError::Invalid,
        _ => FrameError::Unknown(kind),
    })
}

/// The first frame must be a hello at this protocol's version.
pub fn check_hello(frame: &Frame) -> Result<&Hello, String> {
    match frame {
        Frame::Hello(hello) if hello.v == PROTOCOL_VERSION => Ok(hello),
        Frame::Hello(hello) => Err(format!("Unsupported agent protocol version: {}", hello.v)),
        _ => Err("expected hello frame".into()),
    }
}

/// Accumulates stdout chunks into lines; a partial trailing line stays buffered.
#[derive(Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(newline) = self.buf.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                lines.push(String::from_utf8_lossy(&line).into_owned());
            }
        }
        lines
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// What a host looks like now: the latest fast fields with the last slow fields kept.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub cpu_total_percent: f64,
    pub cpu_cores: Vec<CpuCore>,
    pub load: [f64; 3],
    pub uptime_seconds: f64,
    pub memory: Memory,
    pub network: Vec<NetworkInterface>,
    pub disk_io: Vec<DiskIo>,
    pub pressure: Option<Pressure>,
    pub disks: Vec<Disk>,
    pub processes: Vec<Process>,
    pub thermals: Vec<Thermal>,
    pub gpus: Vec<Gpu>,
    pub server_timestamp: u64,
}

impl Snapshot {
    /// The next snapshot from a metrics frame, keeping this one's slow fields where the
    /// frame has none.
    pub fn next(&self, frame: Metrics) -> Snapshot {
        let load = |index: usize| frame.load.get(index).copied().unwrap_or(0.0);
        Snapshot {
            cpu_total_percent: frame.cpu_total_percent,
            load: [load(0), load(1), load(2)],
            cpu_cores: frame.cpu_cores,
            uptime_seconds: frame.uptime_seconds,
            memory: frame.memory,
            network: frame.network,
            disk_io: frame.disk_io,
            pressure: frame.pressure,
            disks: frame.disks.unwrap_or_else(|| self.disks.clone()),
            processes: frame.processes.unwrap_or_else(|| self.processes.clone()),
            thermals: frame.thermals.unwrap_or_else(|| self.thermals.clone()),
            gpus: frame.gpus.unwrap_or_else(|| self.gpus.clone()),
            server_timestamp: frame.ts_unix_ms,
        }
    }

    pub fn memory_fraction(&self) -> f64 {
        if self.memory.total_kb <= 0.0 {
            return 0.0;
        }
        (self.memory.used_kb / self.memory.total_kb).clamp(0.0, 1.0)
    }

    /// Bytes per second received and sent, summed over the interfaces.
    pub fn network_totals(&self) -> (f64, f64) {
        self.network.iter().fold((0.0, 0.0), |(rx, tx), interface| {
            (
                rx + interface.rx_bytes_per_sec,
                tx + interface.tx_bytes_per_sec,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: &str = r#"{"type":"metrics","tsUnixMs":1700000000000,"monoNs":1,"cpuTotalPercent":12.5,"cpuCores":[{"name":"cpu0","usagePercent":10}],"load":[0.5,0.4,0.3],"uptimeSeconds":3600,"memory":{"totalKb":1000,"availableKb":600,"usedKb":400,"swapTotalKb":0,"swapFreeKb":0,"swapUsedKb":0},"network":[{"name":"eth0","rxBytesPerSec":100,"txBytesPerSec":50},{"name":"eth1","rxBytesPerSec":1,"txBytesPerSec":2}],"diskIo":null,"disks":[{"filesystem":"/dev/sda1","sizeKb":10,"usedKb":5,"availKb":5,"usePercent":50,"mountPoint":"/"}]}"#;

    #[test]
    fn lines_split_across_chunks() {
        let mut buffer = LineBuffer::default();
        assert!(buffer.push(b"{\"type\":").is_empty());
        assert_eq!(
            buffer.push(b"\"pong\"}\r\n\n{\"a\""),
            vec!["{\"type\":\"pong\"}"]
        );
        assert_eq!(buffer.push(b":1}\n"), vec!["{\"a\":1}"]);
        buffer.push(b"partial");
        buffer.reset();
        assert_eq!(buffer.push(b"x\n"), vec!["x"]);
    }

    #[test]
    fn hello_must_match_the_protocol() {
        let hello = parse_frame(r#"{"type":"hello","v":1,"agentVersion":"1.1.0","arch":"amd64","hostname":"box","kernel":"6.1","cpuCount":4}"#).unwrap();
        assert_eq!(check_hello(&hello).unwrap().hostname, "box");
        let newer = parse_frame(r#"{"type":"hello","v":2}"#).unwrap();
        assert_eq!(
            check_hello(&newer).unwrap_err(),
            "Unsupported agent protocol version: 2"
        );
        assert!(check_hello(&Frame::Pong).is_err());
    }

    #[test]
    fn bad_and_unknown_lines() {
        assert_eq!(parse_frame("not json"), Err(FrameError::Invalid));
        assert_eq!(parse_frame("[1]"), Err(FrameError::Invalid));
        assert_eq!(
            parse_frame(r#"{"type":"metrics"}"#),
            Err(FrameError::Invalid)
        );
        assert_eq!(
            parse_frame(r#"{"type":"later"}"#),
            Err(FrameError::Unknown("later".into()))
        );
        assert_eq!(
            parse_frame(r#"{"type":"kill_result","id":"k1","ok":false,"error":"EPERM"}"#),
            Ok(Frame::KillResult {
                id: "k1".into(),
                ok: false,
                error: Some("EPERM".into())
            })
        );
    }

    #[test]
    fn slow_fields_carry_over() {
        let Frame::Metrics(first) = parse_frame(METRICS).unwrap() else {
            panic!("metrics frame");
        };
        let snapshot = Snapshot::default().next(*first.clone());
        assert_eq!(snapshot.load, [0.5, 0.4, 0.3]);
        assert_eq!(snapshot.disks.len(), 1);
        assert_eq!(snapshot.network_totals(), (101.0, 52.0));
        assert!((snapshot.memory_fraction() - 0.4).abs() < 1e-9);

        let mut fast = *first;
        fast.disks = None;
        fast.cpu_total_percent = 80.0;
        let next = snapshot.next(fast);
        assert_eq!(next.cpu_total_percent, 80.0);
        assert_eq!(next.disks, snapshot.disks);
    }

    #[test]
    fn commands_are_lines() {
        let kill = Command::Kill {
            pid: 42,
            signal: Signal::Term,
            id: "k1".into(),
        };
        assert_eq!(
            kill.line(),
            "{\"cmd\":\"kill\",\"pid\":42,\"signal\":\"TERM\",\"id\":\"k1\"}\n"
        );
        assert_eq!(Command::Ping.line(), "{\"cmd\":\"ping\"}\n");
    }
}
