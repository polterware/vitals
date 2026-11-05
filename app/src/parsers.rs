//! The script fallback's parsers, from the Electron app's `shared/parsers`. The remote script
//! ([`crate::script`]) prints each file under a `VITALS_SECTION <name>` line and ends every
//! sample with `VITALS_SECTION end`; a sample becomes the same [`Metrics`] frame the agent
//! sends, so the snapshot, its slow fields, and the Metrics page are the agent's.
//!
//! Numbers follow the Electron parsers' `Number(text) || 0`: text that is not a number is 0.

use std::collections::HashMap;

use crate::protocol::{CpuCore, Disk, Gpu, Memory, Metrics, NetworkInterface, Process, Thermal};

pub const SECTION_MARKER: &str = "VITALS_SECTION";

/// The line the script prints after each complete sample.
const FRAME_END_LINE: &[u8] = b"VITALS_SECTION end\n";

/// A sample larger than this is not the script's; the buffer starts over.
const MAX_FRAME_BUFFER: usize = 4 * 1024 * 1024;

/// JavaScript's `Number(text)` for the decimal text a host prints: blank text is 0, and text
/// that is not a finite number is `None`.
fn js_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return Some(0.0);
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// `Number(text) || 0`.
fn number(text: &str) -> f64 {
    js_number(text).unwrap_or(0.0)
}

/// Each section's text by its name, trimmed.
pub fn split_sections(raw: &str) -> HashMap<String, String> {
    let mut sections = HashMap::new();
    let mut current: Option<String> = None;
    let mut lines: Vec<&str> = Vec::new();
    let prefix = format!("{SECTION_MARKER} ");
    for line in raw.split('\n') {
        if let Some(name) = line.strip_prefix(&prefix) {
            if let Some(name) = current.take() {
                sections.insert(name, lines.join("\n").trim().to_string());
            }
            lines.clear();
            current = Some(name.trim().to_string());
            continue;
        }
        if current.is_some() {
            lines.push(line);
        }
    }
    if let Some(name) = current {
        sections.insert(name, lines.join("\n").trim().to_string());
    }
    sections
}

/// Splits the script's streamed output into complete samples; a partial one stays buffered.
#[derive(Default)]
pub struct FrameBuffer {
    buf: Vec<u8>,
}

impl FrameBuffer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = self
            .buf
            .windows(FRAME_END_LINE.len())
            .position(|window| window == FRAME_END_LINE)
        {
            let frame: Vec<u8> = self.buf.drain(..end + FRAME_END_LINE.len()).collect();
            frames.push(String::from_utf8_lossy(&frame[..end]).into_owned());
        }
        if self.buf.len() > MAX_FRAME_BUFFER {
            self.buf.clear();
        }
        frames
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// One `cpu` line of `/proc/stat`, in clock ticks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcStatSample {
    pub name: String,
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub idle: f64,
    pub iowait: f64,
    pub irq: f64,
    pub softirq: f64,
    pub steal: f64,
    pub guest: f64,
    pub guest_nice: f64,
}

pub fn parse_proc_stat(text: &str) -> Vec<ProcStatSample> {
    let mut samples = Vec::new();
    for line in text.split('\n') {
        if !line.starts_with("cpu") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let nums: Vec<f64> = parts.map(number).collect();
        let at = |index: usize| nums.get(index).copied().unwrap_or(0.0);
        samples.push(ProcStatSample {
            name: name.to_string(),
            user: at(0),
            nice: at(1),
            system: at(2),
            idle: at(3),
            iowait: at(4),
            irq: at(5),
            softirq: at(6),
            steal: at(7),
            guest: at(8),
            guest_nice: at(9),
        });
    }
    samples
}

/// Guest and guest-nice time are already inside user and nice (proc(5)); adding them again
/// would count time spent running VMs twice.
fn busy_ticks(sample: &ProcStatSample) -> f64 {
    sample.user + sample.nice + sample.system + sample.irq + sample.softirq + sample.steal
}

fn total_ticks(sample: &ProcStatSample) -> f64 {
    busy_ticks(sample) + sample.idle + sample.iowait
}

