//! What the hosts read turned into numbers: the `/proc` parsers, and the rates between two
//! samples that every host shares (CPU, network, disk I/O, processes). Pure functions over
//! text and samples, so the fixtures under `testdata/` cover them on any machine.

mod cpu;
mod disk;
mod mem;
mod misc;
mod net;
mod pressure;
mod process;

pub use cpu::{CoreUsage, CpuSample, cpu_count, cpu_usage, parse_proc_stat};
pub use disk::{
    DiskRate, DiskSample, DiskUsage, Mount, SECTOR_SIZE, disk_rates, is_pseudo_fs,
    looks_like_whole_disk, parse_diskstats, parse_mounts, skip_disk_name, usage,
};
pub use mem::parse_meminfo;
pub use misc::{milli_celsius, parse_loadavg, parse_nvidia_smi, parse_uptime};
pub use net::{NetRate, NetSample, is_virtual_iface_name, keep_iface, net_rates, parse_net_dev};
pub use pressure::parse_psi_avg10;
pub use process::{
    PidStat, ProcessRow, ProcessSample, mem_percent, parse_pid_stat, parse_status_uid,
    process_cpu_percent, rank_processes,
};

/// The increment of a monotonic kernel counter. When `curr < prev` the counter was reset
/// (a driver reload, a namespace reboot, a reused pid), and the increment is `curr` itself:
/// what was counted since the reset to zero.
pub fn delta_counter(prev: u64, curr: u64) -> u64 {
    if curr >= prev { curr - prev } else { curr }
}

fn parse_u64(text: &str) -> u64 {
    text.parse().unwrap_or(0)
}

fn parse_f64_or_0(text: &str) -> f64 {
    text.parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests;
