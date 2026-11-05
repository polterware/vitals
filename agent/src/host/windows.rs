//! Windows: processor times from `NtQuerySystemInformation`, memory from
//! `GlobalMemoryStatusEx`, network from `GetIfTable2`, disk I/O from each physical drive's
//! `IOCTL_DISK_PERFORMANCE`, volumes from the fixed drive letters, and processes from a
//! Toolhelp snapshot. Windows has no load average (zeros) and its thermal zones need WMI with
//! elevation, so none are reported. CPU and process times are 100 ns units.
#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use crate::gpu;
use crate::metrics::{CpuSample, DiskSample, NetSample, ProcessSample, SECTOR_SIZE, usage};
use crate::protocol::{Disk, Gpu, Hello, Memory, Thermal};
use crate::serve::{Collector, FastRaw, SLOW_EVERY, with_total};

type HANDLE = *mut c_void;
type BOOL = i32;

const INVALID_HANDLE: HANDLE = -1isize as HANDLE;
const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_TERMINATE: u32 = 0x0001;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER: u32 = 1;
const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const OPEN_EXISTING: u32 = 3;
const DRIVE_FIXED: u32 = 3;
const IF_OPER_STATUS_UP: u32 = 1;
const IF_FLAG_HARDWARE: u8 = 0x1;
const IF_FLAG_FILTER: u8 = 0x2;
const TH32CS_SNAPPROCESS: u32 = 0x2;
const COMPUTER_NAME_PHYSICAL_DNS_HOSTNAME: u32 = 5;
const MAX_PHYSICAL_DRIVES: usize = 16;
const FILETIME_NS: u64 = 100;

/// `SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION`. Kernel time includes idle time.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessorPerf {
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    dpc_time: i64,
    interrupt_time: i64,
    interrupt_count: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[repr(C)]
#[derive(Default)]
struct DiskPerformance {
    bytes_read: i64,
    bytes_written: i64,
    read_time: i64,
    write_time: i64,
    idle_time: i64,
    read_count: u32,
    write_count: u32,
    queue_depth: u32,
    split_count: u32,
    query_time: i64,
    storage_device_number: u32,
    storage_manager_name: [u16; 8],
}

#[repr(C)]
struct OsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    csd: [u16; 128],
}

/// `MIB_IF_ROW2` (netioapi.h); its size is checked below.
#[repr(C)]
struct MibIfRow2 {
    interface_luid: u64,
    interface_index: u32,
    interface_guid: [u8; 16],
    alias: [u16; 257],
    description: [u16; 257],
    physical_address_length: u32,
    physical_address: [u8; 32],
    permanent_physical_address: [u8; 32],
    mtu: u32,
    kind: u32,
    tunnel_type: u32,
    media_type: u32,
    physical_medium_type: u32,
    access_type: u32,
    direction_type: u32,
    interface_and_oper_status_flags: u8,
    oper_status: u32,
    admin_status: u32,
    media_connect_state: u32,
    network_guid: [u8; 16],
    connection_type: u32,
    transmit_link_speed: u64,
    receive_link_speed: u64,
    in_octets: u64,
    in_ucast_pkts: u64,
    in_nucast_pkts: u64,
    in_discards: u64,
    in_errors: u64,
    in_unknown_protos: u64,
    in_ucast_octets: u64,
    in_multicast_octets: u64,
    in_broadcast_octets: u64,
    out_octets: u64,
    out_ucast_pkts: u64,
    out_nucast_pkts: u64,
    out_discards: u64,
    out_errors: u64,
    out_ucast_octets: u64,
    out_multicast_octets: u64,
    out_broadcast_octets: u64,
    out_qlen: u64,
}

const _: () = assert!(std::mem::size_of::<MibIfRow2>() == 1352);
const _: () = assert!(std::mem::size_of::<DiskPerformance>() == 88);
const _: () = assert!(std::mem::size_of::<ProcessorPerf>() == 48);

