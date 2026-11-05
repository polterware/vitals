//! The frames the agent writes, protocol v1. Field names and their camelCase spelling are the
//! wire format the apps read; a change keeps old readers working or bumps
//! [`PROTOCOL_VERSION`]. Slow fields (`disks`, `processes`, `thermals`, `gpus`) and
//! `pressure` are left out of a frame that did not collect them.

use serde::Serialize;

pub const PROTOCOL_VERSION: u32 = 1;
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A finite number: NaN and infinities have no JSON form.
pub fn finite(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

/// The architecture in the names the installer uses (`amd64`, `arm64`).
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub v: u32,
    pub agent_version: String,
    pub arch: String,
    pub hostname: String,
    pub kernel: String,
    pub cpu_count: u32,
}

impl Hello {
    pub fn new(hostname: String, kernel: String, cpu_count: u32) -> Self {
        Self {
            kind: "hello",
            v: PROTOCOL_VERSION,
            agent_version: AGENT_VERSION.to_string(),
            arch: arch().to_string(),
            hostname,
            kernel,
            cpu_count,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Core {
    pub name: String,
    pub usage_percent: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub total_kb: u64,
    pub available_kb: u64,
    pub used_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    pub swap_used_kb: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Net {
    pub name: String,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskIo {
    pub device: String,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub util_percent: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Pressure {
    pub cpu: f64,
    pub memory: f64,
    pub io: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub filesystem: String,
    pub size_kb: u64,
    pub used_kb: u64,
    pub avail_kb: u64,
    pub use_percent: f64,
    pub mount_point: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    pub pid: i64,
    pub user: String,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub command: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Thermal {
    pub id: String,
    pub celsius: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub index: i64,
    pub name: String,
    pub temperature_c: f64,
    pub utilization_percent: f64,
    pub memory_used_mb: f64,
    pub memory_total_mb: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub ts_unix_ms: i64,
    pub mono_ns: i64,
    pub cpu_total_percent: f64,
    pub cpu_cores: Vec<Core>,
    pub load: [f64; 3],
    pub uptime_seconds: f64,
    pub memory: Memory,
    pub network: Vec<Net>,
    pub disk_io: Vec<DiskIo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<Pressure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disks: Option<Vec<Disk>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processes: Option<Vec<Process>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermals: Option<Vec<Thermal>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpus: Option<Vec<Gpu>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KillResult {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pong {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Warn {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub collector: String,
    pub message: String,
}

/// Writes a whole float as an integer (`0`, not `0.0`), as Go's `encoding/json` did, which
/// keeps a frame about a tenth shorter on the wire; any other float keeps its shortest form.
struct WireFormatter;

/// The largest float every integer up to is exact in.
const EXACT: f64 = 9_007_199_254_740_992.0;

impl serde_json::ser::Formatter for WireFormatter {
    fn write_f64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        if value.fract() == 0.0 && value.abs() < EXACT {
            return self.write_i64(writer, value as i64);
        }
        serde_json::ser::CompactFormatter.write_f64(writer, value)
    }

    fn write_f32<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f32,
    ) -> std::io::Result<()> {
        self.write_f64(writer, f64::from(value))
    }
}

/// One frame as a line without its newline.
pub fn line<T: Serialize>(frame: &T) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, WireFormatter);
    frame.serialize(&mut serializer).expect("frames serialize");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_floats_are_integers() {
        #[derive(Serialize)]
        struct Numbers {
            whole: f64,
            part: f64,
            negative: f64,
            huge: f64,
        }
        let text = String::from_utf8(line(&Numbers {
            whole: 0.0,
            part: 12.5,
            negative: -3.0,
            huge: 1e300,
        }))
        .unwrap();
        assert_eq!(
            text,
            r#"{"whole":0,"part":12.5,"negative":-3,"huge":1e+300}"#
        );
    }

    #[test]
    fn frames_keep_the_wire_names() {
        let hello = String::from_utf8(line(&Hello::new("box".into(), "6.8.0".into(), 4))).unwrap();
        assert!(
            hello.starts_with(r#"{"type":"hello","v":1,"agentVersion":""#),
            "{hello}"
        );
        assert!(hello.contains(r#""hostname":"box","kernel":"6.8.0","cpuCount":4}"#));
        let kill = KillResult {
            kind: "kill_result",
            id: "k1".into(),
            ok: true,
            error: String::new(),
        };
        assert_eq!(
            String::from_utf8(line(&kill)).unwrap(),
            r#"{"type":"kill_result","id":"k1","ok":true}"#
        );
        let warn = Warn {
            kind: "warn",
            collector: "cpu".into(),
            message: "<stat>".into(),
        };
        // No HTML escaping, as the Go agent wrote it.
        assert_eq!(
            String::from_utf8(line(&warn)).unwrap(),
            r#"{"type":"warn","collector":"cpu","message":"<stat>"}"#
        );
        assert_eq!(finite(f64::NAN), 0.0);
        assert_eq!(finite(2.5), 2.5);
    }
}
