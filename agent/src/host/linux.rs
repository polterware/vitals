//! Linux: the stable `/proc` files are opened once and read again each sample; per-pid files
//! are opened per sample, since a reused pid would keep a dead inode.

use std::collections::HashMap;
use std::ffi::CString;
use std::time::Instant;

use super::procfile::{ProcFile, read_into};
use crate::gpu;
use crate::metrics::{
    ProcessSample, cpu_count, keep_iface, looks_like_whole_disk, milli_celsius, parse_diskstats,
    parse_loadavg, parse_meminfo, parse_mounts, parse_net_dev, parse_pid_stat, parse_proc_stat,
    parse_psi_avg10, parse_status_uid, parse_uptime, skip_disk_name, usage,
};
use crate::protocol::{Disk, Gpu, Hello, Pressure, Thermal};
use crate::serve::{Collector, FastRaw};

pub struct Host {
    start: Instant,
    page: u64,
    tick_ns: u64,
    gpu: bool,
    passwd: HashMap<u32, String>,
    physical: HashMap<String, bool>,
    sys_block: Option<bool>,
    psi_checked: bool,
    stat: Option<ProcFile>,
    meminfo: Option<ProcFile>,
    netdev: Option<ProcFile>,
    loadavg: Option<ProcFile>,
    uptime: Option<ProcFile>,
    diskstats: Option<ProcFile>,
    mounts: Option<ProcFile>,
    psi: [Option<ProcFile>; 3],
    pid_buf: Vec<u8>,
}

impl Host {
    pub fn new() -> Self {
        // SAFETY: sysconf has no preconditions; a non-positive answer means unknown.
        let (page, ticks) = unsafe {
            (
                libc::sysconf(libc::_SC_PAGESIZE),
                libc::sysconf(libc::_SC_CLK_TCK),
            )
        };
        let ticks = if ticks > 0 { ticks as u64 } else { 100 };
        Self {
            start: Instant::now(),
            page: if page > 0 { page as u64 } else { 4096 },
            tick_ns: 1_000_000_000 / ticks,
            gpu: gpu::has_nvidia_smi(),
            passwd: load_passwd(),
            physical: HashMap::new(),
            sys_block: None,
            psi_checked: false,
            stat: None,
            meminfo: None,
            netdev: None,
            loadavg: None,
            uptime: None,
            diskstats: None,
            mounts: None,
            psi: [None, None, None],
            pid_buf: vec![0; 64 * 1024],
        }
    }

    fn keep_iface(&mut self, name: &str) -> bool {
        if let Some(keep) = self.physical.get(name) {
            return *keep;
        }
        let device = std::fs::symlink_metadata(format!("/sys/class/net/{name}/device")).is_ok();
        let keep = keep_iface(name, device);
        self.physical.insert(name.to_string(), keep);
        keep
    }

    fn keep_disk(&mut self, name: &str) -> bool {
        if skip_disk_name(name) {
            return false;
        }
        let sys_block = *self
            .sys_block
            .get_or_insert_with(|| std::path::Path::new("/sys/block").is_dir());
        if !sys_block {
            return looks_like_whole_disk(name);
        }
        std::path::Path::new("/sys/block").join(name).is_dir()
    }

