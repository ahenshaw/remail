//! OS keyring access for the two kinds of secret an account can hold:
//! an IMAP/SMTP password, or an OAuth2 refresh token.
//!
//! Nothing here is cached in memory beyond the lifetime of a call, and nothing
//! is written to the config file.

use anyhow::Result;

use crate::config::AccountId;

const SERVICE: &str = "remail";

fn entry(kind: &str, account: AccountId) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(SERVICE, &format!("{kind}:{account}"))?)
}

/// Stores a secret, replacing any existing one.
pub fn set(kind: SecretKind, account: AccountId, value: &str) -> Result<()> {
    entry(kind.as_str(), account)?.set_password(value)?;
    Ok(())
}

/// Reads a secret. Returns `Ok(None)` when no entry exists, which is the normal
/// state for an account that has not been authenticated yet.
pub fn get(kind: SecretKind, account: AccountId) -> Result<Option<String>> {
    match entry(kind.as_str(), account)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete(kind: SecretKind, account: AccountId) -> Result<()> {
    match entry(kind.as_str(), account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Removes every secret belonging to an account.
pub fn delete_all(account: AccountId) {
    for kind in [SecretKind::Password, SecretKind::RefreshToken] {
        let _ = delete(kind, account);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Password,
    RefreshToken,
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::RefreshToken => "refresh-token",
        }
    }
}

/// Whether the platform keyring is usable at all. On headless Linux there may
/// be no Secret Service, in which case the UI explains the failure rather than
/// silently losing credentials.
pub fn available() -> bool {
    keyring::Entry::new(SERVICE, "probe").is_ok()
}
