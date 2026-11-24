//! Makes Cursor believe it is signed in as a local Ultra account.
//!
//! The token this writes is not a credential: no server verifies its
//! signature. Its only job is to mark a request as "went through this app", so
//! the local server can answer account and entitlement calls itself instead of
//! forwarding them. An already signed-in user is left untouched.

use std::path::PathBuf;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};

use super::{Error, Result};

const EMAIL: &str = "cursor@ai.com";
const SIGN_UP_TYPE: &str = "Google";
const SUBJECT: &str = "cursor-local-user";
const MEMBERSHIP_TYPE: &str = "ultra";
const SUBSCRIPTION_STATUS: &str = "active";

/// Cursor's `state.vscdb`, where its auth and membership cache live.
#[derive(Debug)]
pub struct AccountStore {
    state_db: PathBuf,
}

impl AccountStore {
    /// Cursor's real `state.vscdb` for this platform.
    pub fn managed() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
        let path = match std::env::consts::OS {
            "macos" => {
                home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
            }
            "windows" => std::env::var_os("APPDATA")
                .map_or_else(|| home.join("AppData/Roaming"), PathBuf::from)
                .join("Cursor/User/globalStorage/state.vscdb"),
            "linux" => std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("Cursor/User/globalStorage/state.vscdb"),
            platform => {
                return Err(Error::Config(format!(
                    "Cursor account injection is unsupported on {platform}"
                )));
            }
        };
        Ok(Self { state_db: path })
    }

    /// A store over an explicit database file, for tests.
    pub fn at(state_db: PathBuf) -> Self {
        Self { state_db }
    }

    /// Writes the local account unless a real one is already there, and repairs
    /// the cached membership when the stored token is this app's own.
    pub async fn inject_if_missing(&self) -> Result<()> {
        if let Some(parent) = self.state_db.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let options = SqliteConnectOptions::new()
            .filename(&self.state_db)
            .create_if_missing(true)
            // Cursor keeps this file open while it runs.
            .busy_timeout(std::time::Duration::from_secs(5));
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .execute(&mut connection)
        .await?;

        let token = local_token()?;
        let account =
            sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
                .bind("cursorAuth/accessToken")
                .fetch_optional(&mut connection)
                .await?;
        if account.is_some_and(|row| {
            row.try_get::<String, _>("value")
                .is_ok_and(|value| !value.trim().is_empty() && !is_local_cursor_token(&value))
        }) {
            return Ok(());
        }

        let values = [
            ("cursorAuth/accessToken", token.as_str()),
            ("cursorAuth/refreshToken", token.as_str()),
            ("cursorAuth/cachedEmail", EMAIL),
            ("cursorAuth/cachedSignUpType", SIGN_UP_TYPE),
            ("cursorAuth/stripeMembershipAuthId", SUBJECT),
            ("cursorAuth/stripeMembershipType", MEMBERSHIP_TYPE),
            ("cursorAuth/stripeSubscriptionStatus", SUBSCRIPTION_STATUS),
        ];
        let mut transaction = connection.begin().await?;
        for (key, value) in values {
            sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
                .bind(key)
                .bind(value)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        tracing::info!(
            email = EMAIL,
            subject = SUBJECT,
            "injected the local Cursor account"
        );
        Ok(())
    }
}

/// Whether an `Authorization` header carries this app's local token.
fn is_local_cursor_authorization(authorization: &str) -> bool {
    authorization
        .strip_prefix("Bearer ")
        .is_some_and(is_local_cursor_token)
}

/// Whether `headers` carry this app's local Cursor token.
pub fn request_uses_local_cursor_token(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_local_cursor_authorization)
}

