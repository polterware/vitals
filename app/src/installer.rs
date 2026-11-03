//! Puts the matching vitals-agent on the host, as the Electron app does: detect the remote
//! OS and architecture, then keep the binary at `~/.cache/vitals/agent-<sha12>` (mode 0700,
//! in a 0700 folder) on Linux and macOS, or `%LOCALAPPDATA%\Vitals\agent-<sha12>.exe` on
//! Windows. A cached copy runs only when its SHA-256 matches; otherwise the binary goes up
//! under a temporary name and is renamed into place, so a partial or altered file never runs.

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileAttributes;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::ssh::Connection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Darwin,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    Amd64,
    Arm64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub os: Os,
    pub arch: Arch,
}

pub fn map_arch(machine: &str) -> Option<Arch> {
    match machine.trim() {
        "x86_64" | "amd64" => Some(Arch::Amd64),
        "aarch64" | "arm64" => Some(Arch::Arm64),
        _ => None,
    }
}

/// `uname -sm` ("Linux x86_64", "Darwin arm64") as a target.
pub fn map_uname(uname: &str) -> Result<Target> {
    let mut parts = uname.split_whitespace();
    let os = match parts.next().unwrap_or("") {
        "Linux" => Os::Linux,
        "Darwin" => Os::Darwin,
        _ => bail!("Unsupported remote platform: {}", uname.trim()),
    };
    let arch = map_arch(parts.next().unwrap_or(""))
        .ok_or_else(|| anyhow!("Unsupported remote platform: {}", uname.trim()))?;
    Ok(Target { os, arch })
}

/// Windows `%PROCESSOR_ARCHITECTURE%` (AMD64, ARM64) as a target.
pub fn map_windows(processor: &str) -> Result<Target> {
    let arch = match processor.trim().to_ascii_uppercase().as_str() {
        "AMD64" => Arch::Amd64,
        "ARM64" => Arch::Arm64,
        other => bail!("Unsupported remote platform: Windows {other}"),
    };
    Ok(Target {
        os: Os::Windows,
        arch,
    })
}

pub fn binary_name(target: Target) -> String {
    let os = match target.os {
        Os::Linux => "linux",
        Os::Darwin => "darwin",
        Os::Windows => "windows",
    };
    let arch = match target.arch {
        Arch::Amd64 => "amd64",
        Arch::Arm64 => "arm64",
    };
    let ext = if target.os == Os::Windows { ".exe" } else { "" };
    format!("vitals-agent-{os}-{arch}{ext}")
}

/// The folders the agent binaries may be in: `VITALS_AGENT_DIR`, the app bundle's
/// `Resources/agent`, then the repository's `agent/dist` that `agent/build.sh` fills.
pub fn agent_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("VITALS_AGENT_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(macos) = exe.parent()
    {
        dirs.push(macos.join("../Resources/agent"));
    }
    dirs.push(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../agent/dist"
    )));
    dirs
}

pub fn local_binary(target: Target) -> Result<PathBuf> {
    let name = binary_name(target);
    agent_dirs()
        .into_iter()
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("No bundled agent for this host ({name}); run agent/build.sh"))
}

pub fn cache_path(home: &str, sha256: &str) -> String {
    format!(
        "{}/.cache/vitals/agent-{}",
        home.trim_end_matches('/'),
        &sha256[..12]
    )
}

pub fn windows_path(local_app_data: &str, sha256: &str) -> String {
    format!(
        "{}\\Vitals\\agent-{}.exe",
        local_app_data.trim_end_matches(['\\', '/']),
        &sha256[..12]
    )
}

/// Windows OpenSSH SFTP addresses drive paths as `/C:/dir/file`.
pub fn windows_sftp_path(native: &str) -> String {
    format!("/{}", native.replace('\\', "/"))
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The command that starts the agent. On Windows `cmd /c` runs it whether the account's
/// shell is cmd or PowerShell.
pub fn exec_command(os: Os, remote_path: &str) -> String {
    match os {
        Os::Windows => format!("cmd /c \"{remote_path}\""),
        _ => shell_quote(remote_path),
    }
}

pub fn remote_sha256_command(os: Os, remote_path: &str) -> String {
    match os {
        Os::Windows => format!("cmd /c certutil -hashfile \"{remote_path}\" SHA256"),
        _ => {
            let path = shell_quote(remote_path);
            format!("sha256sum {path} 2>/dev/null || shasum -a 256 {path} 2>/dev/null")
        }
    }
}

/// The first 64-character hex digest in sha256sum, shasum, or certutil output.
pub fn parse_sha256(text: &str) -> Option<String> {
    let is_digest = |word: &str| word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit());
    for line in text.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        if is_digest(first) {
            return Some(first.to_ascii_lowercase());
        }
        // Older certutil prints the digest as byte pairs with spaces.
        let joined: String = line.split_whitespace().collect();
        if is_digest(&joined) {
            return Some(joined.to_ascii_lowercase());
        }
    }
    None
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or("").trim().to_string()
}

pub struct Installed {
    pub command: String,
    pub target: Target,
    pub uploaded: bool,
}

async fn detect_target(connection: &Connection) -> Result<Target> {
    let uname = connection.exec_capture("uname -sm").await?;
    if uname.code == 0 && !uname.stdout.trim().is_empty() {
        return map_uname(&uname.stdout);
    }
    // No uname: Windows OpenSSH, with cmd or PowerShell as the shell.
    let windows = connection
        .exec_capture("cmd /c echo %PROCESSOR_ARCHITECTURE%")
        .await?;
    if windows.code != 0 || windows.stdout.trim().is_empty() {
        bail!("uname -sm failed with code {}", uname.code);
    }
    map_windows(&first_line(&windows.stdout))
}

