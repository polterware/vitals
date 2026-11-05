//! Saved passwords and key passphrases, in the macOS Keychain, so `servers.json` never holds
//! a secret: one generic password per saved server, its account the server's id, under a
//! service named for the data folder (`com.polterware.vitals` for the default one, the
//! folder's path after it for a `VITALS_DATA_DIR`), so a scratch folder never reaches the
//! app's own items. The Electron app sealed its secrets with its own safe storage instead
//! (`secretEnc` in `servers.json`), which this app cannot read.
//!
//! Keychain calls can wait on a system prompt, so the app makes them off the main thread.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::store::{self, AuthMode, Store};

/// The Keychain service of the default data folder: the app's bundle identifier.
pub const SERVICE: &str = "com.polterware.vitals";

/// What the Keychain gave for a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(String),
    /// No secret is saved for it.
    Missing,
    /// The person denied access, or the Keychain could not ask them.
    Denied,
    Failed(String),
}

/// Where saved secrets live: the Keychain, or memory (tests).
#[derive(Clone)]
pub struct Secrets {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Keychain { service: String },
    Memory(Arc<Mutex<Memory>>),
}

#[derive(Default)]
struct Memory {
    items: BTreeMap<String, String>,
    denied: bool,
}

/// The Keychain service for the data folder `dir`.
pub fn service_for(dir: &Path) -> String {
    if dir == store::default_data_dir() {
        SERVICE.to_string()
    } else {
        format!("{SERVICE} ({})", dir.display())
    }
}

impl Secrets {
    /// The secrets of the data folder `dir`: its Keychain service, or memory in unit tests,
    /// which never reach the Keychain.
    pub fn open(dir: &Path) -> Self {
        if cfg!(test) {
            Self::memory()
        } else {
            Self::keychain(service_for(dir))
        }
    }

    pub fn keychain(service: impl Into<String>) -> Self {
        Self {
            backend: Backend::Keychain {
                service: service.into(),
            },
        }
    }

    pub fn memory() -> Self {
        Self {
            backend: Backend::Memory(Arc::default()),
        }
    }

    /// Makes a memory store answer as a Keychain whose prompt was denied.
    #[cfg(test)]
    fn deny_access(&self) {
        if let Backend::Memory(memory) = &self.backend {
            memory.lock().unwrap().denied = true;
        }
    }

    /// The secret saved for the server `id`.
    pub fn get(&self, id: &str) -> Lookup {
        match &self.backend {
            Backend::Keychain { service } => keychain::get(service, id),
            Backend::Memory(memory) => {
                let memory = memory.lock().unwrap();
                if memory.denied {
                    return Lookup::Denied;
                }
                match memory.items.get(id) {
                    Some(secret) => Lookup::Found(secret.clone()),
                    None => Lookup::Missing,
                }
            }
        }
    }

    /// Saves `secret` for the server `id`, over any saved before. `label` names the item in
    /// Keychain Access.
    pub fn set(&self, id: &str, label: &str, secret: &str) -> Result<(), String> {
        match &self.backend {
            Backend::Keychain { service } => keychain::set(service, id, label, secret),
            Backend::Memory(memory) => {
                memory
                    .lock()
                    .unwrap()
                    .items
                    .insert(id.to_string(), secret.to_string());
                Ok(())
            }
        }
    }

    /// Forgets the secret of the server `id`; nothing saved is fine.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        match &self.backend {
            Backend::Keychain { service } => keychain::delete(service, id),
            Backend::Memory(memory) => {
                memory.lock().unwrap().items.remove(id);
                Ok(())
            }
        }
    }

    /// Keeps `secret` for the server `id`, or forgets the saved one when there is none, as
    /// each save of a server replaces its secrets.
    pub fn remember(&self, id: &str, label: &str, secret: Option<&str>) -> Result<(), String> {
        match secret.filter(|secret| !secret.is_empty()) {
            Some(secret) => self.set(id, label, secret),
            None => self.delete(id),
        }
    }

    /// The servers that have a secret saved, by id.
    pub fn saved_ids(&self) -> Result<Vec<String>, String> {
        match &self.backend {
            Backend::Keychain { service } => keychain::accounts(service),
            Backend::Memory(memory) => Ok(memory.lock().unwrap().items.keys().cloned().collect()),
        }
    }

    /// Forgets the secrets of servers `store` no longer lists: its data was cleared, or a
    /// removal never reached the Keychain. The list is read after the Keychain, so a server
    /// saved meanwhile keeps its secret, and an unreadable `servers.json` forgets nothing.
    /// Returns how many it forgot.
    pub fn forget_unlisted(&self, store: &Store) -> Result<usize, String> {
        let ids = self.saved_ids()?;
        if ids.is_empty() {
            return Ok(0);
        }
        let Some(servers) = store.servers_checked() else {
            return Ok(0);
        };
        let mut forgotten = 0;
        for id in ids {
            if !servers.iter().any(|server| server.id == id) {
                self.delete(&id)?;
                forgotten += 1;
            }
        }
        Ok(forgotten)
    }
}

