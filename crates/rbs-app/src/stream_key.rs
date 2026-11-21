//! The stream key in the operating system's credential store: the studio host's part
//! (`rbs_studio_host::stream_key`: the key store trait, the memory store, the account
//! names) and the system store, which stays in this composition root (ADR-0159
//! decision 2).

use std::sync::Arc;

pub use rbs_studio_host::stream_key::*;
#[cfg(any(target_os = "macos", windows))]
use rbs_transport::Secret;

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
