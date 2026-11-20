//! Open Horizon's own accounts, beside the Microsoft ones: an offline name, or an Ely.by account
//! (https://ely.by), signed in with OAuth 2.0 and PKCE in the player's browser.
//!
//! Either one plays the game under its name (`--name`). Neither can download Minecraft: that
//! stays with a Microsoft account that owns it (`account::launch`), so the game files come from
//! the player's own Java Edition install, or from Mojang after that one ownership check.
//!
//! Ely.by's token is kept in the secret store under `elyby:<uuid>` for joining Ely.by servers
//! and its skins; accounts.json keeps only the name, the UUID and the kind.

use super::{send, str_field, Account, Accounts, SecretStore};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

/// What kind of account an entry of accounts.json is. Entries from before Open Horizon have none:
/// Microsoft.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Microsoft,
    Offline,
    ElyBy,
}

impl Kind {
    /// Whether this account starts the game under its name only, with no Microsoft session.
    pub fn plays_by_name(self) -> bool {
        !matches!(self, Self::Microsoft)
    }
}

/// Vanilla's offline UUID for `name`: `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)`, as a
/// server in offline mode gives that player. Lower-case hex, no dashes, as accounts.json keeps.
pub fn offline_uuid(name: &str) -> String {
    use md5::{Digest, Md5};
    let mut h: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    h[6] = (h[6] & 0x0f) | 0x30;
    h[8] = (h[8] & 0x3f) | 0x80;
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `name` is a name vanilla lets a player have: 3 to 16 letters, digits or `_`.
pub fn valid_name(name: &str) -> bool {
    (3..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The secret-store key of an Ely.by account's token.
fn elyby_key(uuid: &str) -> String {
    format!("elyby:{uuid}")
}

/// What the secret store keeps for an Ely.by account: its OAuth tokens. The access token has the
/// `minecraft_server_session` scope, so it is the game's session for Ely.by servers.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ElyBySecrets {
    pub access_token: String,
    /// From `offline_access`: renews the access token without asking the player again.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// When the access token expires, in seconds since 1970.
    #[serde(default)]
    pub expires_at: u64,
}

impl std::fmt::Debug for ElyBySecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ElyBySecrets(<hidden>)")
    }
}

/// Open Horizon's OAuth client at Ely.by: a public client (no secret), so every sign-in proves
/// itself with PKCE (RFC 7636, S256), and Ely.by sends the player back to a loopback address.
pub const ELYBY_CLIENT_ID: &str = "xt-launcher";
const ELYBY_SCOPES: &str = "account_info minecraft_server_session offline_access";
/// How long a sign-in waits for the player in the browser.
const ELYBY_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// `bytes` as unpadded base64url, PKCE's encoding.
fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 3986 percent-encoding of a query value.
fn query_value(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Decodes a query value (`%XX` and `+`).
fn query_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn random_token(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| e.to_string())?;
    Ok(base64url(&buf))
}

/// A started Ely.by sign-in: the page to open in the browser, and the loopback listener Ely.by
/// sends the player back to.
pub struct ElyByOAuth {
    /// The authorization page to open in the player's browser.
    pub url: String,
    listener: std::net::TcpListener,
    redirect: String,
    verifier: String,
    state: String,
}

impl std::fmt::Debug for ElyByOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElyByOAuth")
            .field("redirect", &self.redirect)
            .finish_non_exhaustive()
    }
}

/// Starts an Ely.by sign-in at `authorize` (`horizon_addresses::ELYBY_OAUTH_AUTHORIZE`): a
/// loopback listener on a free port, a fresh PKCE verifier and state.
pub fn elyby_oauth_start(authorize: &str) -> Result<ElyByOAuth, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("Could not open a local port for Ely.by's answer: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_token(48)?;
    let challenge = {
        use sha2::{Digest, Sha256};
        base64url(&Sha256::digest(verifier.as_bytes()))
    };
    let state = random_token(16)?;
    let url = format!(
        "{authorize}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}\
         &code_challenge={challenge}&code_challenge_method=S256&prompt=select_account",
        query_value(ELYBY_CLIENT_ID),
        query_value(&redirect),
        query_value(ELYBY_SCOPES),
        query_value(&state),
    );
    Ok(ElyByOAuth {
        url,
        listener,
        redirect,
        verifier,
        state,
    })
}

/// The page the browser shows once Ely.by has sent the player back.
const BACK_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Open Horizon</title>\
<body style=\"font:16px system-ui;background:#16181f;color:#ecedf3;display:grid;place-items:center;height:100vh;margin:0\">\
<p>{message}</p>";

