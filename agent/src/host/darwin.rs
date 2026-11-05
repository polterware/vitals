//! macOS: CPU ticks and memory from Mach, network from `NET_RT_IFLIST2` (64-bit counters),
//! disk I/O from IOKit's block storage drivers, processes from libproc, and volumes from
//! `getfsstat`. Thermal sensors need private SMC calls and GPUs are not queried, so both are
//! reported empty.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::time::Instant;

use crate::metrics::{CpuSample, DiskSample, NetSample, ProcessSample, SECTOR_SIZE, usage};
use crate::protocol::{Disk, Gpu, Hello, Memory, Thermal};
use crate::serve::{Collector, FastRaw, with_total};

type MachPort = u32;
type KernReturn = c_int;

const PROCESSOR_CPU_LOAD_INFO: c_int = 2;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
const CPU_STATE_NICE: usize = 3;
const HOST_VM_INFO64: c_int = 4;
const MNT_NOWAIT: c_int = 2;
const MNT_LOCAL: u32 = 0x0000_1000;
const MNT_DONTBROWSE: u32 = 0x0010_0000;
const MAX_COMLEN: usize = 16;

/// `vm_statistics64` up to `total_uncompressed_pages_in_compressor`: the fields every macOS
/// fills. The count passed tells the kernel how much of it to write.
#[repr(C, align(8))]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

#[repr(C)]
#[derive(Default)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    static mach_task_self_: MachPort;
    static vm_kernel_page_size: usize;
    fn mach_host_self() -> MachPort;
    fn host_processor_info(
        host: MachPort,
        flavor: c_int,
        count: *mut u32,
        info: *mut *mut c_int,
        info_count: *mut u32,
    ) -> KernReturn;
    fn host_statistics64(
        host: MachPort,
        flavor: c_int,
        info: *mut c_int,
        count: *mut u32,
    ) -> KernReturn;
    fn vm_deallocate(task: MachPort, address: usize, size: usize) -> KernReturn;
    fn mach_timebase_info(info: *mut TimebaseInfo) -> KernReturn;
}

type CfTypeRef = *const c_void;
type CfAllocatorRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorDefault: CfAllocatorRef;
    fn CFStringCreateWithCString(
        allocator: CfAllocatorRef,
        text: *const c_char,
        encoding: u32,
    ) -> CfTypeRef;
    fn CFStringGetCString(text: CfTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFDictionaryGetValue(dict: CfTypeRef, key: CfTypeRef) -> CfTypeRef;
    fn CFNumberGetValue(number: CfTypeRef, kind: isize, out: *mut c_void) -> u8;
    fn CFGetTypeID(value: CfTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFRelease(value: CfTypeRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CfTypeRef;
    fn IOServiceGetMatchingServices(
        port: MachPort,
        matching: CfTypeRef,
        iter: *mut u32,
    ) -> KernReturn;
    fn IOIteratorNext(iter: u32) -> u32;
    fn IORegistryEntryGetChildEntry(
        entry: u32,
        plane: *const c_char,
        child: *mut u32,
    ) -> KernReturn;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CfTypeRef,
        allocator: CfAllocatorRef,
        options: u32,
    ) -> CfTypeRef;
    fn IOObjectRelease(object: u32) -> KernReturn;
}

const UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64: isize = 4;

/// A CFString that releases itself.
struct CfString(CfTypeRef);

impl CfString {
    fn new(text: &str) -> Self {
        let text = CString::new(text).expect("no NUL in keys");
        // SAFETY: `text` is NUL-terminated UTF-8; the result is owned and released on drop.
        Self(unsafe { CFStringCreateWithCString(kCFAllocatorDefault, text.as_ptr(), UTF8) })
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created by CFStringCreateWithCString above.
            unsafe { CFRelease(self.0) }
        }
    }
}

/// The IOKit statistics keys, made once.
struct DiskKeys {
    bsd_name: CfString,
    statistics: CfString,
    bytes_read: CfString,
    bytes_written: CfString,
    read_time: CfString,
    write_time: CfString,
}

pub struct Host {
    start: Instant,
    host: MachPort,
    timebase: TimebaseInfo,
    users: HashMap<u32, String>,
    disk_keys: DiskKeys,
    pids: Vec<c_int>,
}

