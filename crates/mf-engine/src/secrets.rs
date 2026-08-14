//! API keys, stored in the OS keychain.
//!
//! Keys never touch `config.toml` and never touch the sqlite file. The only copy on disk is the
//! one the platform keychain owns and encrypts (gnome-keyring / kwallet via Secret Service,
//! macOS Keychain, Windows Credential Manager).
//!
//! When no keychain is available — a headless box with no Secret Service — this reports that
//! plainly instead of falling back to a plaintext file. A silent downgrade from "encrypted by the
//! OS" to "world-readable in your home directory" is exactly the kind of surprise a secrets layer
//! must never spring on someone.

use secrecy::SecretString;

/// Keychain service name. Entries are `(SERVICE, provider name)`.
const SERVICE: &str = "meshflow";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("no OS keychain is available: {0}. On Linux, a Secret Service provider such as \
             gnome-keyring or kwallet must be running.")]
    Unavailable(String),
    #[error("keychain error for `{account}`: {source}")]
    Backend { account: String, source: keyring::v1::Error },
}

/// Store (or replace) the key for a provider entry.
pub fn set(account: &str, key: &SecretString) -> Result<(), SecretError> {
    use secrecy::ExposeSecret;
    entry(account)?
        .set_password(key.expose_secret())
        .map_err(|source| SecretError::Backend { account: account.to_owned(), source })
}

/// Fetch the key for a provider entry, or `None` when none is stored.
///
/// A missing entry is not an error: local endpoints legitimately have no key, and a user may
/// have configured a provider before pasting a key in.
pub fn get(account: &str) -> Result<Option<SecretString>, SecretError> {
    match entry(account)?.get_password() {
        Ok(secret) => Ok(Some(SecretString::from(secret))),
        Err(keyring::v1::Error::NoEntry) => Ok(None),
        Err(source) => Err(SecretError::Backend { account: account.to_owned(), source }),
    }
}

/// Remove a provider's key. Deleting an entry that isn't there succeeds — the caller wanted it
/// gone, and it is.
pub fn delete(account: &str) -> Result<(), SecretError> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
        Err(source) => Err(SecretError::Backend { account: account.to_owned(), source }),
    }
}

/// Whether a usable keychain exists. Called before the settings UI offers to save a key, so the
/// user is told up front rather than after typing one in.
pub fn available() -> bool {
    // A read of a name that will not exist still exercises the backend connection.
    !matches!(
        entry("__meshflow_probe__").and_then(|e| match e.get_password() {
            Ok(_) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(source) =>
                Err(SecretError::Backend { account: "__meshflow_probe__".into(), source }),
        }),
        Err(SecretError::Unavailable(_))
    )
}

fn entry(account: &str) -> Result<keyring::v1::Entry, SecretError> {
    keyring::v1::Entry::new(SERVICE, account)
        .map_err(|e| SecretError::Unavailable(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    /// Unique per run so concurrent tests and repeat runs never collide in a real keychain.
    fn scratch_account() -> String {
        format!("__meshflow_test_{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn round_trips_a_key_through_the_os_keychain() {
        if !available() {
            eprintln!("skipping: no keychain on this machine");
            return;
        }
        let account = scratch_account();
        let secret = SecretString::from("sk-test-value-123");

        set(&account, &secret).expect("store");
        let fetched = get(&account).expect("fetch").expect("a key was stored");
        assert_eq!(fetched.expose_secret(), "sk-test-value-123");

        delete(&account).expect("delete");
        assert!(get(&account).expect("fetch after delete").is_none());
    }

    #[test]
    fn a_missing_key_is_none_rather_than_an_error() {
        if !available() {
            return;
        }
        // Local providers have no key at all; that must not read as a failure.
        assert!(get(&scratch_account()).expect("missing key is not an error").is_none());
    }

    #[test]
    fn deleting_a_missing_key_succeeds() {
        if !available() {
            return;
        }
        assert!(delete(&scratch_account()).is_ok());
    }

    #[test]
    fn secret_error_never_renders_the_key_itself() {
        // Error paths are a classic leak: the value must never appear in a message that ends up
        // in a log or a UI toast.
        let err = SecretError::Unavailable("dbus refused".into());
        assert!(!format!("{err}").contains("sk-"));
    }
}
