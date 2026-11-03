//! One SSH connection to a host: the host key checked against `known_hosts.json`, then the
//! sign-in by the SSH agent, a private key, or a password, as the Electron app's `ssh2`
//! client did.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use russh::client::{self, Handle, Msg};
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, Disconnect};
use sha2::{Digest, Sha256};

use crate::store::{AuthMode, HostKeyVerdict, Store};

#[derive(Clone, Debug, Default)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_mode: AuthMode,
    pub private_key_path: Option<String>,
    /// The password, or the private key's passphrase.
    pub secret: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectError {
    /// The host's key is not known yet; the fingerprint waits for approval.
    NewHostKey(String),
    /// The host's key is not the one approved before.
    ChangedHostKey,
    Auth(String),
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NewHostKey(_) => f.write_str("Waiting for host key approval"),
            ConnectError::ChangedHostKey => f.write_str("Host key changed — connection refused"),
            ConnectError::Auth(message) | ConnectError::Other(message) => f.write_str(message),
        }
    }
}

/// `SHA256:` and the standard, padded Base64 of the key blob's digest: the form the
/// Electron app stored in `known_hosts.json`.
pub fn fingerprint(key: &PublicKey) -> String {
    let blob = key.to_bytes().unwrap_or_default();
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(blob))
    )
}

struct Verifier {
    store: Store,
    host: String,
    port: u16,
    approved: Option<String>,
    issue: Arc<Mutex<Option<ConnectError>>>,
}

impl client::Handler for Verifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(certificate) => {
                PublicKey::new(certificate.public_key().clone(), "")
            }
        };
        let print = fingerprint(&key);
        let issue = match self.store.verify_host_key(&self.host, self.port, &print) {
            HostKeyVerdict::Known => return Ok(true),
            HostKeyVerdict::Changed => ConnectError::ChangedHostKey,
            HostKeyVerdict::New if self.approved.as_deref() == Some(print.as_str()) => {
                return Ok(true);
            }
            HostKeyVerdict::New => ConnectError::NewHostKey(print),
        };
        *self.issue.lock().expect("host key issue") = Some(issue);
        Ok(false)
    }
}

pub struct Captured {
    pub stdout: String,
    pub code: u32,
}

pub struct Connection {
    handle: Handle<Verifier>,
}

impl Connection {
    /// Connects and signs in. `approved` is a fingerprint approved in this session that
    /// `known_hosts.json` may not hold yet.
    pub async fn open(
        target: &Target,
        store: Store,
        approved: Option<String>,
    ) -> Result<Connection, ConnectError> {
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(Duration::from_secs(10)),
            keepalive_max: 3,
            ..Default::default()
        });
        let issue = Arc::new(Mutex::new(None));
        let verifier = Verifier {
            store,
            host: target.host.clone(),
            port: target.port,
            approved,
            issue: issue.clone(),
        };
        let address = (target.host.clone(), target.port);
        let connect = client::connect(config, address, verifier);
        let mut handle = match tokio::time::timeout(Duration::from_secs(20), connect).await {
            Err(_) => return Err(ConnectError::Other("Timed out while connecting".into())),
            Ok(Err(error)) => {
                if let Some(issue) = issue.lock().expect("host key issue").take() {
                    return Err(issue);
                }
                return Err(ConnectError::Other(error.to_string()));
            }
            Ok(Ok(handle)) => handle,
        };
        authenticate(&mut handle, target)
            .await
            .map_err(|error| ConnectError::Auth(error.to_string()))?;
        Ok(Connection { handle })
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    pub async fn open_channel(&self) -> Result<Channel<Msg>> {
        Ok(self.handle.channel_open_session().await?)
    }

    /// Runs `command` and keeps its stdout and exit code; stderr is dropped.
    pub async fn exec_capture(&self, command: &str) -> Result<Captured> {
        let mut channel = self.open_channel().await?;
        channel.exec(true, command).await?;
        let mut stdout = Vec::new();
        let mut code = 0;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = exit_status,
                _ => {}
            }
        }
        Ok(Captured {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            code,
        })
    }

    pub async fn close(&self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await;
    }
}

async fn rsa_hash(
    handle: &Handle<Verifier>,
    key: &PublicKey,
) -> Result<Option<russh::keys::HashAlg>> {
    if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
        Ok(handle.best_supported_rsa_hash().await?.flatten())
    } else {
        Ok(None)
    }
}

async fn authenticate(handle: &mut Handle<Verifier>, target: &Target) -> Result<()> {
    let user = target.username.clone();
    match target.auth_mode {
        AuthMode::Agent => {
            let mut agent = AgentClient::connect_env()
                .await
                .map_err(|_| anyhow!("No SSH agent found: SSH_AUTH_SOCK is not set"))?;
            let identities = agent.request_identities().await?;
            if identities.is_empty() {
                return Err(anyhow!("The SSH agent holds no keys"));
            }
            for identity in identities {
                let key = identity.public_key().into_owned();
                let hash = rsa_hash(handle, &key).await?;
                let result = handle
                    .authenticate_publickey_with(user.clone(), key, hash, &mut agent)
                    .await;
                if matches!(result, Ok(outcome) if outcome.success()) {
                    return Ok(());
                }
            }
            Err(anyhow!("Authentication failed: no agent key was accepted"))
        }
        AuthMode::PrivateKey => {
            let path = target
                .private_key_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .ok_or_else(|| anyhow!("Private key path is required"))?;
            let path = expand_home(path);
            let key = russh::keys::load_secret_key(&path, target.secret.as_deref())
                .map_err(|error| anyhow!("Could not read the private key {path}: {error}"))?;
            let hash = rsa_hash(handle, key.public_key()).await?;
            let outcome = handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?;
            if outcome.success() {
                Ok(())
            } else {
                Err(anyhow!("Authentication failed: the key was not accepted"))
            }
        }
        AuthMode::Password => {
            let password = target.secret.clone().unwrap_or_default();
            if password.is_empty() {
                return Err(anyhow!("Password is required"));
            }
            let outcome = handle.authenticate_password(user, password).await?;
            if outcome.success() {
                Ok(())
            } else {
                Err(anyhow!("Authentication failed: wrong user or password"))
            }
        }
    }
}

pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_padded_base64() {
        let key = PublicKey::from_openssh(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHkVTSkQ6dsIQFh2EZjcxj+ty6+1HCeLHc/5eu+Q+pOY test",
        )
        .unwrap();
        let print = fingerprint(&key);
        assert!(print.starts_with("SHA256:"));
        assert!(print.ends_with('='), "{print}");
        assert_eq!(print.len(), "SHA256:".len() + 44);
        // OpenSSH prints the same digest without the padding.
        let openssh = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        assert_eq!(print.trim_end_matches('='), openssh);
    }

    #[test]
    fn home_expands() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/.ssh/id"), format!("{home}/.ssh/id"));
        assert_eq!(expand_home("/k"), "/k");
    }
}
