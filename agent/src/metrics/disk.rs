use std::collections::HashMap;

use super::{delta_counter, parse_u64};

/// A `/proc/diskstats` sector is always 512 bytes, whatever the device's block size.
pub const SECTOR_SIZE: u64 = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskSample {
    pub device: String,
    pub read_sectors: u64,
    pub write_sectors: u64,
    pub io_time_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiskRate {
    pub device: String,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub util_percent: f64,
}

pub fn parse_diskstats(text: &str) -> Vec<DiskSample> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // major, minor, name, and at least the io-time field (index 12).
        if fields.len() < 13 {
            continue;
        }
        out.push(DiskSample {
            device: fields[2].to_string(),
            read_sectors: parse_u64(fields[5]),
            write_sectors: parse_u64(fields[9]),
            io_time_ms: parse_u64(fields[12]),
        });
    }
    out
}

/// Ramdisks, loop devices, and floppies, which diskstats lists too.
pub fn skip_disk_name(name: &str) -> bool {
    name.is_empty() || name.starts_with("loop") || name.starts_with("ram") || name.starts_with("fd")
}

/// The name rule when `/sys/block` is not there: partitions (sda1, nvme0n1p1, mmcblk0p1)
/// are left out so their rates are not counted twice.
pub fn looks_like_whole_disk(name: &str) -> bool {
    if skip_disk_name(name) {
        return false;
    }
    if name.starts_with("nvme") {
        return !name.contains('p');
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return !rest.contains('p');
    }
    !name.ends_with(|c: char| c.is_ascii_digit())
}

pub fn disk_rates(prev: &[DiskSample], curr: &[DiskSample], elapsed: f64) -> Vec<DiskRate> {
    let prev_by: HashMap<&str, &DiskSample> = prev
        .iter()
        .map(|sample| (sample.device.as_str(), sample))
        .collect();
    curr.iter()
        .map(|sample| {
            let mut rate = DiskRate {
                device: sample.device.clone(),
                ..DiskRate::default()
            };
            if let Some(prev) = prev_by.get(sample.device.as_str())
                && elapsed > 0.0
            {
                let read = delta_counter(prev.read_sectors, sample.read_sectors) * SECTOR_SIZE;
                let write = delta_counter(prev.write_sectors, sample.write_sectors) * SECTOR_SIZE;
                rate.read_bytes_per_sec = read as f64 / elapsed;
                rate.write_bytes_per_sec = write as f64 / elapsed;
                let io_ms = delta_counter(prev.io_time_ms, sample.io_time_ms);
                rate.util_percent = (io_ms as f64 / (elapsed * 1000.0) * 100.0).min(100.0);
            }
            rate
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mount_point: String,
    pub fs_type: String,
}

const PSEUDO_FS: [&str; 24] = [
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "cpuset",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "proc",
    "pstore",
    "ramfs",
    "rootfs",
    "rpc_pipefs",
    "securityfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

pub fn is_pseudo_fs(fs_type: &str) -> bool {
    PSEUDO_FS.contains(&fs_type)
}

/// `/proc/mounts` without the kernel's pseudo filesystems; `\040`-style escapes are read.
pub fn parse_mounts(text: &str) -> Vec<Mount> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || is_pseudo_fs(fields[2]) {
            continue;
        }
        out.push(Mount {
            device: unescape_mount(fields[0]),
            mount_point: unescape_mount(fields[1]),
            fs_type: fields[2].to_string(),
        });
    }
    out
}

fn unescape_mount(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let octal = |byte: u8| (b'0'..=b'7').contains(&byte);
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && octal(bytes[index + 1])
            && octal(bytes[index + 2])
            && octal(bytes[index + 3])
        {
            let value = ((bytes[index + 1] - b'0') << 6)
                | ((bytes[index + 2] - b'0') << 3)
                | (bytes[index + 3] - b'0');
            out.push(value);
            index += 4;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DiskUsage {
    pub size_kb: u64,
    pub used_kb: u64,
    pub avail_kb: u64,
    pub use_percent: f64,
}

/// Block counts as sizes; used is `blocks - bfree`, as df counts it.
pub fn usage(blocks: u64, bfree: u64, bavail: u64, bsize: u64) -> DiskUsage {
    if bsize == 0 {
        return DiskUsage::default();
    }
    let used = blocks.saturating_sub(bfree);
    let use_percent = if blocks > 0 {
        used as f64 / blocks as f64 * 100.0
    } else {
        0.0
    };
    let kb = |count: u64| (u128::from(count) * u128::from(bsize) / 1024) as u64;
    DiskUsage {
        size_kb: kb(blocks),
        used_kb: kb(used),
        avail_kb: kb(bavail),
        use_percent,
    }
}
