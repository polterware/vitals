//! How numbers read on the page.

use crate::settings::NetworkUnit;

/// A percent with `digits` decimals (`formatPercent`).
pub fn percent(value: f64, digits: usize) -> String {
    format!("{value:.digits$}%")
}

/// Kilobytes as the Electron app reads them (`formatKb`): whole KB under 1 MB, MB with one
/// decimal under 1 GB, GB with two above.
pub fn kilobytes(kb: f64) -> String {
    if kb < 1024.0 {
        format!("{} KB", number(kb))
    } else if kb < 1024.0 * 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{:.2} GB", kb / (1024.0 * 1024.0))
    }
}

/// A number the way JavaScript's `String` prints it: whole numbers with no decimals.
pub fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

/// Bytes per second (`formatBytesPerSec`): whole B/s under 1 KB, KB/s with one decimal under
/// 1 MB, MB/s with two above.
pub fn rate(bytes_per_second: f64) -> String {
    let value = bytes_per_second;
    if value < 1024.0 {
        format!("{value:.0} B/s")
    } else if value < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", value / 1024.0)
    } else {
        format!("{:.2} MB/s", value / (1024.0 * 1024.0))
    }
}

/// A network rate in the unit the settings pick (`formatNetworkRate`): bytes in 1024 steps,
/// or bits in 1000 steps.
pub fn network_rate(bytes_per_second: f64, unit: NetworkUnit) -> String {
    match unit {
        NetworkUnit::Bytes => rate(bytes_per_second),
        NetworkUnit::Bits => {
            let bits = bytes_per_second * 8.0;
            if bits < 1e3 {
                format!("{bits:.0} bit/s")
            } else if bits < 1e6 {
                format!("{:.1} Kbit/s", bits / 1e3)
            } else if bits < 1e9 {
                format!("{:.2} Mbit/s", bits / 1e6)
            } else {
                format!("{:.2} Gbit/s", bits / 1e9)
            }
        }
    }
}

/// A chart's unit word for `unit`.
pub fn rate_unit(unit: NetworkUnit) -> &'static str {
    match unit {
        NetworkUnit::Bytes => "B/s",
        NetworkUnit::Bits => "bit/s",
    }
}

pub fn uptime(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (days, hours, minutes) = (total / 86_400, total % 86_400 / 3600, total % 3600 / 60);
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(kilobytes(512.0), "512 KB");
        assert_eq!(kilobytes(0.0), "0 KB");
        assert_eq!(kilobytes(492_646.0), "481.1 MB");
        assert_eq!(kilobytes(4_000_000.0), "3.81 GB");
        assert_eq!(rate(0.0), "0 B/s");
        assert_eq!(rate(1536.0), "1.5 KB/s");
        assert_eq!(rate(3.0 * 1024.0 * 1024.0), "3.00 MB/s");
        assert_eq!(percent(12.345, 1), "12.3%");
        assert_eq!(percent(42.0, 0), "42%");
        assert_eq!(uptime(90_061.0), "1d 1h 1m");
        assert_eq!(network_rate(125.0, NetworkUnit::Bits), "1.0 Kbit/s");
        assert_eq!(network_rate(2_500_000.0, NetworkUnit::Bits), "20.00 Mbit/s");
        assert_eq!(network_rate(2048.0, NetworkUnit::Bytes), "2.0 KB/s");
        assert_eq!(uptime(3_700.0), "1h 1m");
        assert_eq!(uptime(59.0), "0m");
    }
}