#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    pri_class_base: i32,
    flags: u32,
    exe_file: [u16; 260],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn value(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> BOOL;
    fn GetTickCount64() -> u64;
    fn GetLogicalDriveStringsW(len: u32, buf: *mut u16) -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetDiskFreeSpaceExW(
        root: *const u16,
        avail: *mut u64,
        total: *mut u64,
        free: *mut u64,
    ) -> BOOL;
    fn K32GetProcessMemoryInfo(
        process: HANDLE,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> BOOL;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: HANDLE,
    ) -> HANDLE;
    fn DeviceIoControl(
        device: HANDLE,
        code: u32,
        input: *mut c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> BOOL;
    fn CloseHandle(handle: HANDLE) -> BOOL;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    fn Process32FirstW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> BOOL;
    fn Process32NextW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> BOOL;
    fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    fn GetProcessTimes(
        process: HANDLE,
        created: *mut FileTime,
        exited: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> BOOL;
    fn GetCurrentProcess() -> HANDLE;
    fn TerminateProcess(process: HANDLE, code: u32) -> BOOL;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn GetComputerNameExW(kind: u32, buf: *mut u16, size: *mut u32) -> BOOL;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: HANDLE, access: u32, token: *mut HANDLE) -> BOOL;
    fn GetTokenInformation(
        token: HANDLE,
        class: u32,
        info: *mut c_void,
        len: u32,
        ret: *mut u32,
    ) -> BOOL;
    fn LookupAccountSidW(
        system: *const u16,
        sid: *mut c_void,
        name: *mut u16,
        name_len: *mut u32,
        domain: *mut u16,
        domain_len: *mut u32,
        kind: *mut u32,
    ) -> BOOL;
    fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> BOOL;
}

#[link(name = "iphlpapi")]
unsafe extern "system" {
    fn GetIfTable2(table: *mut *mut c_void) -> u32;
    fn FreeMibTable(table: *mut c_void);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(chars: &[u16]) -> String {
    let end = chars.iter().position(|c| *c == 0).unwrap_or(chars.len());
    String::from_utf16_lossy(&chars[..end])
}

/// A handle closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE {
            // SAFETY: an open handle this agent owns.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn open_process(access: u32, pid: u32) -> Option<Owned> {
    // SAFETY: OpenProcess returns null on failure.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    (!handle.is_null()).then_some(Owned(handle))
}

/// The token user of a process, as a buffer holding a `TOKEN_USER`.
fn token_user(process: HANDLE) -> Option<Vec<u64>> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is written on success and closed below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = Owned(token);
    let mut len = 0u32;
    // SAFETY: a size query, then a fill of a buffer of that size (u64 keeps it aligned).
    unsafe {
        GetTokenInformation(token.0, TOKEN_USER, std::ptr::null_mut(), 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token.0, TOKEN_USER, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return None;
        }
        Some(buf)
    }
}

fn sid_of(user: &[u64]) -> *mut c_void {
    // SAFETY: the buffer starts with a TOKEN_USER, whose first field is the SID pointer.
    unsafe { (*(user.as_ptr() as *const SidAndAttributes)).sid }
}

fn sid_string(sid: *mut c_void) -> Option<String> {
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: on success `out` is a NUL-terminated string freed with LocalFree.
    unsafe {
        if ConvertSidToStringSidW(sid, &mut out) == 0 || out.is_null() {
            return None;
        }
        let mut len = 0;
        while *out.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
        LocalFree(out.cast());
        Some(text)
    }
}

fn process_user(process: HANDLE) -> String {
    let Some(user) = token_user(process) else {
        return "?".into();
    };
    let sid = sid_of(&user);
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut name_len, mut domain_len, mut kind) = (name.len() as u32, domain.len() as u32, 0u32);
    // SAFETY: both buffers are writable for the lengths passed.
    let found = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    let account = from_wide(&name);
    if found == 0 || account.is_empty() {
        return sid_string(sid).unwrap_or_else(|| "?".into());
    }
    account
}