impl Host {
    pub fn new() -> Self {
        let mut timebase = TimebaseInfo::default();
        // SAFETY: plain out-parameter calls; mach_host_self returns a send right kept for life.
        let host = unsafe {
            mach_timebase_info(&mut timebase);
            mach_host_self()
        };
        if timebase.denom == 0 {
            timebase = TimebaseInfo { numer: 1, denom: 1 };
        }
        Self {
            start: Instant::now(),
            host,
            timebase,
            users: HashMap::new(),
            disk_keys: DiskKeys {
                bsd_name: CfString::new("BSD Name"),
                statistics: CfString::new("Statistics"),
                bytes_read: CfString::new("Bytes (Read)"),
                bytes_written: CfString::new("Bytes (Write)"),
                read_time: CfString::new("Total Time (Read)"),
                write_time: CfString::new("Total Time (Write)"),
            },
            pids: Vec::new(),
        }
    }

    fn cpu_samples(&self) -> Result<Vec<CpuSample>, String> {
        let mut count = 0u32;
        let mut info: *mut c_int = std::ptr::null_mut();
        let mut info_count = 0u32;
        // SAFETY: out-parameters only; the kernel allocates `info`, freed below.
        let code = unsafe {
            host_processor_info(
                self.host,
                PROCESSOR_CPU_LOAD_INFO,
                &mut count,
                &mut info,
                &mut info_count,
            )
        };
        if code != 0 || info.is_null() {
            return Err("host_processor_info failed".into());
        }
        // SAFETY: `info` holds `info_count` integers, four ticks per processor.
        let ticks = unsafe { std::slice::from_raw_parts(info, info_count as usize) };
        let cores = ticks
            .chunks_exact(4)
            .take(count as usize)
            .enumerate()
            .map(|(index, cpu)| CpuSample {
                name: format!("cpu{index}"),
                user: cpu[CPU_STATE_USER] as u32 as u64,
                system: cpu[CPU_STATE_SYSTEM] as u32 as u64,
                idle: cpu[CPU_STATE_IDLE] as u32 as u64,
                nice: cpu[CPU_STATE_NICE] as u32 as u64,
                ..CpuSample::default()
            })
            .collect();
        // SAFETY: the region came from host_processor_info in this task.
        unsafe {
            vm_deallocate(
                mach_task_self_,
                info as usize,
                info_count as usize * std::mem::size_of::<c_int>(),
            );
        }
        Ok(with_total(cores))
    }

    /// Used matches Activity Monitor: app memory (anonymous minus purgeable), wired, and
    /// compressed.
    fn memory(&self) -> Result<Memory, String> {
        let total: u64 = sysctl_value("hw.memsize").ok_or("hw.memsize failed")?;
        let mut vm = VmStatistics64::default();
        let mut count =
            (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<c_int>()) as u32;
        // SAFETY: `vm` is as large as `count` integers.
        let code = unsafe {
            host_statistics64(
                self.host,
                HOST_VM_INFO64,
                (&mut vm as *mut VmStatistics64).cast(),
                &mut count,
            )
        };
        if code != 0 {
            return Err("host_statistics64 failed".into());
        }
        // SAFETY: a constant the kernel exports.
        let page = unsafe { vm_kernel_page_size } as u64;
        let app = u64::from(vm.internal_page_count.saturating_sub(vm.purgeable_count));
        let used = ((app + u64::from(vm.wire_count) + u64::from(vm.compressor_page_count)) * page)
            .min(total);
        let swap: libc::xsw_usage = sysctl_value("vm.swapusage").unwrap_or(libc::xsw_usage {
            xsu_total: 0,
            xsu_avail: 0,
            xsu_used: 0,
            xsu_pagesize: 0,
            xsu_encrypted: 0,
        });
        let swap_used = swap.xsu_used.min(swap.xsu_total);
        Ok(Memory {
            total_kb: total / 1024,
            available_kb: (total - used) / 1024,
            used_kb: used / 1024,
            swap_total_kb: swap.xsu_total / 1024,
            swap_free_kb: (swap.xsu_total - swap_used) / 1024,
            swap_used_kb: swap_used / 1024,
        })
    }