    fn read_psi(&mut self) -> Result<Option<Pressure>, String> {
        if !self.psi_checked {
            self.psi_checked = true;
            for (slot, path) in self.psi.iter_mut().zip([
                "/proc/pressure/cpu",
                "/proc/pressure/memory",
                "/proc/pressure/io",
            ]) {
                *slot = ProcFile::open(path).ok();
            }
        }
        if self.psi.iter().all(Option::is_none) {
            return Ok(None);
        }
        let mut values = [0.0; 3];
        let mut first_error = None;
        for (value, file) in values.iter_mut().zip(self.psi.iter_mut()) {
            let Some(file) = file else { continue };
            match file.text() {
                Ok(text) => *value = parse_psi_avg10(&text).unwrap_or(0.0),
                Err(error) => {
                    first_error.get_or_insert(error.to_string());
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(Some(Pressure {
                cpu: values[0],
                memory: values[1],
                io: values[2],
            })),
        }
    }
}

/// Reads a kept file, opening it the first time and again after a failed read.
fn text(slot: &mut Option<ProcFile>, path: &str) -> Result<String, String> {
    if slot.is_none() {
        *slot = Some(ProcFile::open(path).map_err(|error| error.to_string())?);
    }
    let file = slot.as_mut().expect("opened above");
    match file.text() {
        Ok(text) => Ok(text),
        Err(error) => {
            *slot = None;
            Err(error.to_string())
        }
    }
}

impl Collector for Host {
    fn hello(&mut self) -> Hello {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|text| text.trim().to_string())
            .ok()
            .filter(|kernel| !kernel.is_empty())
            .unwrap_or_else(|| "unknown".into());
        let mut cpus = std::thread::available_parallelism().map_or(1, |count| count.get());
        if let Ok(stat) = text(&mut self.stat, "/proc/stat") {
            let counted = cpu_count(&parse_proc_stat(&stat));
            if counted > 0 {
                cpus = counted;
            }
        }
        Hello::new(super::hostname(), kernel, cpus as u32)
    }

    fn gpu_enabled(&self) -> bool {
        self.gpu
    }

    fn sample_fast(&mut self) -> FastRaw {
        let mut raw = FastRaw::new(self.start);
        raw.cpus = text(&mut self.stat, "/proc/stat").map(|text| parse_proc_stat(&text));
        raw.memory = text(&mut self.meminfo, "/proc/meminfo").map(|text| parse_meminfo(&text));
        raw.nets = text(&mut self.netdev, "/proc/net/dev").map(|text| {
            parse_net_dev(&text)
                .into_iter()
                .filter(|sample| self.keep_iface(&sample.name))
                .collect()
        });
        raw.load = text(&mut self.loadavg, "/proc/loadavg")
            .and_then(|text| parse_loadavg(&text).ok_or_else(|| "bad loadavg".to_string()));
        raw.uptime = text(&mut self.uptime, "/proc/uptime")
            .and_then(|text| parse_uptime(&text).ok_or_else(|| "bad uptime".to_string()));
        raw.disks = text(&mut self.diskstats, "/proc/diskstats").map(|text| {
            parse_diskstats(&text)
                .into_iter()
                .filter(|sample| self.keep_disk(&sample.device))
                .collect()
        });
        raw.pressure = self.read_psi();
        raw
    }

    fn sample_procs(&mut self, _mem_total_kb: u64) -> Result<Vec<ProcessSample>, String> {
        let entries = std::fs::read_dir("/proc").map_err(|error| error.to_string())?;
        let mut out = Vec::with_capacity(256);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|name| name.parse::<i64>().ok()) else {
                continue;
            };
            if pid <= 0 {
                continue;
            }
            let buf = &mut self.pid_buf;
            let read = |file: &str, buf: &mut Vec<u8>| -> Option<String> {
                let read = read_into(&format!("/proc/{pid}/{file}"), buf).ok()?;
                (read > 0 && read < buf.len())
                    .then(|| String::from_utf8_lossy(&buf[..read]).into_owned())
            };
            let Some(stat) = read("stat", buf).and_then(|text| parse_pid_stat(&text).ok()) else {
                continue;
            };
            let Some(uid) = read("status", buf).and_then(|text| parse_status_uid(&text).ok())
            else {
                continue;
            };
            let user = self
                .passwd
                .get(&uid)
                .cloned()
                .unwrap_or_else(|| uid.to_string());
            out.push(ProcessSample {
                pid: stat.pid,
                user,
                cpu_ns: stat.ticks().saturating_mul(self.tick_ns),
                rss_bytes: stat.rss_pages.saturating_mul(self.page),
                command: stat.command,
            });
        }
        Ok(out)
    }

    fn sample_disks(&mut self) -> Result<Vec<Disk>, String> {
        let mounts = parse_mounts(&text(&mut self.mounts, "/proc/mounts")?);
        let mut out = Vec::with_capacity(mounts.len());
        for mount in &mounts {
            let Some((blocks, bfree, bavail, size)) = statvfs(&mount.mount_point) else {
                continue;
            };
            let used = usage(blocks, bfree, bavail, size);
            out.push(Disk {
                filesystem: mount.device.clone(),
                size_kb: used.size_kb,
                used_kb: used.used_kb,
                avail_kb: used.avail_kb,
                use_percent: used.use_percent,
                mount_point: mount.mount_point.clone(),
            });
        }
        if !mounts.is_empty() && out.is_empty() {
            return Err("statfs failed".into());
        }
        Ok(out)
    }

    fn sample_thermals(&mut self) -> Result<Vec<Thermal>, String> {
        let Ok(entries) = std::fs::read_dir("/sys/class/thermal") else {
            return Ok(Vec::new());
        };
        let mut zones: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with("thermal_zone"))
            .collect();
        zones.sort();
        Ok(zones
            .into_iter()
            .filter_map(|zone| {
                let raw =
                    std::fs::read_to_string(format!("/sys/class/thermal/{zone}/temp")).ok()?;
                Some(Thermal {
                    celsius: milli_celsius(&raw)?,
                    id: zone,
                })
            })
            .collect())
    }

    fn sample_gpus(&mut self) -> Result<Vec<Gpu>, String> {
        gpu::sample_nvidia_smi()
    }
}

/// `(blocks, bfree, bavail, fragment size)` of the filesystem at `path`, as df reads it.
fn statvfs(path: &str) -> Option<(u64, u64, u64, u64)> {
    let path = CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is writable.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let size = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    if size == 0 {
        return None;
    }
    Some((
        stat.f_blocks as u64,
        stat.f_bfree as u64,
        stat.f_bavail as u64,
        size,
    ))
}

fn owner(pid: i64) -> Result<u32, String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|_| "process not found".to_string())?;
    parse_status_uid(&status)
}

pub fn kill(pid: i64, signal: &str) -> Result<(), String> {
    super::signal(pid, signal, owner(pid))
}

fn load_passwd() -> HashMap<u32, String> {
    let mut users = HashMap::new();
    let Ok(text) = std::fs::read_to_string("/etc/passwd") else {
        return users;
    };
    for line in text.lines() {
        let mut parts = line.split(':');
        let (Some(name), Some(_), Some(uid)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(uid) = uid.parse() {
            users.entry(uid).or_insert_with(|| name.to_string());
        }
    }
    users
}