pub async fn install(connection: &Connection) -> Result<Installed> {
    let target = detect_target(connection).await?;
    let local = local_binary(target)?;
    let bytes = tokio::fs::read(&local)
        .await
        .with_context(|| format!("Could not read {}", local.display()))?;
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    let (remote_path, sftp_path, sftp_dir) = if target.os == Os::Windows {
        let local_app_data = first_line(
            &connection
                .exec_capture("cmd /c echo %LOCALAPPDATA%")
                .await?
                .stdout,
        );
        if local_app_data.is_empty() || local_app_data.contains('%') {
            bail!("Could not resolve remote LOCALAPPDATA");
        }
        let remote = windows_path(&local_app_data, &sha256);
        let sftp = windows_sftp_path(&remote);
        let dir = sftp[..sftp.rfind('/').unwrap_or(0)].to_string();
        (remote, sftp, Some(dir))
    } else {
        let home = first_line(
            &connection
                .exec_capture("printenv HOME || echo \"$HOME\"")
                .await?
                .stdout,
        );
        if home.is_empty() {
            bail!("Could not resolve remote HOME");
        }
        let dir = shell_quote(&format!("{home}/.cache/vitals"));
        connection
            .exec_capture(&format!("mkdir -p {dir} && chmod 700 {dir}"))
            .await?;
        let remote = cache_path(&home, &sha256);
        (remote.clone(), remote, None)
    };
    let command = exec_command(target.os, &remote_path);

    let channel = connection.open_channel().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream()).await?;
    if let Some(dir) = &sftp_dir {
        // An existing folder is the common case; a real failure shows on the upload.
        let _ = sftp.create_dir(dir.clone()).await;
    }

    // The size only spares the hash when the file plainly differs.
    let size = sftp
        .metadata(sftp_path.clone())
        .await
        .ok()
        .and_then(|m| m.size);
    let cached = size == Some(bytes.len() as u64) && {
        let hashed = connection
            .exec_capture(&remote_sha256_command(target.os, &remote_path))
            .await?;
        hashed.code == 0 && parse_sha256(&hashed.stdout).as_deref() == Some(sha256.as_str())
    };
    if !cached {
        let mut suffix = [0u8; 4];
        crate::store::getrandom(&mut suffix);
        let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        let tmp = format!("{sftp_path}.{suffix}.tmp");
        let upload = async {
            let mut file = sftp.create(tmp.clone()).await?;
            file.write_all(&bytes).await?;
            file.shutdown().await?;
            if target.os != Os::Windows {
                let attributes = FileAttributes {
                    permissions: Some(0o700),
                    ..FileAttributes::empty()
                };
                sftp.set_metadata(tmp.clone(), attributes).await?;
            }
            // A plain rename needs a free target.
            let _ = sftp.remove_file(sftp_path.clone()).await;
            sftp.rename(tmp.clone(), sftp_path.clone()).await?;
            anyhow::Ok(())
        };
        if let Err(error) = upload.await {
            let _ = sftp.remove_file(tmp).await;
            let _ = sftp.close().await;
            return Err(error.context("Agent upload failed"));
        }
    }
    let _ = sftp.close().await;
    Ok(Installed {
        command,
        target,
        uploaded: !cached,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            map_uname("Linux x86_64\n").unwrap(),
            Target {
                os: Os::Linux,
                arch: Arch::Amd64
            }
        );
        assert_eq!(map_uname("Darwin arm64").unwrap().os, Os::Darwin);
        assert!(map_uname("Linux riscv64").is_err());
        assert!(map_uname("FreeBSD amd64").is_err());
        assert_eq!(map_windows("arm64").unwrap().arch, Arch::Arm64);
        assert!(map_windows("x86").is_err());
        assert_eq!(
            binary_name(map_windows("AMD64").unwrap()),
            "vitals-agent-windows-amd64.exe"
        );
        assert_eq!(
            binary_name(map_uname("Linux aarch64").unwrap()),
            "vitals-agent-linux-arm64"
        );
    }

    #[test]
    fn paths_and_commands() {
        let sha = "0123456789abcdef".repeat(4);
        assert_eq!(
            cache_path("/home/u/", &sha),
            "/home/u/.cache/vitals/agent-0123456789ab"
        );
        let windows = windows_path("C:\\Users\\u\\AppData\\Local\\", &sha);
        assert_eq!(
            windows,
            "C:\\Users\\u\\AppData\\Local\\Vitals\\agent-0123456789ab.exe"
        );
        assert_eq!(
            windows_sftp_path(&windows),
            "/C:/Users/u/AppData/Local/Vitals/agent-0123456789ab.exe"
        );
        assert_eq!(exec_command(Os::Linux, "/h/it's"), "'/h/it'\\''s'");
        assert_eq!(
            exec_command(Os::Windows, "C:\\a.exe"),
            "cmd /c \"C:\\a.exe\""
        );
    }

    #[test]
    fn digests() {
        let sha = "ab".repeat(32);
        assert_eq!(parse_sha256(&format!("{sha}  /path\n")), Some(sha.clone()));
        let pairs = sha
            .as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).unwrap().to_uppercase())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_sha256(&format!(
                "SHA256 hash of file:\r\n{pairs}\r\nCertUtil: done"
            )),
            Some(sha)
        );
        assert_eq!(parse_sha256("nothing"), None);
    }
}