    /// Physical block devices: IOBlockStorageDriver sits under real disks, not under the
    /// synthesized APFS containers. Read and write service times add up to busy time, which
    /// can pass wall time; the rate caps utilization at 100.
    fn disk_samples(&self) -> Result<Vec<DiskSample>, String> {
        let keys = &self.disk_keys;
        let mut iter = 0u32;
        // SAFETY: the matching dictionary is consumed by the call; `iter` is released below.
        let code = unsafe {
            IOServiceGetMatchingServices(
                0,
                IOServiceMatching(c"IOBlockStorageDriver".as_ptr()),
                &mut iter,
            )
        };
        if code != 0 {
            return Err("IOKit block storage query failed".into());
        }
        let mut out = Vec::new();
        loop {
            // SAFETY: `iter` is a live iterator; each entry is released after use.
            let driver = unsafe { IOIteratorNext(iter) };
            if driver == 0 {
                break;
            }
            let mut media = 0u32;
            // SAFETY: registry calls on live entries; created properties are released.
            unsafe {
                if IORegistryEntryGetChildEntry(driver, c"IOService".as_ptr(), &mut media) == 0 {
                    let bsd = IORegistryEntryCreateCFProperty(
                        media,
                        keys.bsd_name.0,
                        kCFAllocatorDefault,
                        0,
                    );
                    let stats = IORegistryEntryCreateCFProperty(
                        driver,
                        keys.statistics.0,
                        kCFAllocatorDefault,
                        0,
                    );
                    if !bsd.is_null()
                        && !stats.is_null()
                        && CFGetTypeID(bsd) == CFStringGetTypeID()
                        && CFGetTypeID(stats) == CFDictionaryGetTypeID()
                    {
                        let mut name = [0 as c_char; 64];
                        if CFStringGetCString(bsd, name.as_mut_ptr(), name.len() as isize, UTF8)
                            != 0
                        {
                            let number =
                                |key: &CfString| cf_u64(CFDictionaryGetValue(stats, key.0));
                            let read = number(&keys.bytes_read);
                            let written = number(&keys.bytes_written);
                            let busy_ns = number(&keys.read_time) + number(&keys.write_time);
                            out.push(DiskSample {
                                device: CStr::from_ptr(name.as_ptr())
                                    .to_string_lossy()
                                    .into_owned(),
                                read_sectors: read / SECTOR_SIZE,
                                write_sectors: written / SECTOR_SIZE,
                                io_time_ms: busy_ns / 1_000_000,
                            });
                        }
                    }
                    if !bsd.is_null() {
                        CFRelease(bsd);
                    }
                    if !stats.is_null() {
                        CFRelease(stats);
                    }
                    IOObjectRelease(media);
                }
                IOObjectRelease(driver);
            }
        }
        // SAFETY: the iterator came from IOServiceGetMatchingServices.
        unsafe { IOObjectRelease(iter) };
        Ok(out)
    }

    fn user(&mut self, uid: u32) -> String {
        self.users
            .entry(uid)
            .or_insert_with(|| super::user_name(uid))
            .clone()
    }
}

/// A positive CFNumber as u64; anything else is 0.
///
/// # Safety
/// `value` is null or a live CF object.
unsafe fn cf_u64(value: CfTypeRef) -> u64 {
    if value.is_null() {
        return 0;
    }
    let mut out: i64 = 0;
    // SAFETY: the type is checked before the value is read.
    unsafe {
        if CFGetTypeID(value) == CFNumberGetTypeID()
            && CFNumberGetValue(value, CF_NUMBER_SINT64, (&mut out as *mut i64).cast()) != 0
            && out > 0
        {
            out as u64
        } else {
            0
        }
    }
}

