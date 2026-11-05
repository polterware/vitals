use super::{parse_f64_or_0, parse_u64};
use crate::protocol::Gpu;

pub fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut fields = text.split_whitespace();
    let mut load = [0.0; 3];
    for slot in &mut load {
        *slot = fields.next()?.parse().ok()?;
    }
    Some(load)
}

pub fn parse_uptime(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// A sysfs temperature in millidegrees as degrees Celsius.
pub fn milli_celsius(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().map(|value| value / 1000.0)
}

/// `nvidia-smi --query-gpu=index,name,temperature.gpu,utilization.gpu,memory.used,
/// memory.total --format=csv,noheader,nounits`. A reading the GPU does not report
/// (`[N/A]`) is 0.
pub fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 6 {
            continue;
        }
        out.push(Gpu {
            index: parse_u64(parts[0]) as i64,
            name: parts[1].to_string(),
            temperature_c: parse_f64_or_0(parts[2]),
            utilization_percent: parse_f64_or_0(parts[3]),
            memory_used_mb: parse_f64_or_0(parts[4]),
            memory_total_mb: parse_f64_or_0(parts[5]),
        });
    }
    out
}