impl ElyByOAuth {
    /// Waits for Ely.by to send the player back (until `cancelled` or five minutes), then trades
    /// the code for tokens at `token_url` and reads the profile at `info_url`.
    pub fn finish(
        self,
        token_url: &str,
        info_url: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Account, ElyBySecrets), String> {
        let code = self.wait_for_code(cancelled)?;
        let (status, v) = send(
            super::request_to("POST", token_url),
            Some(super::Body::Form(vec![
                ("client_id", ELYBY_CLIENT_ID),
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &self.redirect),
                ("code_verifier", &self.verifier),
            ])),
            "Ely.by",
        )?;
        let secrets = tokens(status, &v, None)?;
        let account = elyby_profile(info_url, &secrets.access_token)?;
        Ok((account, secrets))
    }

    fn wait_for_code(&self, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
        use std::io::{Read, Write};
        self.listener
            .set_nonblocking(true)
            .map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        loop {
            if cancelled() {
                return Err("The Ely.by sign-in was cancelled.".into());
            }
            if started.elapsed() > ELYBY_WAIT {
                return Err("Ely.by didn't answer in time. Try again.".into());
            }
            let mut stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut request = [0u8; 8192];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            let answer = callback(&request, &self.state);
            let message = match &answer {
                Some(Ok(_)) => "Signed in. You can close this tab and go back to Open Horizon.",
                Some(Err(_)) => {
                    "The sign-in didn't go through. Go back to Open Horizon to try again."
                }
                // Another request to the port (a browser's favicon): not the answer.
                None => "Open Horizon",
            };
            let body = BACK_PAGE.replace("{message}", crate::i18n::tr(message));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if let Some(answer) = answer {
                return answer;
            }
        }
    }
}

/// The code in Ely.by's redirect (`GET /callback?code=…&state=…`), checked against `state`;
/// none when the request is something else.
fn callback(request: &str, state: &str) -> Option<Result<String, String>> {
    let target = request.lines().next()?.split_whitespace().nth(1)?;
    let query = target.strip_prefix("/callback?")?;
    let field = |name: &str| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| query_decode(v))
        })
    };
    if let Some(error) = field("error") {
        let said = field("error_message").unwrap_or(error);
        return Some(Err(if said == "access_denied" {
            "Ely.by sign-in was declined.".into()
        } else {
            format!("Ely.by: {said}")
        }));
    }
    if field("state").as_deref() != Some(state) {
        return Some(Err(
            "Ely.by's answer didn't match this sign-in. Try again.".into()
        ));
    }
    Some(field("code").ok_or_else(|| "Ely.by sent no code.".to_string()))
}

/// Reads a token answer; `refresh` is kept when Ely.by sends no new one (it doesn't on refresh).
fn tokens(status: u16, v: &Value, refresh: Option<String>) -> Result<ElyBySecrets, String> {
    if status != 200 {
        let why = str_field(v, "error_description")
            .or_else(|| str_field(v, "error"))
            .unwrap_or_else(|| format!("answered {status}"));
        return Err(format!("Ely.by: {why}"));
    }
    let access_token = str_field(v, "access_token").ok_or("Ely.by sent no access token.")?;
    let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
    Ok(ElyBySecrets {
        access_token,
        refresh_token: str_field(v, "refresh_token").or(refresh),
        expires_at: super::now() + expires_in,
    })
}

/// The Ely.by account behind `token`.
fn elyby_profile(info_url: &str, token: &str) -> Result<Account, String> {
    let (status, v) = send(
        super::bearer(super::request_to("GET", info_url), token),
        None,
        "Ely.by",
    )?;
    elyby_account(status, &v)
}

fn elyby_account(status: u16, v: &Value) -> Result<Account, String> {
    if status != 200 {
        return Err(format!("Could not read the Ely.by account ({status})."));
    }
    let (Some(uuid), Some(name)) = (str_field(v, "uuid"), str_field(v, "username")) else {
        return Err("Could not read the Ely.by account.".into());
    };
    Ok(Account {
        uuid: uuid.replace('-', "").to_lowercase(),
        name,
        skin: None,
        cape: None,
    })
}