/// What a secret is called for a sign-in method.
pub fn secret_name(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::PrivateKey => "passphrase",
        _ => "password",
    }
}

/// What opening a saved host does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenSaved {
    /// Connect, with the saved secret if there is one.
    Connect(Option<String>),
    /// Open the connect dialog for it, with why when something went wrong.
    Ask(Option<String>),
}

/// What opening a saved host signing in by `mode` does, given what the Keychain gave for it.
/// A password host with no saved password asks for it; a key host tries the key without a
/// passphrase, as it may have none; a denied or failed Keychain asks.
pub fn open_saved(mode: AuthMode, lookup: Lookup) -> OpenSaved {
    let name = secret_name(mode);
    match (mode, lookup) {
        (AuthMode::Agent, _) => OpenSaved::Connect(None),
        (_, Lookup::Found(secret)) => OpenSaved::Connect(Some(secret)),
        (AuthMode::PrivateKey, Lookup::Missing) => OpenSaved::Connect(None),
        (_, Lookup::Missing) => OpenSaved::Ask(None),
        (_, Lookup::Denied) => OpenSaved::Ask(Some(format!(
            "The Keychain did not give Vitals the saved {name}; type it to connect."
        ))),
        (_, Lookup::Failed(error)) => OpenSaved::Ask(Some(format!(
            "Could not read the saved {name} from the Keychain: {error}"
        ))),
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password, generic_password, set_generic_password_options,
    };

    use super::Lookup;

    const ITEM_NOT_FOUND: i32 = -25300;
    const USER_CANCELED: i32 = -128;
    const AUTH_FAILED: i32 = -25293;
    const INTERACTION_NOT_ALLOWED: i32 = -25308;

    pub fn get(service: &str, account: &str) -> Lookup {
        match generic_password(PasswordOptions::new_generic_password(service, account)) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(secret) => Lookup::Found(secret),
                Err(_) => Lookup::Failed("the saved item is not text".into()),
            },
            Err(error) => match error.code() {
                ITEM_NOT_FOUND => Lookup::Missing,
                USER_CANCELED | AUTH_FAILED | INTERACTION_NOT_ALLOWED => Lookup::Denied,
                _ => Lookup::Failed(error.to_string()),
            },
        }
    }

    /// Replaces the item, so its label follows the server's.
    pub fn set(service: &str, account: &str, label: &str, secret: &str) -> Result<(), String> {
        delete(service, account)?;
        let mut options = PasswordOptions::new_generic_password(service, account);
        options.set_label(label);
        set_generic_password_options(secret.as_bytes(), options).map_err(|error| error.to_string())
    }

    pub fn delete(service: &str, account: &str) -> Result<(), String> {
        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn accounts(service: &str) -> Result<Vec<String>, String> {
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(service)
            .load_attributes(true)
            .limit(Limit::All)
            .search();
        match found {
            Ok(items) => Ok(items
                .iter()
                .filter_map(|item| item.simplify_dict()?.remove("acct"))
                .collect()),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(Vec::new()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// No Keychain off macOS: nothing is saved, so every saved host asks.
#[cfg(not(target_os = "macos"))]
mod keychain {
    use super::Lookup;

    pub fn get(_service: &str, _account: &str) -> Lookup {
        Lookup::Missing
    }

    pub fn set(_service: &str, _account: &str, _label: &str, _secret: &str) -> Result<(), String> {
        Err("there is no Keychain on this system".into())
    }

    pub fn delete(_service: &str, _account: &str) -> Result<(), String> {
        Ok(())
    }

    pub fn accounts(_service: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Server;

    fn scratch_store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("vitals-secrets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    fn password_host() -> Server {
        Server {
            label: "Box".into(),
            host: "10.0.0.2".into(),
            port: 22,
            username: "root".into(),
            auth_mode: AuthMode::Password,
            ..Default::default()
        }
    }

    #[test]
    fn a_saved_password_reopens_without_asking_and_never_reaches_the_file() {
        let store = scratch_store("save");
        let secrets = Secrets::memory();
        let password = "correct horse battery staple";
        let id = store.upsert_server(password_host()).unwrap();
        secrets
            .remember(&id, "Vitals: root@10.0.0.2:22", Some(password))
            .unwrap();

        let file = std::fs::read_to_string(store.dir().join("servers.json")).unwrap();
        assert!(
            !file.contains(password),
            "servers.json holds the password: {file}"
        );
        let record: serde_json::Value = serde_json::from_str(&file).unwrap();
        let mut fields: Vec<_> = record["servers"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        fields.sort();
        assert_eq!(
            fields,
            ["authMode", "host", "id", "label", "port", "username"],
            "servers.json has a field beyond the public ones"
        );

        let server = &store.servers()[0];
        assert_eq!(
            open_saved(server.auth_mode, secrets.get(&server.id)),
            OpenSaved::Connect(Some(password.to_string()))
        );
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_missing_or_denied_secret_asks() {
        let secrets = Secrets::memory();
        assert_eq!(secrets.get("gone"), Lookup::Missing);
        assert_eq!(
            open_saved(AuthMode::Password, secrets.get("gone")),
            OpenSaved::Ask(None)
        );
        // A key may have no passphrase: it is tried as it is.
        assert_eq!(
            open_saved(AuthMode::PrivateKey, secrets.get("gone")),
            OpenSaved::Connect(None)
        );
        assert_eq!(
            open_saved(AuthMode::Agent, Lookup::Denied),
            OpenSaved::Connect(None)
        );

        secrets.set("a", "Vitals: a", "secret").unwrap();
        secrets.deny_access();
        assert_eq!(secrets.get("a"), Lookup::Denied);
        for mode in [AuthMode::Password, AuthMode::PrivateKey] {
            assert!(matches!(
                open_saved(mode, secrets.get("a")),
                OpenSaved::Ask(Some(_))
            ));
        }
        assert!(matches!(
            open_saved(AuthMode::Password, Lookup::Failed("locked".into())),
            OpenSaved::Ask(Some(reason)) if reason.contains("locked")
        ));
    }

    #[test]
    fn saving_again_replaces_or_forgets_the_secret() {
        let secrets = Secrets::memory();
        secrets.remember("a", "Vitals: a", Some("one")).unwrap();
        secrets.remember("a", "Vitals: a", Some("two")).unwrap();
        assert_eq!(secrets.get("a"), Lookup::Found("two".into()));
        // A host saved again with no secret (an agent, or a key with no passphrase).
        secrets.remember("a", "Vitals: a", None).unwrap();
        assert_eq!(secrets.get("a"), Lookup::Missing);
        secrets.remember("a", "Vitals: a", Some("")).unwrap();
        assert_eq!(secrets.get("a"), Lookup::Missing);
    }

    #[test]
    fn cleared_data_forgets_its_secrets() {
        let store = scratch_store("prune");
        let secrets = Secrets::memory();
        let kept = store.upsert_server(password_host()).unwrap();
        secrets.set(&kept, "Vitals: kept", "one").unwrap();
        secrets
            .set("removed-elsewhere", "Vitals: old", "two")
            .unwrap();
        assert_eq!(secrets.forget_unlisted(&store), Ok(1));
        assert_eq!(secrets.saved_ids().unwrap(), vec![kept.clone()]);

        // An unreadable list forgets nothing.
        std::fs::write(store.dir().join("servers.json"), "{ not json").unwrap();
        assert_eq!(secrets.forget_unlisted(&store), Ok(0));
        assert_eq!(secrets.get(&kept), Lookup::Found("one".into()));

        // A cleared data folder forgets everything it had.
        std::fs::remove_dir_all(store.dir()).unwrap();
        assert_eq!(secrets.forget_unlisted(&store), Ok(1));
        assert!(secrets.saved_ids().unwrap().is_empty());
    }

    #[test]
    fn a_scratch_folder_has_its_own_service() {
        assert_eq!(service_for(&store::default_data_dir()), SERVICE);
        let scratch = std::env::temp_dir().join("vitals-scratch");
        assert_eq!(
            service_for(&scratch),
            format!("{SERVICE} ({})", scratch.display())
        );
    }

    /// The real Keychain, under a throwaway service it removes again:
    /// `cargo test -p vitals-app keychain_round_trip -- --ignored`.
    #[test]
    #[ignore = "writes to the login Keychain"]
    fn keychain_round_trip() {
        let secrets = Secrets::keychain(format!("{SERVICE}.test.{}", std::process::id()));
        let id = "00000000-0000-4000-8000-000000000000";
        assert_eq!(secrets.get(id), Lookup::Missing);
        secrets.set(id, "Vitals test item", "first").unwrap();
        secrets
            .set(id, "Vitals test item, renamed", "second")
            .unwrap();
        assert_eq!(secrets.get(id), Lookup::Found("second".into()));
        assert_eq!(secrets.saved_ids().unwrap(), vec![id.to_string()]);
        secrets.delete(id).unwrap();
        assert_eq!(secrets.get(id), Lookup::Missing);
        assert!(secrets.saved_ids().unwrap().is_empty());
        secrets.delete(id).unwrap();
    }
}
