//! Where the stream key is kept: the operating system's credential store (the macOS
//! Keychain, the Windows Credential Manager), never a settings file, log or URL. Where
//! RBS has no credential store yet (Linux today) the key is kept in memory until the app
//! closes. The studio's control thread reads it only when a stream starts and writes it
//! only when the user saves or removes it, so nothing touches the store at start-up and
//! an OS prompt to allow access never blocks the window.
//!
//! Each output profile has its own key, kept under an account named after the
//! profile's id ([`account`], ADR-0051 decision 5). The account RBS used before
//! profiles existed belongs to the `default` profile, so a key saved then keeps working.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use rbs_transport::Secret;

/// The credential store's service name.
pub const SERVICE: &str = "RBS";
/// The credential store's account name of the stream key saved before output profiles
/// existed; it is the `default` profile's.
pub const ACCOUNT: &str = "stream key";

/// The credential store's account for the stream key of the output profile with id
/// `profile` (an `rbs_user_data::EntryId`): `stream-key/<id>`, or [`ACCOUNT`] for
/// `default`. Ids are never reused, so a key never passes to another profile.
pub fn account(profile: &str) -> String {
    if profile == "default" {
        ACCOUNT.to_owned()
    } else {
        format!("stream-key/{profile}")
    }
}

/// Keeps stream keys, one per account ([`account`]).
pub trait KeyStore: Send + Sync + fmt::Debug {
    /// Where keys are kept, for the Stream section's hint ("the macOS Keychain").
    fn place(&self) -> &'static str;
    /// A saved key is still there after a restart.
    fn persistent(&self) -> bool;
    /// The key saved for `account`, `None` when there is none. May block (an OS
    /// prompt).
    fn read(&self, account: &str) -> Result<Option<Secret>, String>;
    /// Saves `key` for `account`, replacing any saved one.
    fn write(&self, account: &str, key: &Secret) -> Result<(), String>;
    /// Removes the key saved for `account`; removing none is not an error.
    fn remove(&self, account: &str) -> Result<(), String>;
}

/// Keys kept in memory until the app closes: where there is no credential store, and
/// in tests.
#[derive(Default)]
pub struct MemoryKeyStore {
    keys: Mutex<HashMap<String, Secret>>,
}

impl fmt::Debug for MemoryKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MemoryKeyStore")
    }
}

impl MemoryKeyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for MemoryKeyStore {
    fn place(&self) -> &'static str {
        "memory only (this system has no credential store RBS can use yet)"
    }
    fn persistent(&self) -> bool {
        false
    }
    fn read(&self, account: &str) -> Result<Option<Secret>, String> {
        let keys = self.keys.lock().map_err(|_| "the key store broke")?;
        Ok(keys.get(account).cloned())
    }
    fn write(&self, account: &str, key: &Secret) -> Result<(), String> {
        let mut keys = self.keys.lock().map_err(|_| "the key store broke")?;
        keys.insert(account.to_owned(), key.clone());
        Ok(())
    }
    fn remove(&self, account: &str) -> Result<(), String> {
        let mut keys = self.keys.lock().map_err(|_| "the key store broke")?;
        keys.remove(account);
        Ok(())
    }
}

/// The system credential store, through the `keyring` crate.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
pub struct SystemKeyStore;

#[cfg(any(target_os = "macos", windows))]
impl SystemKeyStore {
    fn entry(account: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, account).map_err(|e| e.to_string())
    }
}

#[cfg(any(target_os = "macos", windows))]
impl KeyStore for SystemKeyStore {
    fn place(&self) -> &'static str {
        if cfg!(target_os = "macos") {
            "the macOS Keychain"
        } else {
            "the Windows Credential Manager"
        }
    }
    fn persistent(&self) -> bool {
        true
    }
    fn read(&self, account: &str) -> Result<Option<Secret>, String> {
        match Self::entry(account)?.get_password() {
            Ok(k) => Ok(Some(Secret::new(k))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn write(&self, account: &str, key: &Secret) -> Result<(), String> {
        Self::entry(account)?
            .set_password(key.expose())
            .map_err(|e| e.to_string())
    }
    fn remove(&self, account: &str) -> Result<(), String> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The system's credential store, or memory where RBS has none.
pub fn system() -> Arc<dyn KeyStore> {
    #[cfg(any(target_os = "macos", windows))]
    {
        Arc::new(SystemKeyStore)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Arc::new(MemoryKeyStore::new())
    }
}

/// The hint under the Stream key field.
pub fn hint(store: &dyn KeyStore) -> String {
    if store.persistent() {
        format!(
            "Kept in {}, never in a settings file. With SRT, this is the stream ID (optional).",
            store.place()
        )
    } else {
        "Kept in memory until RBS closes: this system has no credential store RBS can use yet. With SRT, this is the stream ID (optional).".to_owned()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_keeps_one_key_per_account() {
        let s = MemoryKeyStore::new();
        assert!(s.read(ACCOUNT).unwrap().is_none());
        s.write(ACCOUNT, &Secret::new("abc")).unwrap();
        assert_eq!(s.read(ACCOUNT).unwrap().unwrap().expose(), "abc");
        s.write(ACCOUNT, &Secret::new("def")).unwrap();
        assert_eq!(s.read(ACCOUNT).unwrap().unwrap().expose(), "def");
        let other = account("studio-b");
        assert!(s.read(&other).unwrap().is_none());
        s.write(&other, &Secret::new("ghi")).unwrap();
        s.remove(ACCOUNT).unwrap();
        s.remove(ACCOUNT).unwrap();
        assert!(s.read(ACCOUNT).unwrap().is_none());
        assert_eq!(s.read(&other).unwrap().unwrap().expose(), "ghi");
        assert!(!s.persistent());
        assert!(hint(&s).contains("until RBS closes"));
        assert_eq!(format!("{s:?}"), "MemoryKeyStore", "no key in debug output");
    }

    #[test]
    fn the_default_profile_keeps_the_key_saved_before_profiles() {
        assert_eq!(account("default"), ACCOUNT);
        assert_eq!(account("studio-b"), "stream-key/studio-b");
        assert_ne!(account("default-2"), ACCOUNT);
    }
}