pub fn cpu_usage_percent(prev: &ProcStatSample, curr: &ProcStatSample) -> f64 {
    let total = total_ticks(curr) - total_ticks(prev);
    if total <= 0.0 {
        return 0.0;
    }
    let busy = busy_ticks(curr) - busy_ticks(prev);
    (busy / total * 100.0).clamp(0.0, 100.0)
}

/// A `Key:   123 kB` line of `/proc/meminfo`.
fn meminfo_line(line: &str) -> Option<(&str, f64)> {
    let (key, rest) = line.split_once(':')?;
    let key_ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '(' || c == ')');
    let value = rest.trim_start();
    if !key_ok || value.len() == rest.len() {
        return None;
    }
    let digits = &value[..value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len())];
    Some((key, digits.parse().ok()?))
}

pub fn parse_meminfo(text: &str) -> Memory {
    let map: HashMap<&str, f64> = text.split('\n').filter_map(meminfo_line).collect();
    let kb = |key: &str| map.get(key).copied().unwrap_or(0.0);
    let total_kb = kb("MemTotal");
    let available_kb = Some(kb("MemAvailable"))
        .filter(|available| *available != 0.0)
        .unwrap_or_else(|| kb("MemFree"));
    let swap_total_kb = kb("SwapTotal");
    let swap_free_kb = kb("SwapFree");
    Memory {
        total_kb,
        available_kb,
        used_kb: (total_kb - available_kb).max(0.0),
        swap_total_kb,
        swap_free_kb,
        swap_used_kb: (swap_total_kb - swap_free_kb).max(0.0),
    }
}

/// An interface's byte counters from `/proc/net/dev`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetDevSample {
    pub name: String,
    pub rx_bytes: f64,
    pub tx_bytes: f64,
}

/// The interfaces of `/proc/net/dev`, loopback left out.
pub fn parse_net_dev(text: &str) -> Vec<NetDevSample> {
    let mut samples = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Inter-") || trimmed.starts_with("face") {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let rest: Vec<&str> = rest.split_whitespace().collect();
        if rest.len() < 16 {
            continue;
        }
        let name = name.trim();
        if name == "lo" {
            continue;
        }
        samples.push(NetDevSample {
            name: name.to_string(),
            rx_bytes: number(rest[0]),
            tx_bytes: number(rest[8]),
        });
    }
    samples
}

/// Bytes per second between two samples `interval` seconds apart; an interface new in
/// `curr`, or a counter that went back, reads 0.
pub fn net_rates(
    prev: &[NetDevSample],
    curr: &[NetDevSample],
    interval: f64,
) -> Vec<NetworkInterface> {
    if interval <= 0.0 {
        return Vec::new();
    }
    curr.iter()
        .map(|sample| {
            let before = prev.iter().find(|before| before.name == sample.name);
            let rate = |now: f64, then: Option<f64>| {
                then.map_or(0.0, |then| (now - then).max(0.0) / interval)
            };
            NetworkInterface {
                name: sample.name.clone(),
                rx_bytes_per_sec: rate(sample.rx_bytes, before.map(|b| b.rx_bytes)),
                tx_bytes_per_sec: rate(sample.tx_bytes, before.map(|b| b.tx_bytes)),
            }
        })
        .collect()
}

/// The 1, 5, and 15 minute load averages of `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> [f64; 3] {
    let mut parts = text.split_whitespace().map(number);
    let mut next = || parts.next().unwrap_or(0.0);
    [next(), next(), next()]
}

/// The seconds since boot in `/proc/uptime`.
pub fn parse_uptime(text: &str) -> f64 {
    text.split_whitespace().next().map_or(0.0, number)
}

/// `df -P -k`, past its header.
pub fn parse_df(text: &str) -> Vec<Disk> {
    text.trim()
        .split('\n')
        .skip(1)
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 6 {
                return None;
            }
            Some(Disk {
                filesystem: parts[0].to_string(),
                size_kb: number(parts[1]),
                used_kb: number(parts[2]),
                avail_kb: number(parts[3]),
                use_percent: number(&parts[4].replace('%', "")),
                mount_point: parts[5..].join(" "),
            })
        })
        .collect()
}

