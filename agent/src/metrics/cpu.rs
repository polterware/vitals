use std::collections::HashMap;

use super::parse_u64;

/// One `cpu` line of `/proc/stat` (macOS and Windows fill the same shape). Guest and
/// guest_nice are read but not added to busy time: Linux already counts them in user and
/// nice (proc(5)).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuSample {
    pub name: String,
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuSample {
    fn busy(&self) -> u64 {
        self.user + self.nice + self.system + self.irq + self.softirq + self.steal
    }

    fn total(&self) -> u64 {
        self.busy() + self.idle + self.iowait
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoreUsage {
    pub name: String,
    pub usage_percent: f64,
}

pub fn parse_proc_stat(text: &str) -> Vec<CpuSample> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 || !fields[0].starts_with("cpu") {
            continue;
        }
        let num = |index: usize| fields.get(index + 1).map_or(0, |field| parse_u64(field));
        out.push(CpuSample {
            name: fields[0].to_string(),
            user: num(0),
            nice: num(1),
            system: num(2),
            idle: num(3),
            iowait: num(4),
            irq: num(5),
            softirq: num(6),
            steal: num(7),
            guest: num(8),
            guest_nice: num(9),
        });
    }
    out
}

/// The total and per-core usage between two samples. A missing previous line or a counter
/// that went backwards gives 0: a partial rewind is not a trustworthy zero point.
pub fn cpu_usage(prev: &[CpuSample], curr: &[CpuSample]) -> (f64, Vec<CoreUsage>) {
    let prev_by: HashMap<&str, &CpuSample> = prev
        .iter()
        .map(|sample| (sample.name.as_str(), sample))
        .collect();
    let mut total = 0.0;
    let mut cores = Vec::with_capacity(curr.len());
    for sample in curr {
        let percent = prev_by
            .get(sample.name.as_str())
            .map_or(0.0, |prev| percent(prev, sample));
        if sample.name == "cpu" {
            total = percent;
        } else {
            cores.push(CoreUsage {
                name: sample.name.clone(),
                usage_percent: percent,
            });
        }
    }
    (total, cores)
}

fn percent(prev: &CpuSample, curr: &CpuSample) -> f64 {
    let (pt, ct) = (prev.total(), curr.total());
    let (pb, cb) = (prev.busy(), curr.busy());
    if ct < pt || cb < pb || ct == pt {
        return 0.0;
    }
    (cb - pb) as f64 / (ct - pt) as f64 * 100.0
}

pub fn cpu_count(samples: &[CpuSample]) -> usize {
    samples
        .iter()
        .filter(|sample| sample.name != "cpu" && sample.name.starts_with("cpu"))
        .count()
}
