//! How numbers read on the page.

pub fn percent(value: f64) -> String {
    format!("{:.0}%", value.clamp(0.0, 100.0))
}

/// Bytes in the largest unit that keeps the number at least 1 (1024 steps).
pub fn bytes(value: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = value.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn kilobytes(value: f64) -> String {
    bytes(value * 1024.0)
}

pub fn rate(bytes_per_second: f64) -> String {
    format!("{}/s", bytes(bytes_per_second))
}

pub fn uptime(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (days, hours, minutes) = (total / 86_400, total % 86_400 / 3600, total % 3600 / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

pub fn load(load: [f64; 3]) -> String {
    format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KB");
        assert_eq!(kilobytes(16.0 * 1024.0 * 1024.0), "16.0 GB");
        assert_eq!(bytes(200.0 * 1024.0 * 1024.0), "200 MB");
        assert_eq!(rate(0.0), "0 B/s");
        assert_eq!(percent(142.0), "100%");
        assert_eq!(uptime(90_061.0), "1d 1h");
        assert_eq!(uptime(3_700.0), "1h 1m");
        assert_eq!(uptime(59.0), "0m");
        assert_eq!(load([0.5, 0.25, 1.0]), "0.50 0.25 1.00");
    }
}
