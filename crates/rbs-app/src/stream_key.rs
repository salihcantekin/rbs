//! Where the stream key is kept: the operating system's credential store (the macOS
//! Keychain, the Windows Credential Manager), never a settings file, log or URL. Where
//! RBS has no credential store yet (Linux today) the key is kept in memory until the app
//! closes. The studio's control thread reads it only when a stream starts and writes it
//! only when the user saves or removes it, so nothing touches the store at start-up and
//! an OS prompt to allow access never blocks the window.

use std::fmt;
use std::sync::{Arc, Mutex};

use rbs_transport::Secret;

/// The credential store's service name.
pub const SERVICE: &str = "RBS";
/// The credential store's account name for the (one) stream key.
pub const ACCOUNT: &str = "stream key";

/// Keeps one stream key.
pub trait KeyStore: Send + Sync + fmt::Debug {
    /// Where keys are kept, for the Stream section's hint ("the macOS Keychain").
    fn place(&self) -> &'static str;
    /// A saved key is still there after a restart.
    fn persistent(&self) -> bool;
    /// The saved key, `None` when there is none. May block (an OS prompt).
    fn read(&self) -> Result<Option<Secret>, String>;
    /// Saves `key`, replacing any saved one.
    fn write(&self, key: &Secret) -> Result<(), String>;
    /// Removes the saved key; removing none is not an error.
    fn remove(&self) -> Result<(), String>;
}

/// A key kept in memory until the app closes: where there is no credential store, and
/// in tests.
#[derive(Default)]
pub struct MemoryKeyStore {
    key: Mutex<Option<Secret>>,
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
    fn read(&self) -> Result<Option<Secret>, String> {
        Ok(self.key.lock().map_err(|_| "the key store broke")?.clone())
    }
    fn write(&self, key: &Secret) -> Result<(), String> {
        *self.key.lock().map_err(|_| "the key store broke")? = Some(key.clone());
        Ok(())
    }
    fn remove(&self) -> Result<(), String> {
        *self.key.lock().map_err(|_| "the key store broke")? = None;
        Ok(())
    }
}

/// The system credential store, through the `keyring` crate.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
pub struct SystemKeyStore;

#[cfg(any(target_os = "macos", windows))]
impl SystemKeyStore {
    fn entry() -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())
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
    fn read(&self) -> Result<Option<Secret>, String> {
        match Self::entry()?.get_password() {
            Ok(k) => Ok(Some(Secret::new(k))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn write(&self, key: &Secret) -> Result<(), String> {
        Self::entry()?
            .set_password(key.expose())
            .map_err(|e| e.to_string())
    }
    fn remove(&self) -> Result<(), String> {
        match Self::entry()?.delete_credential() {
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
    fn the_memory_store_keeps_one_key() {
        let s = MemoryKeyStore::new();
        assert!(s.read().unwrap().is_none());
        s.write(&Secret::new("abc")).unwrap();
        assert_eq!(s.read().unwrap().unwrap().expose(), "abc");
        s.write(&Secret::new("def")).unwrap();
        assert_eq!(s.read().unwrap().unwrap().expose(), "def");
        s.remove().unwrap();
        s.remove().unwrap();
        assert!(s.read().unwrap().is_none());
        assert!(!s.persistent());
        assert!(hint(&s).contains("until RBS closes"));
        assert_eq!(format!("{s:?}"), "MemoryKeyStore", "no key in debug output");
    }
}