/// `ps -eo pid,user,pcpu,pmem,comm`, past its header. A row without a whole pid is left
/// out, since the pid is what a kill names.
pub fn parse_ps(text: &str) -> Vec<Process> {
    text.trim()
        .split('\n')
        .skip(1)
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 {
                return None;
            }
            Some(Process {
                pid: parts[0].parse().ok()?,
                user: parts[1].to_string(),
                cpu_percent: number(parts[2]),
                mem_percent: number(parts[3]),
                command: parts[4..].join(" "),
            })
        })
        .collect()
}

/// `zone:value` lines; a value over 1000 is in millidegrees.
pub fn parse_thermal(text: &str) -> Vec<Thermal> {
    text.split('\n')
        .filter_map(|line| {
            let (id, raw) = line.trim().rsplit_once(':')?;
            let raw = js_number(raw)?;
            Some(Thermal {
                id: id.trim().to_string(),
                celsius: if raw > 1000.0 { raw / 1000.0 } else { raw },
            })
        })
        .collect()
}

/// `nvidia-smi --query-gpu=index,name,temperature.gpu,utilization.gpu,memory.used,memory.total
/// --format=csv,noheader,nounits`.
pub fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    text.split('\n')
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
            if parts.len() < 6 {
                return None;
            }
            Some(Gpu {
                index: number(parts[0]) as u32,
                name: parts[1].to_string(),
                temperature_c: number(parts[2]),
                utilization_percent: number(parts[3]),
                memory_used_mb: number(parts[4]),
                memory_total_mb: number(parts[5]),
            })
        })
        .collect()
}

/// What the next sample's rates are measured against.
#[derive(Clone, Debug, Default)]
pub struct DeltaState {
    proc_stat: Vec<ProcStatSample>,
    net_dev: Vec<NetDevSample>,
    /// When the sample arrived, in Unix milliseconds.
    collected_at: u64,
    uptime_seconds: f64,
}

pub struct Sample {
    pub metrics: Metrics,
    pub state: DeltaState,
    /// False when the host has no `/proc/stat` (it is not Linux), so the script cannot
    /// measure it.
    pub supported: bool,
}

