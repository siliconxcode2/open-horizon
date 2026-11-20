//! Open Horizon's own accounts, beside the Microsoft ones: an offline name, or an Ely.by account
//! (https://ely.by, its Yggdrasil-style auth server).
//!
//! Either one plays the game under its name (`--name`). Neither can download Minecraft: that
//! stays with a Microsoft account that owns it (`account::launch`), so the game files come from
//! the player's own Java Edition install, or from Mojang after that one ownership check.
//!
//! Ely.by's token is kept in the secret store under `elyby:<uuid>` for joining Ely.by servers
//! and its skins; accounts.json keeps only the name, the UUID and the kind.

use super::{send, str_field, Account, Accounts, SecretStore};
use serde_json::{json, Value};

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

/// What the secret store keeps for an Ely.by account.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ElyBySecrets {
    pub access_token: String,
    pub client_token: String,
}

impl std::fmt::Debug for ElyBySecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ElyBySecrets(<hidden>)")
    }
}

/// Ely.by's answer to a sign-in.
#[derive(Debug)]
pub enum ElyBySignIn {
    Done(Account, ElyBySecrets),
    /// The account has two-step verification: ask for the code and sign in again with it.
    NeedsCode,
}

/// Signs in to Ely.by at `server` (its auth server's root, `horizon_addresses::ELYBY_AUTH`) with
/// `login` (the name or e-mail) and `password`, and `code` when the account has two-step
/// verification on.
pub fn elyby_sign_in(
    server: &str,
    login: &str,
    password: &str,
    code: Option<&str>,
) -> Result<ElyBySignIn, String> {
    let mut client = [0u8; 16];
    getrandom::fill(&mut client).map_err(|e| e.to_string())?;
    let client_token: String = client.iter().map(|b| format!("{b:02x}")).collect();
    // Ely.by takes the code after the password, separated by a colon.
    let password = match code.map(str::trim).filter(|c| !c.is_empty()) {
        Some(code) => format!("{password}:{code}"),
        None => password.to_owned(),
    };
    let body = json!({
        "username": login.trim(),
        "password": password,
        "clientToken": client_token,
        "requestUser": true,
    });
    let (status, v) = send(
        super::request_to("POST", &format!("{server}/auth/authenticate")),
        Some(super::Body::Json(body)),
        "Ely.by",
    )?;
    elyby_answer(status, &v, client_token)
}

/// Reads Ely.by's answer to `/auth/authenticate`.
fn elyby_answer(status: u16, v: &Value, client_token: String) -> Result<ElyBySignIn, String> {
    if status != 200 {
        let message = str_field(v, "errorMessage").unwrap_or_default();
        if message.to_ascii_lowercase().contains("two factor") {
            return Ok(ElyBySignIn::NeedsCode);
        }
        return Err(
            if status == 401
                || str_field(v, "error").as_deref() == Some("ForbiddenOperationException")
            {
                "Ely.by didn't accept that name and password.".into()
            } else if message.is_empty() {
                format!("Ely.by answered {status}.")
            } else {
                format!("Ely.by: {message}")
            },
        );
    }
    let profile = v.get("selectedProfile").ok_or("Ely.by sent no profile.")?;
    let (Some(uuid), Some(name), Some(access_token)) = (
        str_field(profile, "id"),
        str_field(profile, "name"),
        str_field(v, "accessToken"),
    ) else {
        return Err("Could not read Ely.by's answer.".into());
    };
    Ok(ElyBySignIn::Done(
        Account {
            uuid: uuid.replace('-', "").to_lowercase(),
            name,
            skin: None,
            cape: None,
        },
        ElyBySecrets {
            access_token,
            client_token: str_field(v, "clientToken").unwrap_or(client_token),
        },
    ))
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
    fn elyby_answers_are_read() {
        let ok = json!({
            "accessToken": "t", "clientToken": "c",
            "selectedProfile": {"id": "ffc8fdc95824509e8a57c99b940fb996", "name": "ErickSkrauch"}
        });
        let ElyBySignIn::Done(account, secrets) = elyby_answer(200, &ok, "mine".into()).unwrap()
        else {
            panic!("signed in")
        };
        assert_eq!(account.name, "ErickSkrauch");
        assert_eq!(secrets.client_token, "c");
        let two_step = json!({"error": "ForbiddenOperationException",
            "errorMessage": "Account protected with two factor auth."});
        assert!(matches!(
            elyby_answer(401, &two_step, String::new()),
            Ok(ElyBySignIn::NeedsCode)
        ));
        let wrong = json!({"error": "ForbiddenOperationException",
            "errorMessage": "Invalid credentials. Invalid username or password."});
        assert!(elyby_answer(401, &wrong, String::new()).is_err());
    }
}
