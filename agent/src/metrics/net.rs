use std::collections::HashMap;

use super::{delta_counter, parse_u64};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetSample {
    pub name: String,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetRate {
    pub name: String,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

pub fn parse_net_dev(text: &str) -> Vec<NetSample> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Inter-") || line.starts_with("face") {
            continue;
        }
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 9 {
            continue;
        }
        out.push(NetSample {
            name: name.trim().to_string(),
            rx: parse_u64(fields[0]),
            tx: parse_u64(fields[8]),
        });
    }
    out
}

/// Names that are never a physical NIC. tailscale0 and other tun devices are not matched
/// here; the host drops them when `/sys/class/net/<if>/device` is absent.
pub fn is_virtual_iface_name(name: &str) -> bool {
    name == "lo"
        || name.starts_with("veth")
        || name.starts_with("docker")
        || name.starts_with("br-")
        || name.starts_with("virbr")
}

/// A physical interface: not a known virtual name, and its sysfs device link exists.
pub fn keep_iface(name: &str, has_device: bool) -> bool {
    !name.is_empty() && !is_virtual_iface_name(name) && has_device
}

/// Bytes per second between two samples. A counter that moved back was reset; a name not
/// seen before, or no elapsed time, gives 0.
pub fn net_rates(prev: &[NetSample], curr: &[NetSample], elapsed: f64) -> Vec<NetRate> {
    let prev_by: HashMap<&str, &NetSample> = prev
        .iter()
        .map(|sample| (sample.name.as_str(), sample))
        .collect();
    curr.iter()
        .map(|sample| {
            let mut rate = NetRate {
                name: sample.name.clone(),
                ..NetRate::default()
            };
            if let Some(prev) = prev_by.get(sample.name.as_str())
                && elapsed > 0.0
            {
                rate.rx_bytes_per_sec = delta_counter(prev.rx, sample.rx) as f64 / elapsed;
                rate.tx_bytes_per_sec = delta_counter(prev.tx, sample.tx) as f64 / elapsed;
            }
            rate
        })
        .collect()
}
