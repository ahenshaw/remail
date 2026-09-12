//! Google OAuth2 for IMAP and SMTP, using the installed-application flow:
//! a loopback HTTP listener receives the authorization code, which is then
//! exchanged for tokens with PKCE.
//!
//! Google does not publish a client that third-party applications may share,
//! so the user registers a "Desktop app" OAuth client of their own and pastes
//! the id and secret into the account settings. The secret is not secret in
//! this flow — PKCE is what actually protects the exchange.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// Full IMAP/SMTP access. Google's narrower Gmail API scopes do not grant it.
const SCOPE: &str = "https://mail.google.com/";

/// How long before nominal expiry a token is treated as stale, so a long
/// IMAP command never starts with a token that expires mid-flight.
const EXPIRY_SLACK: i64 = 120;

#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    /// Present on the initial authorization; absent on most refreshes, in
    /// which case the previously stored token stays valid.
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
}

impl TokenSet {
    pub fn is_fresh(&self) -> bool {
        now() + EXPIRY_SLACK < self.expires_at
    }
}

#[derive(Debug, Clone)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl ClientCredentials {
    pub fn is_complete(&self) -> bool {
        !self.client_id.trim().is_empty()
    }
}

/// Runs the full interactive flow: opens the system browser, waits for the
/// redirect, and exchanges the code for tokens.
///
/// Blocks on the loopback listener, so callers run this off the UI thread.
pub async fn authorize(creds: &ClientCredentials, login_hint: &str) -> Result<TokenSet> {
    if !creds.is_complete() {
        bail!("no OAuth client id configured for this account");
    }

    let verifier = random_urlsafe(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);

    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| anyhow!("could not open loopback listener: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("loopback listener has no IP address")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let url = format!(
        "{AUTH_ENDPOINT}?client_id={}&redirect_uri={}&response_type=code&scope={}\
         &code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent\
         &state={}&login_hint={}",
        enc(&creds.client_id),
        enc(&redirect_uri),
        enc(SCOPE),
        enc(&challenge),
        enc(&state),
        enc(login_hint),
    );

    open::that_detached(&url).ok();

    let expected_state = state.clone();
    let code = tokio::task::spawn_blocking(move || wait_for_code(server, &expected_state))
        .await
        .context("authorization listener panicked")??;

    exchange(creds, &code, &verifier, &redirect_uri).await
}

/// Blocks until the browser hits the loopback listener with a code, or the
/// user gives up. Returns the authorization code.
fn wait_for_code(server: tiny_http::Server, expected_state: &str) -> Result<String> {
    // Generous, but finite: the user has to sign in and consent.
    let deadline = SystemTime::now() + Duration::from_secs(300);

    loop {
        let remaining = deadline
            .duration_since(SystemTime::now())
            .map_err(|_| anyhow!("timed out waiting for authorization"))?;
        let Some(request) = server.recv_timeout(remaining)? else {
            bail!("timed out waiting for authorization");
        };

        // Browsers also request /favicon.ico; ignore anything without a query.
        let Some(query) = request.url().split_once('?').map(|(_, q)| q.to_string()) else {
            let _ = request.respond(html_response("Waiting for authorization\u{2026}"));
            continue;
        };

        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in parse_query(&query) {
            match key.as_str() {
                "code" => code = Some(value),
                "state" => state = Some(value),
                "error" => error = Some(value),
                _ => {}
            }
        }

        if let Some(err) = error {
            let _ = request.respond(html_response(&format!("Authorization failed: {err}")));
            bail!("authorization denied: {err}");
        }
        if state.as_deref() != Some(expected_state) {
            let _ = request.respond(html_response("Authorization state mismatch."));
            bail!("authorization state mismatch; the redirect did not come from this request");
        }
        let Some(code) = code else {
            let _ = request.respond(html_response("No authorization code in the redirect."));
            continue;
        };

        let _ = request.respond(html_response("Signed in. You can close this tab."));
        return Ok(code);
    }
}

async fn exchange(
    creds: &ClientCredentials,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet> {
    let params = [
        ("client_id", creds.client_id.as_str()),
        ("client_secret", creds.client_secret.as_str()),
        ("code", code),
        ("code_verifier", verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ];
    post_token(&params).await
}

/// Trades a refresh token for a fresh access token.
pub async fn refresh(creds: &ClientCredentials, refresh_token: &str) -> Result<TokenSet> {
    let params = [
        ("client_id", creds.client_id.as_str()),
        ("client_secret", creds.client_secret.as_str()),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    let mut tokens = post_token(&params).await?;
    // A refresh response usually omits the refresh token; keep the old one.
    tokens.refresh_token = tokens.refresh_token.or_else(|| Some(refresh_token.to_string()));
    Ok(tokens)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

async fn post_token(params: &[(&str, &str)]) -> Result<TokenSet> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client.post(TOKEN_ENDPOINT).form(params).send().await?;
    let status = response.status();
    let body = response.text().await?;

    if !status.is_success() {
        let detail = serde_json::from_str::<ErrorResponse>(&body)
            .map(|e| {
                if e.error_description.is_empty() {
                    e.error
                } else {
                    format!("{}: {}", e.error, e.error_description)
                }
            })
            .unwrap_or_else(|_| body.clone());
        bail!("token endpoint returned {status}: {detail}");
    }

    let parsed: TokenResponse =
        serde_json::from_str(&body).context("token endpoint returned unexpected JSON")?;
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at: now() + parsed.expires_in.unwrap_or(3600),
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn random_urlsafe(bytes: usize) -> String {
    let raw: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(raw)
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    percent_encoding::percent_decode_str(&s).decode_utf8_lossy().into_owned()
}

fn html_response(message: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>remail</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\">\
         <p>{}</p></body>",
        html_escape(message)
    );
    let header = "Content-Type: text/html; charset=utf-8".parse::<tiny_http::Header>().unwrap();
    tiny_http::Response::from_string(body).with_header(header)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_redirect_queries() {
        let q = parse_query("code=4%2F0Ab&state=xyz&scope=https%3A%2F%2Fmail.google.com%2F");
        assert_eq!(q[0], ("code".into(), "4/0Ab".into()));
        assert_eq!(q[1], ("state".into(), "xyz".into()));
        assert_eq!(q[2].1, "https://mail.google.com/");
    }

    #[test]
    fn treats_expiring_tokens_as_stale() {
        let stale = TokenSet { access_token: "a".into(), refresh_token: None, expires_at: now() + 30 };
        assert!(!stale.is_fresh());
        let good = TokenSet { access_token: "a".into(), refresh_token: None, expires_at: now() + 3600 };
        assert!(good.is_fresh());
    }
}
