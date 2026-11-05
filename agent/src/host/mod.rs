//! The host this agent runs on, as a [`Collector`](crate::serve::Collector) and the kill it
//! answers with.

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub mod procfile;
#[cfg(windows)]
mod windows;

use std::io;

use crate::commands::KillFn;

#[cfg(target_os = "linux")]
pub fn open() -> io::Result<(impl crate::serve::Collector, KillFn)> {
    Ok((
        linux::Host::new(),
        std::sync::Arc::new(linux::kill) as KillFn,
    ))
}

#[cfg(target_os = "macos")]
pub fn open() -> io::Result<(impl crate::serve::Collector, KillFn)> {
    Ok((
        darwin::Host::new(),
        std::sync::Arc::new(darwin::kill) as KillFn,
    ))
}

#[cfg(windows)]
pub fn open() -> io::Result<(impl crate::serve::Collector, KillFn)> {
    Ok((
        windows::Host::new(),
        std::sync::Arc::new(windows::kill) as KillFn,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn open() -> io::Result<(Unsupported, KillFn)> {
    Err(io::Error::other(
        "vitals-agent supports linux, darwin and windows only",
    ))
}

/// Stands in for a host this agent does not support; [`open`] never returns one.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub struct Unsupported;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl crate::serve::Collector for Unsupported {
    fn hello(&mut self) -> crate::protocol::Hello {
        unreachable!("unsupported hosts never start")
    }
    fn sample_fast(&mut self) -> crate::serve::FastRaw {
        unreachable!("unsupported hosts never start")
    }
    fn sample_procs(&mut self, _: u64) -> Result<Vec<crate::metrics::ProcessSample>, String> {
        unreachable!("unsupported hosts never start")
    }
    fn sample_disks(&mut self) -> Result<Vec<crate::protocol::Disk>, String> {
        unreachable!("unsupported hosts never start")
    }
    fn sample_thermals(&mut self) -> Result<Vec<crate::protocol::Thermal>, String> {
        unreachable!("unsupported hosts never start")
    }
    fn sample_gpus(&mut self) -> Result<Vec<crate::protocol::Gpu>, String> {
        unreachable!("unsupported hosts never start")
    }
    fn gpu_enabled(&self) -> bool {
        false
    }
}

/// `uid`'s user name from the system's user database, or the number.
#[cfg(target_os = "macos")]
pub fn user_name(uid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 1024];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    loop {
        // SAFETY: the buffers outlive the call; `result` points into `pwd` on success.
        let code =
            unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
        if code == libc::ERANGE && buf.len() < 1 << 16 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if code != 0 || result.is_null() || pwd.pw_name.is_null() {
            return uid.to_string();
        }
        // SAFETY: a found entry's name is a NUL-terminated string inside `buf`.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        let name = name.to_string_lossy();
        return if name.is_empty() {
            uid.to_string()
        } else {
            name.into_owned()
        };
    }
}

/// This machine's name, or `unknown`.
#[cfg(unix)]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for its whole length.
    let code = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if code != 0 || name.is_empty() {
        "unknown".into()
    } else {
        name
    }
}

/// Sends TERM or KILL after the owner check.
#[cfg(unix)]
pub fn signal(pid: i64, signal: &str, owner: Result<u32, String>) -> Result<(), String> {
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    crate::commands::authorize_kill(pid, me, owner, signal)?;
    let number = if signal == "KILL" {
        libc::SIGKILL
    } else {
        libc::SIGTERM
    };
    let pid = libc::pid_t::try_from(pid).map_err(|_| "pid is out of range".to_string())?;
    // SAFETY: kill takes any pid and signal number and reports errors through errno.
    if unsafe { libc::kill(pid, number) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}
