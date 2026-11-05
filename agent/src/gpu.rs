//! NVIDIA GPUs through `nvidia-smi`, on Linux and Windows hosts that have it on `PATH`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::metrics::parse_nvidia_smi;
use crate::protocol::Gpu;

const TIMEOUT: Duration = Duration::from_secs(2);

/// True when `nvidia-smi` is on `PATH`.
pub fn has_nvidia_smi() -> bool {
    let name = if cfg!(windows) {
        "nvidia-smi.exe"
    } else {
        "nvidia-smi"
    };
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// One query, given up after two seconds so a hung driver never stalls the stream.
pub fn sample_nvidia_smi() -> Result<Vec<Gpu>, String> {
    let mut child = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,temperature.gpu,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("nvidia-smi timed out".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let text = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("nvidia-smi exited with {status}"));
    }
    Ok(parse_nvidia_smi(&text))
}
