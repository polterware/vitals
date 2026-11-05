use std::collections::HashMap;

use super::parse_u64;
use crate::protocol::Memory;

/// `/proc/meminfo`: used is total minus available (MemFree on kernels without
/// MemAvailable).
pub fn parse_meminfo(text: &str) -> Memory {
    let mut values: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(first) = rest.split_whitespace().next() {
            values.insert(name, parse_u64(first));
        }
    }
    let get = |name: &str| values.get(name).copied().unwrap_or(0);
    let total = get("MemTotal");
    let available = match get("MemAvailable") {
        0 => get("MemFree"),
        available => available,
    };
    let swap_total = get("SwapTotal");
    let swap_free = get("SwapFree");
    Memory {
        total_kb: total,
        available_kb: available,
        used_kb: total.saturating_sub(available),
        swap_total_kb: swap_total,
        swap_free_kb: swap_free,
        swap_used_kb: swap_total.saturating_sub(swap_free),
    }
}
