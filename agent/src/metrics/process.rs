use std::collections::HashMap;

use super::delta_counter;

/// The fields of `/proc/<pid>/stat` the process table uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PidStat {
    pub pid: i64,
    pub command: String,
    pub utime: u64,
    pub stime: u64,
    pub rss_pages: u64,
}

impl PidStat {
    pub fn ticks(&self) -> u64 {
        self.utime + self.stime
    }
}

/// A process as every host reads it. CPU time is cumulative nanoseconds, so Linux ticks,
/// Mach time, and Windows 100ns units each convert once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessSample {
    pub pid: i64,
    pub user: String,
    pub command: String,
    pub cpu_ns: u64,
    pub rss_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRow {
    pub pid: i64,
    pub user: String,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub command: String,
}

/// The command sits in parentheses and may hold spaces and parentheses itself, so it runs
/// from the first `(` to the last `)`.
pub fn parse_pid_stat(text: &str) -> Result<PidStat, String> {
    let text = text.trim();
    let (Some(open), Some(close)) = (text.find('('), text.rfind(')')) else {
        return Err("bad stat".into());
    };
    if open == 0 || close < open {
        return Err("bad stat".into());
    }
    let pid = text[..open]
        .trim()
        .parse()
        .map_err(|_| "bad stat pid".to_string())?;
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    if rest.len() < 22 {
        return Err("short stat".into());
    }
    let counter = |index: usize| rest[index].parse::<u64>();
    match (counter(11), counter(12), counter(21)) {
        (Ok(utime), Ok(stime), Ok(rss_pages)) => Ok(PidStat {
            pid,
            command: text[open + 1..close].to_string(),
            utime,
            stime,
            rss_pages,
        }),
        _ => Err("bad stat counters".into()),
    }
}

/// The real uid on the `Uid:` line of `/proc/<pid>/status`.
pub fn parse_status_uid(text: &str) -> Result<u32, String> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|uid| uid.parse().ok())
                .ok_or_else(|| "bad uid".to_string());
        }
    }
    Err("uid missing".into())
}

/// A CPU-time delta as a percentage of one core. No previous time means the pid was just
/// seen; a decrease is a reset. Multithreaded tasks may go over 100.
pub fn process_cpu_percent(prev: Option<u64>, curr_ns: u64, elapsed: f64) -> f64 {
    match prev {
        Some(prev) if elapsed > 0.0 => delta_counter(prev, curr_ns) as f64 / 1e9 / elapsed * 100.0,
        _ => 0.0,
    }
}

pub fn mem_percent(rss_bytes: u64, total_bytes: u64) -> f64 {
    if total_bytes == 0 {
        0.0
    } else {
        rss_bytes as f64 / total_bytes as f64 * 100.0
    }
}

/// The top `limit` processes by CPU (then by pid), and the next CPU-time baseline for every
/// pid sampled, not only the ones sent.
pub fn rank_processes(
    samples: Vec<ProcessSample>,
    prev: &HashMap<i64, u64>,
    elapsed: f64,
    mem_total_bytes: u64,
    limit: usize,
) -> (Vec<ProcessRow>, HashMap<i64, u64>) {
    let mut next = HashMap::with_capacity(samples.len());
    let mut rows = Vec::with_capacity(samples.len());
    for sample in samples {
        next.insert(sample.pid, sample.cpu_ns);
        let command = if sample.command.is_empty() {
            "?".to_string()
        } else {
            sample.command
        };
        rows.push(ProcessRow {
            pid: sample.pid,
            user: sample.user,
            cpu_percent: process_cpu_percent(
                prev.get(&sample.pid).copied(),
                sample.cpu_ns,
                elapsed,
            ),
            mem_percent: mem_percent(sample.rss_bytes, mem_total_bytes),
            command,
        });
    }
    rows.sort_by(|a, b| {
        b.cpu_percent
            .total_cmp(&a.cpu_percent)
            .then_with(|| a.pid.cmp(&b.pid))
    });
    rows.truncate(limit);
    (rows, next)
}