/// Renews an Ely.by access token with its refresh token at `token_url`.
pub fn elyby_refresh(token_url: &str, secrets: &ElyBySecrets) -> Result<ElyBySecrets, String> {
    let refresh = secrets
        .refresh_token
        .clone()
        .ok_or("This Ely.by sign-in can't be renewed: sign in again.")?;
    let (status, v) = send(
        super::request_to("POST", token_url),
        Some(super::Body::Form(vec![
            ("client_id", ELYBY_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("scope", ELYBY_SCOPES),
        ])),
        "Ely.by",
    )?;
    tokens(status, &v, Some(refresh))
}

impl Accounts {
    /// The kind of the account `uuid`; Microsoft for one saved before Open Horizon.
    pub fn kind(&self, uuid: &str) -> Kind {
        self.kinds.get(uuid).copied().unwrap_or_default()
    }

    /// Adds an offline account named `name` and plays as it. The same name again selects the
    /// account it already has.
    pub fn add_offline(&mut self, name: &str) -> Result<Account, String> {
        let name = name.trim();
        if !valid_name(name) {
            return Err("A name has 3 to 16 characters: letters, digits and _ (no spaces).".into());
        }
        let account = Account {
            uuid: offline_uuid(name),
            name: name.to_owned(),
            skin: None,
            cape: None,
        };
        self.put(account.clone(), Kind::Offline);
        Ok(account)
    }

    /// Keeps a signed-in Ely.by account, its token in `store`, and plays as it.
    pub fn add_elyby(
        &mut self,
        store: &dyn SecretStore,
        account: Account,
        secrets: &ElyBySecrets,
    ) -> Result<(), String> {
        let _operation = super::session::lock(store)?;
        store.set(
            &elyby_key(&account.uuid),
            &serde_json::to_string(secrets).map_err(|e| e.to_string())?,
        )?;
        self.put(account, Kind::ElyBy);
        Ok(())
    }

    /// The Ely.by token of `uuid`, if it is an Ely.by account still signed in here.
    pub fn elyby_secrets(&self, store: &dyn SecretStore, uuid: &str) -> Option<ElyBySecrets> {
        (self.kind(uuid) == Kind::ElyBy)
            .then(|| store.get(&elyby_key(uuid)).ok().flatten())
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    fn put(&mut self, account: Account, kind: Kind) {
        self.kinds.insert(account.uuid.clone(), kind);
        self.selected = Some(account.uuid.clone());
        match self.accounts.iter_mut().find(|a| a.uuid == account.uuid) {
            Some(a) => *a = account,
            None => self.accounts.push(account),
        }
    }

    /// Forgets what this module keeps for `uuid` (its kind, an Ely.by token) as it is removed.
    pub(super) fn forget_open(
        &mut self,
        store: &dyn SecretStore,
        uuid: &str,
    ) -> Result<(), String> {
        if self.kinds.remove(uuid) == Some(Kind::ElyBy) {
            store.delete(&elyby_key(uuid))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_uuid_is_vanillas() {
        // UUID.nameUUIDFromBytes("OfflinePlayer:Notch".getBytes(UTF_8)) in Java.
        assert_eq!(offline_uuid("Notch"), "b50ad385829d3141a2167e7d7539ba7f");
    }

    #[test]
    fn names_follow_vanillas_rule() {
        assert!(valid_name("Steve_01"));
        assert!(!valid_name("ab"));
        assert!(!valid_name("has space"));
        assert!(!valid_name("seventeen_chars__"));
    }

    #[test]
    fn offline_accounts_are_kept_with_their_kind_and_play() {
        let mut accounts = Accounts::default();
        let a = accounts.add_offline("Rahim").unwrap();
        assert_eq!(accounts.kind(&a.uuid), Kind::Offline);
        assert_eq!(accounts.selected().map(|s| s.name.as_str()), Some("Rahim"));
        let saved: Accounts =
            serde_json::from_str(&serde_json::to_string(&accounts).unwrap()).unwrap();
        assert_eq!(saved.kind(&a.uuid), Kind::Offline);
        assert!(saved.kind(&a.uuid).plays_by_name());
    }

    #[test]
    fn the_sign_in_page_asks_for_pkce_and_the_answer_is_checked() {
        let started = elyby_oauth_start("https://account.ely.by/oauth2/v1").unwrap();
        assert!(started.url.contains("client_id=xt-launcher"));
        assert!(started.url.contains("code_challenge_method=S256"));
        assert!(started
            .url
            .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
        let state = started.state.clone();
        let ok = format!("GET /callback?code=abc%2Fd&state={state}&iss=x HTTP/1.1\r\n");
        assert_eq!(callback(&ok, &state), Some(Ok("abc/d".into())));
        assert_eq!(query_decode("a%2"), "a%2");
        let forged = "GET /callback?code=abc&state=other HTTP/1.1\r\n";
        assert!(matches!(callback(forged, &state), Some(Err(_))));
        assert_eq!(callback("GET /favicon.ico HTTP/1.1\r\n", &state), None);
        let denied = format!("GET /callback?error=access_denied&state={state} HTTP/1.1\r\n");
        assert!(matches!(callback(&denied, &state), Some(Err(_))));
    }

    #[test]
    fn pkce_challenge_is_rfc_7636s() {
        // RFC 7636, appendix B.
        use sha2::{Digest, Sha256};
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn elyby_accounts_are_read() {
        let info = json!({"id": 1, "uuid": "ffc8fdc9-5824-509e-8a57-c99b940fb996", "username": "ErickSkrauch"});
        let a = elyby_account(200, &info).unwrap();
        assert_eq!(
            (a.uuid.as_str(), a.name.as_str()),
            ("ffc8fdc95824509e8a57c99b940fb996", "ErickSkrauch")
        );
        let t = tokens(
            200,
            &json!({"access_token": "a", "refresh_token": "r", "expires_in": 86400}),
            None,
        )
        .unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("r"));
        let renewed = tokens(
            200,
            &json!({"access_token": "b", "expires_in": 86400}),
            Some("r".into()),
        )
        .unwrap();
        assert_eq!(renewed.refresh_token.as_deref(), Some("r"));
        assert!(tokens(401, &json!({"error": "invalid_grant"}), None).is_err());
    }
}
