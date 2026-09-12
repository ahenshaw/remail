//! Authentication: OAuth2 token handling and the credential lookup the mail
//! engine performs before each connection.

pub mod oauth;

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};

use crate::config::{AccountConfig, AccountId, AuthMethod};
use crate::secrets::{self, SecretKind};

pub use oauth::{ClientCredentials, TokenSet};

/// What a connection needs to log in.
#[derive(Debug, Clone)]
pub enum Credential {
    /// `LOGIN`/`PLAIN` with this password.
    Password(String),
    /// `XOAUTH2` with this bearer token.
    Bearer(String),
}

/// Caches access tokens so every connection does not trigger a refresh.
/// Refresh tokens themselves stay in the OS keyring.
#[derive(Default)]
pub struct TokenStore {
    cached: Mutex<HashMap<AccountId, TokenSet>>,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves the credential for an account, refreshing an expired OAuth
    /// access token on the way if necessary.
    pub async fn credential(&self, account: &AccountConfig) -> Result<Credential> {
        match account.auth {
            AuthMethod::Password => {
                match secrets::get(SecretKind::Password, account.id)? {
                    Some(password) => Ok(Credential::Password(password)),
                    None => bail!("no password saved for {}", account.email),
                }
            }
            AuthMethod::OAuth2 => Ok(Credential::Bearer(self.access_token(account).await?)),
        }
    }

    /// Returns a usable access token, refreshing if the cached one is stale.
    pub async fn access_token(&self, account: &AccountConfig) -> Result<String> {
        if let Some(tokens) = self.cached.lock().unwrap().get(&account.id) {
            if tokens.is_fresh() {
                return Ok(tokens.access_token.clone());
            }
        }

        let Some(refresh_token) = secrets::get(SecretKind::RefreshToken, account.id)? else {
            bail!("{} is not signed in; use Accounts \u{2192} Sign in", account.email);
        };

        let creds = client_credentials(account);
        let tokens = oauth::refresh(&creds, &refresh_token).await?;
        self.remember(account.id, tokens.clone())?;
        Ok(tokens.access_token)
    }

    /// Stores a freshly issued token set, persisting the refresh token.
    pub fn remember(&self, account: AccountId, tokens: TokenSet) -> Result<()> {
        if let Some(refresh) = &tokens.refresh_token {
            secrets::set(SecretKind::RefreshToken, account, refresh)?;
        }
        self.cached.lock().unwrap().insert(account, tokens);
        Ok(())
    }

    /// Whether this account has completed the OAuth flow at least once.
    /// Configuring a client id is not the same as being authorized.
    pub fn is_signed_in(&self, account: AccountId) -> bool {
        secrets::get(SecretKind::RefreshToken, account)
            .ok()
            .flatten()
            .is_some()
    }

    /// Drops cached state for an account, e.g. after sign-out.
    pub fn forget(&self, account: AccountId) {
        self.cached.lock().unwrap().remove(&account);
    }
}

pub fn client_credentials(account: &AccountConfig) -> ClientCredentials {
    ClientCredentials {
        client_id: account.oauth_client_id.trim().to_string(),
        client_secret: account.oauth_client_secret.trim().to_string(),
    }
}
