//! PSN OAuth: trade an NPSSO cookie for tokens, keep them on disk, refresh them.
//!
//! This is the same flow the PlayStation mobile app uses (and that psn-api /
//! PSNAWP use). The client id/secret below are the app's public credentials.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, COOKIE, LOCATION};
use serde::{Deserialize, Serialize};

const AUTH_BASE: &str = "https://ca.account.sony.com/api/authz/v3/oauth";
const CLIENT_ID: &str = "09515159-7237-4370-9b40-3806e67c0891";
const BASIC_AUTH: &str =
    "Basic MDk1MTUxNTktNzIzNy00MzcwLTliNDAtMzgwNmU2N2MwODkxOnVjUGprYTV0bnRCMktxc1A=";
const REDIRECT_URI: &str = "com.scee.psxandroid.scecompcall://redirect";
const SCOPE: &str = "psn:mobile.v2.core psn:clientapp";

/// Refresh the access token when it has less than this many seconds left.
const EXPIRY_MARGIN_SECS: i64 = 120;

/// The user has to run `psn-friends login` again (no tokens, or they expired).
#[derive(Debug)]
pub struct NeedsLogin(pub String);

impl fmt::Display for NeedsLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsLogin {}

#[derive(Serialize, Deserialize, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub access_expires_at: i64,
    pub refresh_token: String,
    pub refresh_expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
    refresh_token_expires_in: Option<i64>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn tokens_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("no config directory")?;
    Ok(dir.join("psn-friends").join("tokens.json"))
}

/// First sign-in: NPSSO cookie -> authorization code -> tokens.
pub fn login(client: &Client, npsso: &str) -> Result<Tokens> {
    let resp = client
        .get(format!("{AUTH_BASE}/authorize"))
        .query(&[
            ("access_type", "offline"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("response_type", "code"),
            ("scope", SCOPE),
        ])
        .header(COOKIE, format!("npsso={npsso}"))
        .send()
        .context("contacting Sony's sign-in server")?;

    let location = resp
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let code = query_param(location, "code").ok_or_else(|| {
        NeedsLogin(
            "Sony rejected that NPSSO token. Grab a fresh one (it changes every time you sign in) \
             and try again."
                .into(),
        )
    })?;

    let tokens = exchange(
        client,
        &[
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("grant_type", "authorization_code"),
            ("token_format", "jwt"),
        ],
        None,
    )?;
    save(&tokens)?;
    Ok(tokens)
}

/// Load saved tokens, refreshing the access token if it is (nearly) expired.
pub fn valid_tokens(client: &Client) -> Result<Tokens> {
    let path = tokens_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(NeedsLogin("Not signed in. Run `psn-friends login`.".into()).into())
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let tokens: Tokens =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    if tokens.access_expires_at - EXPIRY_MARGIN_SECS > now() {
        return Ok(tokens);
    }
    if tokens.refresh_expires_at <= now() {
        return Err(NeedsLogin(
            "PSN sign-in expired (they last about two months). Run `psn-friends login`.".into(),
        )
        .into());
    }

    let fresh = exchange(
        client,
        &[
            ("refresh_token", tokens.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
            ("token_format", "jwt"),
            ("scope", SCOPE),
        ],
        Some(&tokens),
    )?;
    save(&fresh)?;
    Ok(fresh)
}

fn exchange(client: &Client, form: &[(&str, &str)], previous: Option<&Tokens>) -> Result<Tokens> {
    let resp = client
        .post(format!("{AUTH_BASE}/token"))
        .header(AUTHORIZATION, BASIC_AUTH)
        .form(form)
        .send()
        .context("contacting Sony's token server")?;

    let status = resp.status();
    if status.is_client_error() {
        // 400/401 here means the code or refresh token is no longer accepted.
        return Err(NeedsLogin(format!(
            "PSN refused the saved sign-in (HTTP {status}). Run `psn-friends login`."
        ))
        .into());
    }
    let resp = resp.error_for_status().context("PSN token server error")?;
    let body: TokenResponse = resp.json().context("unexpected token response from PSN")?;

    let t = now();
    // A refresh response may omit the refresh token; keep the one we have.
    let (refresh_token, refresh_expires_at) = match (body.refresh_token, previous) {
        (Some(rt), _) => (
            rt,
            t + body.refresh_token_expires_in.unwrap_or(60 * 24 * 3600),
        ),
        (None, Some(prev)) => (prev.refresh_token.clone(), prev.refresh_expires_at),
        (None, None) => anyhow::bail!("PSN did not return a refresh token"),
    };
    Ok(Tokens {
        access_token: body.access_token,
        access_expires_at: t + body.expires_in,
        refresh_token,
        refresh_expires_at,
    })
}

fn save(tokens: &Tokens) -> Result<()> {
    let path = tokens_path()?;
    let dir = path.parent().expect("tokens path has a parent");
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(tokens)?.as_bytes())?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn logout() -> Result<bool> {
    let path = tokens_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Pull `name=` out of a redirect like `com.scee...://redirect/?code=v3.abc&cid=...`.
fn query_param(location: &str, name: &str) -> Option<String> {
    let query = location.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_code_from_redirect() {
        let loc = "com.scee.psxandroid.scecompcall://redirect/?code=v3.AbC123&cid=8f2e";
        assert_eq!(query_param(loc, "code").as_deref(), Some("v3.AbC123"));
    }

    #[test]
    fn no_code_on_error_redirect() {
        let loc =
            "com.scee.psxandroid.scecompcall://redirect/?error=login_required&error_code=4165";
        assert_eq!(query_param(loc, "code"), None);
        assert_eq!(query_param("", "code"), None);
    }
}