fn is_local_cursor_token(token: &str) -> bool {
    let mut segments = token.split('.');
    let Some(header) = segments.next() else {
        return false;
    };
    let Some(payload) = segments.next() else {
        return false;
    };
    let Some(signature) = segments.next() else {
        return false;
    };
    if segments.next().is_some() || signature != SUBJECT {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(header)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("alg")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("HS256")
        && URL_SAFE_NO_PAD
            .decode(payload)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| {
                value.get("sub").and_then(serde_json::Value::as_str) == Some(SUBJECT)
            })
}

/// The unsigned JWT Cursor stores for the local account. Recognition validates
/// its header, subject and marker segment, not JSON serialization order.
fn local_token() -> Result<String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "sub": SUBJECT,
        "email": EMAIL,
        "type": "session",
        "iss": "cursor-client",
        "scope": "openid profile email",
        "exp": 4_070_908_800_u64
    }))?);
    Ok(format!("{header}.{payload}.{SUBJECT}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    async fn connect(state_db: &Path) -> SqliteConnection {
        let options = SqliteConnectOptions::new()
            .filename(state_db)
            .create_if_missing(true);
        SqliteConnection::connect_with(&options).await.unwrap()
    }

    async fn value(state_db: &Path, key: &str) -> Option<String> {
        let mut connection = connect(state_db).await;
        sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
            .bind(key)
            .fetch_optional(&mut connection)
            .await
            .unwrap()
            .map(|row| row.try_get::<String, _>("value").unwrap())
    }

    #[tokio::test]
    async fn injects_the_local_account_and_repairs_its_membership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.vscdb");
        let store = AccountStore::at(path.clone());
        store.inject_if_missing().await.unwrap();

        assert_eq!(
            value(&path, "cursorAuth/stripeMembershipType")
                .await
                .as_deref(),
            Some(MEMBERSHIP_TYPE)
        );

        let mut connection = connect(&path).await;
        sqlx::query("UPDATE ItemTable SET value = 'free' WHERE key = ?")
            .bind("cursorAuth/stripeMembershipType")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind("cursorAuth/stripeMembershipAuthId")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        store.inject_if_missing().await.unwrap();
        assert_eq!(
            value(&path, "cursorAuth/stripeMembershipType")
                .await
                .as_deref(),
            Some(MEMBERSHIP_TYPE)
        );
        assert_eq!(
            value(&path, "cursorAuth/stripeMembershipAuthId")
                .await
                .as_deref(),
            Some(SUBJECT)
        );
    }

    #[tokio::test]
    async fn leaves_a_real_signed_in_account_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.vscdb");
        let mut connection = connect(&path).await;
        sqlx::query("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO ItemTable(key, value) VALUES(?, ?)")
            .bind("cursorAuth/accessToken")
            .bind("official-cursor-token")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        AccountStore::at(path.clone())
            .inject_if_missing()
            .await
            .unwrap();

        assert_eq!(
            value(&path, "cursorAuth/accessToken").await.as_deref(),
            Some("official-cursor-token")
        );
        assert_eq!(value(&path, "cursorAuth/cachedEmail").await, None);
    }

    #[test]
    fn recognises_only_the_injected_cursor_token() {
        let token = local_token().unwrap();
        assert!(is_local_cursor_token(&token));
        assert!(is_local_cursor_authorization(&format!("Bearer {token}")));
        assert!(!is_local_cursor_authorization(&token));
        assert!(!is_local_cursor_authorization(
            "Bearer official-cursor-token"
        ));
        assert!(!is_local_cursor_token("official-cursor-token"));
        assert!(!is_local_cursor_token(""));
    }

    #[test]
    fn recognises_a_local_token_with_changed_payload() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"cursor-local-user","exp":1}"#);
        let token = format!("{header}.{payload}.{SUBJECT}");
        assert_ne!(token, local_token().unwrap());
        assert!(is_local_cursor_token(&token));
    }

    #[test]
    fn a_fake_signature_does_not_make_a_token_local() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"cursor-local-user"}"#);
        assert!(!is_local_cursor_token(&format!(
            "{header}.{payload}.official"
        )));
    }
}