/// One script sample as a metrics frame (the Electron app's `buildSnapshot`). A slow
/// section the sample leaves out is `None`, so [`crate::protocol::Snapshot::next`] keeps
/// its last value, as it does for the agent's frames.
pub fn build_metrics(raw: &str, prev: Option<&DeltaState>, collected_at: u64) -> Sample {
    let sections = split_sections(raw);
    let section = |name: &str| sections.get(name).map(String::as_str).unwrap_or("");
    let proc_stat = parse_proc_stat(section("stat"));
    let net_dev = parse_net_dev(section("netdev"));
    let uptime_seconds = parse_uptime(section("uptime"));

    // The host's own uptime clock is immune to SSH delivery jitter; the arrival time is
    // the fallback when uptime is missing or did not advance.
    let interval = match prev {
        Some(prev) if uptime_seconds > prev.uptime_seconds && prev.uptime_seconds > 0.0 => {
            uptime_seconds - prev.uptime_seconds
        }
        Some(prev) if prev.collected_at > 0 => {
            ((collected_at as f64 - prev.collected_at as f64) / 1000.0).max(0.001)
        }
        _ => 1.0,
    };

    let before =
        |name: &str| prev.and_then(|prev| prev.proc_stat.iter().find(|sample| sample.name == name));
    let usage = |sample: &ProcStatSample| {
        before(&sample.name).map_or(0.0, |before| cpu_usage_percent(before, sample))
    };
    let cpu_cores = proc_stat
        .iter()
        .filter(|sample| sample.name.starts_with("cpu") && sample.name != "cpu")
        .map(|sample| CpuCore {
            name: sample.name.clone(),
            usage_percent: usage(sample),
        })
        .collect();
    let cpu_total_percent = proc_stat
        .iter()
        .find(|sample| sample.name == "cpu")
        .map_or(0.0, usage);

    let network = match prev {
        Some(prev) if !prev.net_dev.is_empty() => net_rates(&prev.net_dev, &net_dev, interval),
        _ => net_rates(&[], &net_dev, 1.0),
    };

    let metrics = Metrics {
        ts_unix_ms: collected_at,
        mono_ns: 0,
        cpu_total_percent,
        cpu_cores,
        load: parse_loadavg(section("loadavg")).to_vec(),
        uptime_seconds,
        memory: parse_meminfo(section("meminfo")),
        network,
        disk_io: Vec::new(),
        pressure: None,
        disks: sections.get("df").map(|text| parse_df(text)),
        processes: sections.get("ps").map(|text| parse_ps(text)),
        thermals: sections.get("thermal").map(|text| parse_thermal(text)),
        gpus: sections.get("nvidia").map(|text| parse_nvidia_smi(text)),
    };
    let supported = !proc_stat.is_empty();
    Sample {
        metrics,
        state: DeltaState {
            proc_stat,
            net_dev,
            collected_at,
            uptime_seconds,
        },
        supported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Snapshot;

    const SAMPLE_STAT: &str = "cpu  4705 0 2921 145678 120 0 89 0 0 0
cpu0 1200 0 800 36000 30 0 20 0 0 0
cpu1 1100 0 700 36100 25 0 15 0 0 0";

    const SAMPLE_STAT_2: &str = "cpu  4805 0 2951 146678 120 0 89 0 0 0
cpu0 1250 0 820 36500 30 0 20 0 0 0
cpu1 1150 0 720 36600 25 0 15 0 0 0";

    const SAMPLE_MEM: &str = "MemTotal:       16384000 kB
MemAvailable:    8192000 kB
SwapTotal:       2097152 kB
SwapFree:        1048576 kB";

    const SAMPLE_NET: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000       10    0    0    0     0          0         0     1000       10    0    0    0     0       0          0
  eth0: 5000000  40000    0    0    0     0          0         0  2500000  20000    0    0    0     0       0          0";

    const SAMPLE_NET_2: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000       10    0    0    0     0          0         0     1000       10    0    0    0     0       0          0
  eth0: 5005000  40100    0    0    0     0          0         0  2503000  20100    0    0    0     0       0          0";

    const SAMPLE_LOAD: &str = "1.25 0.98 0.75 2/400 12345";

    const SAMPLE_UPTIME: &str = "123456.78 987654.32";

    const SAMPLE_DF: &str = "Filesystem     1024-blocks      Used Available Use% Mounted on
/dev/sda1       50331648  25165824  22649216  53% /
tmpfs            8192000         0   8192000   0% /dev/shm";

    const SAMPLE_PS: &str = "  PID USER     %CPU %MEM COMMAND
    1 root      0.1  0.2 systemd
 1234 www-data 12.5  4.0 nginx: worker process";

    const SAMPLE_THERMAL: &str = "thermal_zone0:45200
thermal_zone1:38";

    const SAMPLE_NVIDIA: &str = "0, NVIDIA GeForce RTX 3080, 65, 42, 4096, 10240";

    fn bundle() -> String {
        [
            ("stat", SAMPLE_STAT),
            ("meminfo", SAMPLE_MEM),
            ("netdev", SAMPLE_NET),
            ("loadavg", SAMPLE_LOAD),
            ("uptime", SAMPLE_UPTIME),
            ("df", SAMPLE_DF),
            ("ps", SAMPLE_PS),
            ("thermal", SAMPLE_THERMAL),
            ("nvidia", SAMPLE_NVIDIA),
        ]
        .iter()
        .map(|(name, text)| format!("{SECTION_MARKER} {name}\n{text}"))
        .collect::<Vec<_>>()
        .join("\n")
    }

    #[test]
    fn sections_split_on_the_marker() {
        let sections = split_sections(&bundle());
        assert!(sections["stat"].contains("cpu  4705"));
        assert!(sections["meminfo"].contains("MemTotal"));
        assert!(sections["nvidia"].contains("RTX 3080"));
        assert_eq!(sections["loadavg"], SAMPLE_LOAD);
        assert!(!sections.contains_key("end"));
    }

    #[test]
    fn proc_stat_lines_and_usage() {
        let first = parse_proc_stat(SAMPLE_STAT);
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].name, "cpu");
        assert_eq!(first[1].user, 1200.0);
        let second = parse_proc_stat(SAMPLE_STAT_2);
        let total = cpu_usage_percent(&first[0], &second[0]);
        assert!(total > 0.0 && total < 100.0, "{total}");
        // 130 busy ticks of 1130: the guest columns are not counted again.
        assert!((total - 130.0 / 1130.0 * 100.0).abs() < 1e-9);
        // A counter that did not move reads 0, not NaN.
        assert_eq!(cpu_usage_percent(&first[0], &first[0]), 0.0);
    }

    #[test]
    fn meminfo_derives_used_memory() {
        let memory = parse_meminfo(SAMPLE_MEM);
        assert_eq!(memory.total_kb, 16_384_000.0);
        assert_eq!(memory.used_kb, 8_192_000.0);
        assert_eq!(memory.swap_used_kb, 1_048_576.0);
        // Old kernels have no MemAvailable: MemFree stands in.
        let old = parse_meminfo("MemTotal: 1000 kB\nMemFree: 250 kB\nActive(anon): 9 kB");
        assert_eq!(old.available_kb, 250.0);
        assert_eq!(old.used_kb, 750.0);
    }

    #[test]
    fn net_dev_skips_loopback_and_rates_per_second() {
        let first = parse_net_dev(SAMPLE_NET);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "eth0");
        let second = parse_net_dev(SAMPLE_NET_2);
        let rates = net_rates(&first, &second, 1.0);
        assert_eq!(rates[0].rx_bytes_per_sec, 5000.0);
        assert_eq!(rates[0].tx_bytes_per_sec, 3000.0);
        // A reset counter reads 0, and no interval reads nothing.
        assert_eq!(net_rates(&second, &first, 1.0)[0].rx_bytes_per_sec, 0.0);
        assert!(net_rates(&first, &second, 0.0).is_empty());
    }

    #[test]
    fn small_parsers() {
        assert_eq!(parse_loadavg(SAMPLE_LOAD), [1.25, 0.98, 0.75]);
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
        assert_eq!(parse_uptime(SAMPLE_UPTIME), 123_456.78);
        assert_eq!(parse_uptime(""), 0.0);

        let disks = parse_df(SAMPLE_DF);
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].mount_point, "/");
        assert_eq!(disks[0].use_percent, 53.0);
        assert_eq!(disks[0].size_kb, 50_331_648.0);
        let spaced = parse_df(
            "Filesystem 1024-blocks Used Available Use% Mounted on\n/dev/sdb1 100 40 60 40% /mnt/My Disk\nshort line",
        );
        assert_eq!(spaced.len(), 1);
        assert_eq!(spaced[0].mount_point, "/mnt/My Disk");
        assert!(parse_df("Filesystem 1024-blocks Used Available Use% Mounted on").is_empty());

        let processes = parse_ps(SAMPLE_PS);
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[1].pid, 1234);
        assert_eq!(processes[1].user, "www-data");
        assert_eq!(processes[1].command, "nginx: worker process");
        assert_eq!(processes[1].cpu_percent, 12.5);

        let thermals = parse_thermal(SAMPLE_THERMAL);
        assert!((thermals[0].celsius - 45.2).abs() < 0.05);
        assert_eq!(thermals[0].id, "thermal_zone0");
        assert_eq!(thermals[1].celsius, 38.0);
        assert!(parse_thermal("zone:hot\nnothing").is_empty());

        let gpus = parse_nvidia_smi(SAMPLE_NVIDIA);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 3080");
        assert_eq!(gpus[0].memory_total_mb, 10_240.0);
        assert_eq!(gpus[0].temperature_c, 65.0);
    }

    #[test]
    fn samples_build_metrics_with_deltas() {
        let first = build_metrics(&bundle(), None, 1000);
        assert!(first.supported);
        assert_eq!(first.metrics.memory.total_kb, 16_384_000.0);
        assert_eq!(first.metrics.gpus.as_ref().map(Vec::len), Some(1));
        assert_eq!(first.metrics.cpu_total_percent, 0.0);
        assert_eq!(first.metrics.cpu_cores.len(), 2);
        assert_eq!(first.metrics.load, vec![1.25, 0.98, 0.75]);
        assert_eq!(first.metrics.network[0].rx_bytes_per_sec, 0.0);

        let later = bundle()
            .replace(SAMPLE_STAT, SAMPLE_STAT_2)
            .replace(SAMPLE_NET, SAMPLE_NET_2);
        let second = build_metrics(&later, Some(&first.state), 2000);
        assert!(second.metrics.cpu_total_percent > 0.0);
        assert!(second.metrics.cpu_cores[0].usage_percent > 0.0);
        assert_eq!(second.metrics.network[0].rx_bytes_per_sec, 5000.0);
        assert_eq!(second.metrics.ts_unix_ms, 2000);
    }

    #[test]
    fn slow_sections_a_sample_omits_carry_over() {
        let first = build_metrics(&bundle(), None, 1000);
        let snapshot = Snapshot::default().next(first.metrics);
        let fast_only = format!(
            "{SECTION_MARKER} stat\n{SAMPLE_STAT_2}\n{SECTION_MARKER} uptime\n{SAMPLE_UPTIME}\n"
        );
        let second = build_metrics(&fast_only, Some(&first.state), 2000);
        assert_eq!(second.metrics.disks, None);
        let next = snapshot.next(second.metrics);
        assert_eq!(next.gpus, snapshot.gpus);
        assert_eq!(next.processes, snapshot.processes);
        assert_eq!(next.disks, snapshot.disks);
        assert_eq!(next.thermals, snapshot.thermals);
        assert_eq!(next.disks.len(), 2);
        assert!(next.disk_io.is_empty() && next.pressure.is_none());
    }

    #[test]
    fn the_host_uptime_is_the_rate_clock() {
        let first = build_metrics(&bundle(), None, 1000);
        let later = bundle()
            .replace(SAMPLE_NET, SAMPLE_NET_2)
            .replace("123456.78", "123458.78");
        // It arrived 500ms later over SSH, but the host says 2s passed.
        let second = build_metrics(&later, Some(&first.state), 1500);
        assert!((second.metrics.network[0].rx_bytes_per_sec - 2500.0).abs() < 1e-6);
        // Without uptime the arrival time measures it.
        let no_uptime = |net: &str| format!("{SECTION_MARKER} netdev\n{net}\n");
        let first = build_metrics(&no_uptime(SAMPLE_NET), None, 1000);
        let second = build_metrics(&no_uptime(SAMPLE_NET_2), Some(&first.state), 1500);
        assert!((second.metrics.network[0].rx_bytes_per_sec - 10_000.0).abs() < 1e-6);
    }

    #[test]
    fn hosts_without_proc_stat_are_flagged() {
        assert!(!build_metrics(&format!("{SECTION_MARKER} stat\n"), None, 1000).supported);
        assert!(build_metrics(&bundle(), None, 1000).supported);
    }

    #[test]
    fn frames_end_on_the_end_line_and_keep_a_partial_one() {
        let mut buffer = FrameBuffer::default();
        let end = format!("{SECTION_MARKER} end\n");
        assert!(
            buffer
                .push(format!("{SECTION_MARKER} stat\ncpu 1").as_bytes())
                .is_empty()
        );
        assert_eq!(
            buffer.push(
                format!(" 2\n{end}{SECTION_MARKER} stat\ncpu 3\n{end}{SECTION_MARKER} st")
                    .as_bytes()
            ),
            vec![
                format!("{SECTION_MARKER} stat\ncpu 1 2\n"),
                format!("{SECTION_MARKER} stat\ncpu 3\n"),
            ]
        );
        assert_eq!(
            buffer.push(format!("at\n{end}").as_bytes()),
            vec![format!("{SECTION_MARKER} stat\n")]
        );
        // A character split across chunks comes out whole.
        let text = format!("{SECTION_MARKER} ps\n1 root 0 0 café\n{end}");
        let (head, tail) = text.as_bytes().split_at(text.find('é').unwrap() + 1);
        assert!(buffer.push(head).is_empty());
        assert!(buffer.push(tail)[0].contains("café"));
        buffer.push(b"partial");
        buffer.reset();
        assert_eq!(buffer.push(end.as_bytes()), vec![String::new()]);
    }
}