/// A sysctl read into a plain value.
fn sysctl_value<T>(name: &str) -> Option<T> {
    let name = CString::new(name).ok()?;
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: `value` is `len` writable bytes; the kernel writes at most that.
    let code = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // SAFETY: zeroed plain-data types are valid, and the call filled it.
    (code == 0 && len == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}

fn sysctl_string(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let mut len = 0usize;
    // SAFETY: a size query with a null buffer, then a read into a buffer of that size.
    unsafe {
        if libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let end = buf.iter().position(|byte| *byte == 0).unwrap_or(len);
        Some(String::from_utf8_lossy(&buf[..end]).trim().to_string())
    }
}

/// `NET_RT_IFLIST2`: interfaces with 64-bit byte counters (getifaddrs only has 32-bit ones).
/// Physical macOS NICs are `en*`; awdl and llw (AirDrop), utun, bridge, and anpi are virtual
/// or internal. Interfaces that are down are left out.
fn net_samples() -> Result<Vec<NetSample>, String> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut len = 0usize;
    // SAFETY: a size query, then a read into a buffer of that size.
    let buf = unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            6,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Err("NET_RT_IFLIST2 failed".into());
        }
        let mut buf = vec![0u8; len];
        if libc::sysctl(
            mib.as_mut_ptr(),
            6,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Err("NET_RT_IFLIST2 failed".into());
        }
        buf.truncate(len);
        buf
    };
    let mut out = Vec::new();
    let header = std::mem::size_of::<libc::if_msghdr>();
    let mut offset = 0;
    while offset + header <= buf.len() {
        // SAFETY: at least a header's bytes remain; unaligned reads copy the packed structs.
        let message: libc::if_msghdr =
            unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
        let length = message.ifm_msglen as usize;
        if length == 0 || offset + length > buf.len() {
            break;
        }
        if c_int::from(message.ifm_type) == libc::RTM_IFINFO2
            && length >= std::mem::size_of::<libc::if_msghdr2>()
        {
            // SAFETY: an RTM_IFINFO2 message is an if_msghdr2 followed by its link address.
            let info: libc::if_msghdr2 =
                unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
            let link_at = offset + std::mem::size_of::<libc::if_msghdr2>();
            if info.ifm_addrs & libc::RTA_IFP != 0 && link_at + 8 <= buf.len() {
                // sockaddr_dl: len, family, index (2), type, nlen, alen, slen, then data.
                let family = c_int::from(buf[link_at + 1]);
                let name_len = buf[link_at + 5] as usize;
                let name_at = link_at + 8;
                if family == libc::AF_LINK
                    && name_len > 0
                    && name_len < libc::IFNAMSIZ
                    && name_at + name_len <= buf.len()
                {
                    let name =
                        String::from_utf8_lossy(&buf[name_at..name_at + name_len]).into_owned();
                    let flags = info.ifm_flags;
                    let data = info.ifm_data;
                    let up = flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0;
                    if name.starts_with("en") && up {
                        out.push(NetSample {
                            name,
                            rx: data.ifi_ibytes,
                            tx: data.ifi_obytes,
                        });
                    }
                }
            }
        }
        offset += length;
    }
    Ok(out)
}

fn uptime() -> Result<f64, String> {
    let boot: libc::timeval = sysctl_value("kern.boottime").ok_or("kern.boottime failed")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock before 1970".to_string())?;
    Ok(now.as_secs_f64() - (boot.tv_sec as f64 + boot.tv_usec as f64 / 1e6))
}

fn load() -> Result<[f64; 3], String> {
    let mut load = [0.0f64; 3];
    // SAFETY: three writable doubles.
    if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } != 3 {
        return Err("getloadavg failed".into());
    }
    Ok(load)
}

