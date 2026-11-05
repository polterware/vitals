//! What Vitals keeps on this computer, in the files the Electron app wrote and reads:
//! `servers.json` (saved hosts), `known_hosts.json` (approved host keys, by `host:port`), and
//! `settings.json` ([`crate::settings`]), all in `~/Library/Application Support/Vitals` unless `VITALS_DATA_DIR`
//! names another folder.
//!
//! No secret is written here: saved passwords and passphrases go to the macOS Keychain
//! ([`crate::secrets`]). The Electron app sealed its own with its safe storage (`secretEnc`),
//! which this app neither reads nor writes, only keeps.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VITALS_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // Unit tests never reach the real folder.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("vitals-test-data-{}", std::process::id()));
    }
    default_data_dir()
}

/// `~/Library/Application Support/Vitals`, where the Electron app kept its files too.
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Application Support/Vitals")
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AuthMode {
    #[default]
    Agent,
    PrivateKey,
    Password,
}

impl AuthMode {
    pub fn label(self) -> &'static str {
        match self {
            AuthMode::Agent => "SSH agent",
            AuthMode::PrivateKey => "Private key",
            AuthMode::Password => "Password",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_mode: AuthMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    /// The Electron app's sealed secrets, kept as they are when the file is written again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_enc: Option<String>,
}

impl Server {
    /// `user@host:port`, which tells hosts apart.
    pub fn key(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    pub fn display_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.host
        } else {
            &self.label
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct ServerFile {
    #[serde(default)]
    servers: Vec<Server>,
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn open() -> Self {
        Self::new(data_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn servers_path(&self) -> PathBuf {
        self.dir.join("servers.json")
    }

    fn known_hosts_path(&self) -> PathBuf {
        self.dir.join("known_hosts.json")
    }

    pub fn servers(&self) -> Vec<Server> {
        read_json::<ServerFile>(&self.servers_path())
            .map(|file| file.servers)
            .unwrap_or_default()
    }

    /// The saved hosts, none when `servers.json` is missing, and `None` when it is there but
    /// cannot be read, so a caller never takes a broken file for an empty list.
    pub fn servers_checked(&self) -> Option<Vec<Server>> {
        let path = self.servers_path();
        if !path.exists() {
            return Some(Vec::new());
        }
        read_json::<ServerFile>(&path).map(|file| file.servers)
    }

    /// Saves `server` over the one with its id, or the one for the same `user@host:port`,
    /// or as a new one; returns its id.
    pub fn upsert_server(&self, mut server: Server) -> std::io::Result<String> {
        let mut servers = self.servers();
        let existing = servers.iter().position(|saved| {
            (!server.id.is_empty() && saved.id == server.id) || saved.key() == server.key()
        });
        match existing {
            Some(index) => {
                server.id = servers[index].id.clone();
                if server.secret_enc.is_none() {
                    server.secret_enc = servers[index].secret_enc.clone();
                }
                servers[index] = server.clone();
            }
            None => {
                if server.id.is_empty() {
                    server.id = new_id();
                }
                servers.push(server.clone());
            }
        }
        write_json(&self.servers_path(), &ServerFile { servers })?;
        Ok(server.id)
    }

    pub fn remove_server(&self, id: &str) -> std::io::Result<()> {
        let mut servers = self.servers();
        servers.retain(|server| server.id != id);
        write_json(&self.servers_path(), &ServerFile { servers })
    }

    fn known_hosts(&self) -> BTreeMap<String, KnownHost> {
        read_json(&self.known_hosts_path()).unwrap_or_default()
    }

    pub fn verify_host_key(&self, host: &str, port: u16, fingerprint: &str) -> HostKeyVerdict {
        match self.known_hosts().get(&host_key(host, port)) {
            None => HostKeyVerdict::New,
            Some(known) if known.fingerprint == fingerprint => HostKeyVerdict::Known,
            Some(_) => HostKeyVerdict::Changed,
        }
    }

    pub fn approve_host_key(
        &self,
        host: &str,
        port: u16,
        fingerprint: &str,
    ) -> std::io::Result<()> {
        let mut hosts = self.known_hosts();
        hosts.insert(
            host_key(host, port),
            KnownHost {
                fingerprint: fingerprint.to_string(),
            },
        );
        write_json(&self.known_hosts_path(), &hosts)
    }

    /// The approved host keys: `host:port` and its fingerprint, by host.
    pub fn known_host_list(&self) -> Vec<(String, String)> {
        self.known_hosts()
            .into_iter()
            .map(|(key, known)| (key, known.fingerprint))
            .collect()
    }

    /// Forgets the key of `key` (`host:port`), so the next connection asks for it again.
    pub fn remove_known_host(&self, key: &str) -> std::io::Result<()> {
        let mut hosts = self.known_hosts();
        if hosts.remove(key).is_some() {
            write_json(&self.known_hosts_path(), &hosts)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct KnownHost {
    fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyVerdict {
    New,
    Known,
    Changed,
}

fn host_key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes through a temporary file in the same folder, so a crash never leaves half a file.
fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// A random id in the UUID text form the Electron app's `randomUUID` writes.
fn new_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn getrandom(bytes: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut file) = fs::File::open("/dev/urandom")
        && file.read_exact(bytes).is_ok()
    {
        return;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (seed >> ((index % 16) * 8)) as u8 ^ index as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("vitals-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    #[test]
    fn reads_the_electron_files() {
        let store = temp_store("read");
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.dir().join("servers.json"),
            r#"{"servers":[{"id":"a","label":"Box","host":"10.0.0.2","port":22,"username":"root","authMode":"privateKey","privateKeyPath":"/k","secretEnc":"sealed"}]}"#,
        )
        .unwrap();
        fs::write(
            store.dir().join("known_hosts.json"),
            r#"{"10.0.0.2:22":{"fingerprint":"SHA256:abc="}}"#,
        )
        .unwrap();
        let servers = store.servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].auth_mode, AuthMode::PrivateKey);
        assert_eq!(servers[0].key(), "root@10.0.0.2:22");
        assert_eq!(
            store.verify_host_key("10.0.0.2", 22, "SHA256:abc="),
            HostKeyVerdict::Known
        );
        assert_eq!(
            store.verify_host_key("10.0.0.2", 22, "SHA256:other="),
            HostKeyVerdict::Changed
        );
        assert_eq!(
            store.verify_host_key("10.0.0.3", 22, "x"),
            HostKeyVerdict::New
        );
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn upsert_keeps_ids_and_sealed_secrets() {
        let store = temp_store("upsert");
        let first = Server {
            label: "Box".into(),
            host: "h".into(),
            port: 22,
            username: "u".into(),
            secret_enc: Some("sealed".into()),
            ..Default::default()
        };
        let id = store.upsert_server(first.clone()).unwrap();
        assert_eq!(id.len(), 36);
        let again = store
            .upsert_server(Server {
                label: "Renamed".into(),
                secret_enc: None,
                ..first
            })
            .unwrap();
        assert_eq!(again, id);
        let servers = store.servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].label, "Renamed");
        assert_eq!(servers[0].secret_enc.as_deref(), Some("sealed"));
        store.remove_server(&id).unwrap();
        assert!(store.servers().is_empty());
        store.approve_host_key("h", 2222, "SHA256:x=").unwrap();
        assert_eq!(
            store.verify_host_key("h", 2222, "SHA256:x="),
            HostKeyVerdict::Known
        );
        assert_eq!(
            store.known_host_list(),
            vec![("h:2222".to_string(), "SHA256:x=".to_string())]
        );
        store.remove_known_host("h:2222").unwrap();
        assert_eq!(
            store.verify_host_key("h", 2222, "SHA256:x="),
            HostKeyVerdict::New
        );
        let _ = fs::remove_dir_all(store.dir());
    }
}