#[derive(Clone)]
struct Owner {
    created: u64,
    user: String,
}

pub struct Host {
    start: Instant,
    gpu: bool,
    perf: Vec<ProcessorPerf>,
    owners: HashMap<u32, Owner>,
    drives: HashMap<usize, Owned>,
    last_scan: Option<Instant>,
}

impl Host {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            gpu: gpu::has_nvidia_smi(),
            perf: vec![ProcessorPerf::default(); 256],
            owners: HashMap::new(),
            drives: HashMap::new(),
            last_scan: None,
        }
    }

    /// Only the calling thread's processor group is returned, so hosts with more than 64
    /// logical processors report their first group.
    fn cpu_samples(&mut self) -> Result<Vec<CpuSample>, String> {
        let size = (self.perf.len() * std::mem::size_of::<ProcessorPerf>()) as u32;
        let mut returned = 0u32;
        // SAFETY: `perf` is `size` writable bytes.
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                self.perf.as_mut_ptr().cast(),
                size,
                &mut returned,
            )
        };
        if status != 0 {
            return Err(format!("NtQuerySystemInformation: 0x{:x}", status as u32));
        }
        let count = returned as usize / std::mem::size_of::<ProcessorPerf>();
        let cores = self.perf[..count.min(self.perf.len())]
            .iter()
            .enumerate()
            .map(|(index, perf)| CpuSample {
                name: format!("cpu{index}"),
                user: perf.user_time.max(0) as u64,
                system: (perf.kernel_time - perf.idle_time).max(0) as u64,
                idle: perf.idle_time.max(0) as u64,
                ..CpuSample::default()
            })
            .collect();
        Ok(with_total(cores))
    }

    /// Physical drive handles stay open; missing indexes are probed again every
    /// [`SLOW_EVERY`]. Busy time is query time minus idle time.
    fn disk_samples(&mut self) -> Vec<DiskSample> {
        let now = Instant::now();
        if self
            .last_scan
            .is_none_or(|last| now.duration_since(last) >= SLOW_EVERY)
        {
            self.last_scan = Some(now);
            for index in 0..MAX_PHYSICAL_DRIVES {
                if self.drives.contains_key(&index) {
                    continue;
                }
                let path = wide(&format!(r"\\.\PhysicalDrive{index}"));
                // SAFETY: a NUL-terminated path; failure returns INVALID_HANDLE.
                let handle = unsafe {
                    CreateFileW(
                        path.as_ptr(),
                        0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null_mut(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if handle != INVALID_HANDLE {
                    self.drives.insert(index, Owned(handle));
                }
            }
        }
        let mut out = Vec::with_capacity(self.drives.len());
        for index in 0..MAX_PHYSICAL_DRIVES {
            let Some(drive) = self.drives.get(&index) else {
                continue;
            };
            let mut perf = DiskPerformance::default();
            let mut returned = 0u32;
            // SAFETY: `perf` is writable for its size.
            let ok = unsafe {
                DeviceIoControl(
                    drive.0,
                    IOCTL_DISK_PERFORMANCE,
                    std::ptr::null_mut(),
                    0,
                    (&mut perf as *mut DiskPerformance).cast(),
                    std::mem::size_of::<DiskPerformance>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                self.drives.remove(&index);
                continue;
            }
            let busy = (perf.query_time - perf.idle_time).max(0) as u64;
            out.push(DiskSample {
                device: format!("PhysicalDrive{index}"),
                read_sectors: perf.bytes_read.max(0) as u64 / SECTOR_SIZE,
                write_sectors: perf.bytes_written.max(0) as u64 / SECTOR_SIZE,
                io_time_ms: busy * FILETIME_NS / 1_000_000,
            });
        }
        out
    }
}

/// Swap is the page file: its size is the commit limit above physical memory and its use is
/// the commit charge above physical use. Windows does not report page file occupancy here,
/// so this is an estimate.
fn memory() -> Result<Memory, String> {
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        ..MemoryStatusEx::default()
    };
    // SAFETY: `status` carries its own size.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let used = status.total_phys.saturating_sub(status.avail_phys);
    let swap_total = status.total_page_file.saturating_sub(status.total_phys);
    let commit = status
        .total_page_file
        .saturating_sub(status.avail_page_file);
    let swap_used = commit.saturating_sub(used).min(swap_total);
    Ok(Memory {
        total_kb: status.total_phys / 1024,
        available_kb: status.avail_phys / 1024,
        used_kb: used / 1024,
        swap_total_kb: swap_total / 1024,
        swap_free_kb: (swap_total - swap_used) / 1024,
        swap_used_kb: swap_used / 1024,
    })
}

/// Hardware interfaces that are up. Filter-driver rows (WFP, QoS) repeat the adapter they
/// sit on and are left out.
fn net_samples() -> Result<Vec<NetSample>, String> {
    let mut table: *mut c_void = std::ptr::null_mut();
    // SAFETY: on success `table` is a MIB_IF_TABLE2 freed with FreeMibTable.
    let code = unsafe { GetIfTable2(&mut table) };
    if code != 0 || table.is_null() {
        return Err(format!("GetIfTable2: {code}"));
    }
    let mut out = Vec::new();
    // SAFETY: NumEntries, then the rows at the row alignment (8).
    unsafe {
        let count = *(table as *const u32) as usize;
        let rows =
            std::slice::from_raw_parts((table as *const u8).add(8) as *const MibIfRow2, count);
        for row in rows {
            let flags = row.interface_and_oper_status_flags;
            if flags & IF_FLAG_HARDWARE == 0
                || flags & IF_FLAG_FILTER != 0
                || row.oper_status != IF_OPER_STATUS_UP
            {
                continue;
            }
            out.push(NetSample {
                name: from_wide(&row.alias),
                rx: row.in_octets,
                tx: row.out_octets,
            });
        }
        FreeMibTable(table);
    }
    Ok(out)
}

impl Collector for Host {
    fn hello(&mut self) -> Hello {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is writable for `len` characters.
        let named = unsafe {
            GetComputerNameExW(
                COMPUTER_NAME_PHYSICAL_DNS_HOSTNAME,
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        let hostname = if named != 0 && len > 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            "unknown".into()
        };
        let mut version = OsVersionInfo {
            size: std::mem::size_of::<OsVersionInfo>() as u32,
            major: 0,
            minor: 0,
            build: 0,
            platform: 0,
            csd: [0; 128],
        };
        // SAFETY: `version` carries its own size.
        let kernel = if unsafe { RtlGetVersion(&mut version) } == 0 {
            format!(
                "Windows {}.{}.{}",
                version.major, version.minor, version.build
            )
        } else {
            "Windows".into()
        };
        let cpus = std::thread::available_parallelism().map_or(1, |count| count.get());
        Hello::new(hostname, kernel, cpus as u32)
    }

    fn gpu_enabled(&self) -> bool {
        self.gpu
    }

    fn sample_fast(&mut self) -> FastRaw {
        let mut raw = FastRaw::new(self.start);
        raw.cpus = self.cpu_samples();
        raw.memory = memory();
        raw.nets = net_samples();
        // SAFETY: no preconditions.
        raw.uptime = Ok(Duration::from_millis(unsafe { GetTickCount64() }).as_secs_f64());
        raw.disks = Ok(self.disk_samples());
        raw
    }

    /// Processes the agent cannot open (protected ones, or another user's without
    /// elevation) are left out.
    fn sample_procs(&mut self, _mem_total_kb: u64) -> Result<Vec<ProcessSample>, String> {
        // SAFETY: a process snapshot, closed on drop.
        let snapshot = Owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) });
        if snapshot.0 == INVALID_HANDLE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: a zeroed entry with its size set is what Process32FirstW expects.
        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut seen = HashMap::with_capacity(self.owners.len());
        let mut out = Vec::with_capacity(128);
        // SAFETY: the snapshot is live and `entry` is sized.
        let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        while more {
            let pid = entry.process_id;
            if pid != 0
                && let Some(process) = open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid)
            {
                let mut times = [FileTime::default(); 4];
                let [created, exited, kernel, user] = &mut times;
                // SAFETY: four writable FILETIMEs.
                let timed =
                    unsafe { GetProcessTimes(process.0, created, exited, kernel, user) } != 0;
                if timed {
                    let mut counters = ProcessMemoryCounters {
                        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
                        ..ProcessMemoryCounters::default()
                    };
                    // SAFETY: `counters` carries its own size.
                    unsafe { K32GetProcessMemoryInfo(process.0, &mut counters, counters.cb) };
                    let created_at = times[0].value();
                    let owner = match self.owners.get(&pid) {
                        Some(owner) if owner.created == created_at => owner.clone(),
                        _ => Owner {
                            created: created_at,
                            user: process_user(process.0),
                        },
                    };
                    out.push(ProcessSample {
                        pid: i64::from(pid),
                        user: owner.user.clone(),
                        command: from_wide(&entry.exe_file),
                        cpu_ns: (times[2].value() + times[3].value()) * FILETIME_NS,
                        rss_bytes: counters.working_set_size as u64,
                    });
                    seen.insert(pid, owner);
                }
            }
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        self.owners = seen;
        Ok(out)
    }

    fn sample_disks(&mut self) -> Result<Vec<Disk>, String> {
        let mut buf = [0u16; 512];
        // SAFETY: `buf` is writable for its length.
        let len = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
        if len == 0 || len > buf.len() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut out = Vec::new();
        for root in buf[..len]
            .split(|c| *c == 0)
            .filter(|root| !root.is_empty())
        {
            let root: Vec<u16> = root.iter().copied().chain(std::iter::once(0)).collect();
            // SAFETY: a NUL-terminated root path.
            if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
                continue;
            }
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            // SAFETY: three writable counters.
            if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) } == 0
            {
                continue;
            }
            let mount = from_wide(&root);
            let used = usage(total, free, avail, 1);
            out.push(Disk {
                filesystem: mount.trim_end_matches('\\').to_string(),
                size_kb: used.size_kb,
                used_kb: used.used_kb,
                avail_kb: used.avail_kb,
                use_percent: used.use_percent,
                mount_point: mount,
            });
        }
        Ok(out)
    }

    fn sample_thermals(&mut self) -> Result<Vec<Thermal>, String> {
        Ok(Vec::new())
    }

    fn sample_gpus(&mut self) -> Result<Vec<Gpu>, String> {
        gpu::sample_nvidia_smi()
    }
}

/// Windows has no TERM: both signals end the process. The owner check compares the token
/// user's SID with the agent's.
pub fn kill(pid: i64, signal: &str) -> Result<(), String> {
    let pid32 = u32::try_from(pid).map_err(|_| "pid is out of range".to_string())?;
    let owner = same_owner(pid32).map(|same| if same { 0 } else { 1 });
    crate::commands::authorize_kill(pid, 0, owner, signal)?;
    let process = open_process(PROCESS_TERMINATE, pid32)
        .ok_or_else(|| std::io::Error::last_os_error().to_string())?;
    // SAFETY: a process handle opened for termination.
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn same_owner(pid: u32) -> Result<bool, String> {
    // SAFETY: the pseudo handle needs no closing.
    let me = token_user(unsafe { GetCurrentProcess() }).ok_or("could not read the agent's user")?;
    let process =
        open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid).ok_or("process not found")?;
    let theirs = token_user(process.0).ok_or("could not read the process's user")?;
    let (mine, theirs) = (sid_string(sid_of(&me)), sid_string(sid_of(&theirs)));
    Ok(mine.is_some() && mine == theirs)
}