fn cstr(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn short_info(pid: c_int) -> Option<libc::proc_bsdshortinfo> {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as c_int;
    // SAFETY: `info` is `size` writable bytes.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    (read == size).then_some(info)
}

impl Collector for Host {
    fn hello(&mut self) -> Hello {
        let kernel = sysctl_string("kern.osrelease")
            .filter(|kernel| !kernel.is_empty())
            .map_or_else(
                || "unknown".to_string(),
                |kernel| format!("Darwin {kernel}"),
            );
        let cpus = std::thread::available_parallelism().map_or(1, |count| count.get());
        Hello::new(super::hostname(), kernel, cpus as u32)
    }

    fn gpu_enabled(&self) -> bool {
        false
    }

    fn sample_fast(&mut self) -> FastRaw {
        let mut raw = FastRaw::new(self.start);
        raw.cpus = self.cpu_samples();
        raw.memory = self.memory();
        raw.nets = net_samples();
        raw.load = load();
        raw.uptime = uptime();
        raw.disks = self.disk_samples();
        raw
    }

    /// Mach absolute time is scaled by the timebase (it is ticks on Apple silicon). Other
    /// users' tasks fail with EPERM unless the agent runs as root, and are left out.
    fn sample_procs(&mut self, _mem_total_kb: u64) -> Result<Vec<ProcessSample>, String> {
        // SAFETY: a size query, then a fill of at most the buffer's bytes.
        let count = unsafe {
            let wanted = libc::proc_listallpids(std::ptr::null_mut(), 0);
            if wanted <= 0 {
                return Err("proc_listallpids failed".into());
            }
            self.pids.resize(wanted as usize + 64, 0);
            libc::proc_listallpids(
                self.pids.as_mut_ptr().cast(),
                (self.pids.len() * std::mem::size_of::<c_int>()) as c_int,
            )
        };
        if count < 0 {
            return Err("proc_listallpids failed".into());
        }
        let pids: Vec<c_int> = self.pids[..(count as usize).min(self.pids.len())].to_vec();
        let mut out = Vec::with_capacity(pids.len());
        for pid in pids {
            if pid <= 0 {
                continue;
            }
            let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_taskinfo>() as c_int;
            // SAFETY: `task` is `size` writable bytes.
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTASKINFO,
                    0,
                    (&mut task as *mut libc::proc_taskinfo).cast(),
                    size,
                )
            };
            if read != size {
                continue;
            }
            let Some(short) = short_info(pid) else {
                continue;
            };
            let mut name = [0 as c_char; 2 * MAX_COMLEN + 1];
            // SAFETY: `name` is writable for its length.
            let named =
                unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
            let command = if named > 0 {
                cstr(&name)
            } else {
                cstr(&short.pbsi_comm)
            };
            let ticks = u128::from(task.pti_total_user + task.pti_total_system);
            let cpu_ns =
                (ticks * u128::from(self.timebase.numer) / u128::from(self.timebase.denom)) as u64;
            out.push(ProcessSample {
                pid: i64::from(pid),
                user: self.user(short.pbsi_uid),
                command,
                cpu_ns,
                rss_bytes: task.pti_resident_size,
            });
        }
        Ok(out)
    }

    /// Local volumes Finder shows: `MNT_DONTBROWSE` ones (the sealed system snapshot's
    /// siblings) share the container's free space with `/`.
    fn sample_disks(&mut self) -> Result<Vec<Disk>, String> {
        // SAFETY: a count query, then a fill of at most the buffer's bytes.
        let volumes = unsafe {
            let count = libc::getfsstat(std::ptr::null_mut(), 0, MNT_NOWAIT);
            if count < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut buf: Vec<libc::statfs> = vec![std::mem::zeroed(); count as usize + 8];
            let size = (buf.len() * std::mem::size_of::<libc::statfs>()) as c_int;
            let filled = libc::getfsstat(buf.as_mut_ptr(), size, MNT_NOWAIT);
            if filled < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            buf.truncate(filled as usize);
            buf
        };
        let mut out = Vec::new();
        for volume in &volumes {
            let fs_type = cstr(&volume.f_fstypename);
            if volume.f_flags & MNT_LOCAL == 0
                || volume.f_flags & MNT_DONTBROWSE != 0
                || fs_type == "devfs"
                || fs_type == "autofs"
            {
                continue;
            }
            let used = usage(
                volume.f_blocks,
                volume.f_bfree,
                volume.f_bavail,
                u64::from(volume.f_bsize),
            );
            out.push(Disk {
                filesystem: cstr(&volume.f_mntfromname),
                size_kb: used.size_kb,
                used_kb: used.used_kb,
                avail_kb: used.avail_kb,
                use_percent: used.use_percent,
                mount_point: cstr(&volume.f_mntonname),
            });
        }
        Ok(out)
    }

    fn sample_thermals(&mut self) -> Result<Vec<Thermal>, String> {
        Ok(Vec::new())
    }

    fn sample_gpus(&mut self) -> Result<Vec<Gpu>, String> {
        Ok(Vec::new())
    }
}

pub fn kill(pid: i64, signal: &str) -> Result<(), String> {
    let owner = c_int::try_from(pid)
        .ok()
        .and_then(short_info)
        .map(|info| info.pbsi_uid)
        .ok_or_else(|| "process not found".to_string());
    super::signal(pid, signal, owner)
}
