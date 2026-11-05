//! The script fallback's remote side, from the Electron app's `remote-script.ts`: where the
//! agent cannot be installed or run, one exec runs this sampling loop on the host and
//! [`crate::parsers`] reads what it prints.
//!
//! One long-running loop instead of one exec per sample: no SSH channel setup each second,
//! and fewer processes on the host, which would otherwise show in its CPU reading. The fast
//! sections print every second, `ps` every 2s, `nvidia-smi` every 3s, and `df` and the thermal
//! zones every 10s; each sample ends with the `end` marker. The thermal loop uses shell
//! builtins only. It only reads.

use crate::installer::shell_quote;
use crate::protocol::Signal;

/// The loop, for `sh`. Its markers are [`crate::parsers::SECTION_MARKER`].
pub const SCRIPT: &str = r#"command -v nvidia-smi >/dev/null 2>&1 && gpu=1 || gpu=0
i=0
while :; do
  printf 'VITALS_SECTION stat\n'
  cat /proc/stat 2>/dev/null
  printf 'VITALS_SECTION meminfo\n'
  cat /proc/meminfo 2>/dev/null
  printf 'VITALS_SECTION netdev\n'
  cat /proc/net/dev 2>/dev/null
  printf 'VITALS_SECTION loadavg\n'
  cat /proc/loadavg 2>/dev/null
  printf 'VITALS_SECTION uptime\n'
  cat /proc/uptime 2>/dev/null
  if [ $((i % 2)) -eq 0 ]; then
    printf 'VITALS_SECTION ps\n'
    { ps -eo pid,user,pcpu,pmem,comm --sort=-pcpu 2>/dev/null ||
      ps -eo pid,user,pcpu,pmem,comm 2>/dev/null; } | head -25
  fi
  if [ $((i % 10)) -eq 0 ]; then
    printf 'VITALS_SECTION df\n'
    df -P -k 2>/dev/null
    printf 'VITALS_SECTION thermal\n'
    for z in /sys/class/thermal/thermal_zone*/temp; do
      [ -f "$z" ] || continue
      d=${z%/temp}
      read -r t < "$z" 2>/dev/null || continue
      printf '%s:%s\n' "${d##*/}" "$t"
    done
  fi
  if [ "$gpu" = 1 ] && [ $((i % 3)) -eq 0 ]; then
    printf 'VITALS_SECTION nvidia\n'
    nvidia-smi --query-gpu=index,name,temperature.gpu,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null
  fi
  printf 'VITALS_SECTION end\n'
  i=$((i + 1))
  sleep 1
done
"#;

/// Runs [`SCRIPT`] under `sh`, whatever the account's login shell is.
pub fn command() -> String {
    format!("sh -c {}", shell_quote(SCRIPT))
}

/// Stops `pid` with `signal` where no agent runs to do it, or `None` for a pid no process
/// has: 0 names the caller's process group, and one past `i32::MAX` wraps to -1, every
/// process the account may signal.
pub fn kill_command(pid: u32, signal: Signal) -> Option<String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    Some(match signal {
        Signal::Kill => format!("kill -9 {pid}"),
        Signal::Term => format!("kill {pid}"),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::parsers::{self, FrameBuffer, SECTION_MARKER};

    #[test]
    fn markers_and_kills() {
        for line in SCRIPT
            .lines()
            .filter(|line| line.contains("VITALS_SECTION"))
        {
            assert!(
                line.trim()
                    .starts_with(&format!("printf '{SECTION_MARKER} ")),
                "{line}"
            );
        }
        assert_eq!(kill_command(42, Signal::Term).unwrap(), "kill 42");
        assert_eq!(kill_command(42, Signal::Kill).unwrap(), "kill -9 42");
        assert_eq!(kill_command(0, Signal::Term), None);
        assert_eq!(kill_command(u32::MAX, Signal::Kill), None);
        assert!(kill_command(i32::MAX as u32, Signal::Term).is_some());
    }

    /// The command a host's shell gets unquotes to the script, and the script parses.
    #[cfg(unix)]
    #[test]
    fn the_command_is_the_script_for_sh() {
        let command = command();
        let quoted = command.strip_prefix("sh -c ").expect("runs under sh");
        let printed = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("sh runs");
        assert_eq!(String::from_utf8(printed.stdout).unwrap(), SCRIPT);
        let checked = Command::new("sh")
            .args(["-n", "-c", SCRIPT])
            .status()
            .unwrap();
        assert!(checked.success());
    }

    /// One sample of this machine: a Linux one is measured, and one without `/proc` (a Mac)
    /// is the host the fallback reports it cannot read.
    #[cfg(unix)]
    #[test]
    fn the_script_samples_this_machine() {
        let mut child = Command::new("sh")
            .args(["-c", SCRIPT])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh runs");
        let mut stdout = child.stdout.take().unwrap();
        let (sent, frames) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = FrameBuffer::default();
            let mut chunk = [0u8; 8192];
            while let Ok(read) = stdout.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                if let Some(frame) = buffer.push(&chunk[..read]).into_iter().next() {
                    let _ = sent.send(frame);
                    break;
                }
            }
        });
        let frame = frames.recv_timeout(Duration::from_secs(20));
        let _ = child.kill();
        let _ = child.wait();
        let frame = frame.expect("the script prints a sample");
        let sample = parsers::build_metrics(&frame, None, 1);
        let sections = parsers::split_sections(&frame);
        assert!(sections.contains_key("df") && sections.contains_key("ps"));
        if cfg!(target_os = "linux") {
            assert!(sample.supported);
            assert!(sample.metrics.memory.total_kb > 0.0);
            assert!(!sample.metrics.cpu_cores.is_empty());
            assert!(sample.metrics.uptime_seconds > 0.0);
            assert!(!sample.metrics.disks.unwrap_or_default().is_empty());
        } else {
            assert!(!sample.supported);
        }
    }
}
