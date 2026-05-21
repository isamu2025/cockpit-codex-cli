use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use uuid::Uuid;

const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_REFRESH_SKEW_SECONDS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub name: String,
    pub email: String,
    pub account_id: Option<String>,
    pub organization_id: Option<String>,
    pub user_id: Option<String>,
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub imported_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub id: String,
    pub name: String,
    pub email: String,
    pub account_id: Option<String>,
    pub organization_id: Option<String>,
    pub has_refresh_token: bool,
    pub token_expired: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct ImportedAccount {
    pub account: Account,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct CodexTokens {
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
}

impl Account {
    pub fn summary(&self) -> AccountSummary {
        AccountSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            email: self.email.clone(),
            account_id: self.account_id.clone(),
            organization_id: self.organization_id.clone(),
            has_refresh_token: self.refresh_token.is_some(),
            token_expired: is_jwt_expired(&self.access_token),
            updated_at: self.updated_at,
        }
    }
}

pub fn import_auth_file(
    path: &Path,
    name: &str,
    email_override: Option<&str>,
) -> Result<ImportedAccount> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let tokens = extract_tokens(&value).ok_or_else(|| {
        anyhow!("auth file does not contain access_token/accessToken or tokens.access_token")
    })?;
    let claims = decode_jwt_payload(&tokens.access_token).unwrap_or(Value::Null);
    let auth_claims = claims
        .get("https://api.openai.com/auth")
        .cloned()
        .unwrap_or(Value::Null);
    let email = email_override
        .map(str::to_string)
        .or_else(|| string_field(&value, "email"))
        .or_else(|| string_field(&value, "outlook_email"))
        .or_else(|| string_field(&claims, "email"))
        .or_else(|| string_field(&auth_claims, "email"))
        .unwrap_or_else(|| format!("imported-{}@unknown.local", Uuid::new_v4()));
    let now = Utc::now().timestamp_millis();
    let account = Account {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        email,
        account_id: string_field(&value, "account_id")
            .or_else(|| string_field(&value, "chatgpt_account_id"))
            .or_else(|| string_field(&auth_claims, "chatgpt_account_id"))
            .or_else(|| string_field(&claims, "chatgpt_account_id")),
        organization_id: string_field(&auth_claims, "poid")
            .or_else(|| string_field(&auth_claims, "organization_id"))
            .or_else(|| string_field(&claims, "organization_id")),
        user_id: string_field(&auth_claims, "chatgpt_user_id")
            .or_else(|| string_field(&claims, "sub")),
        id_token: tokens.id_token,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        imported_at: now,
        updated_at: now,
    };
    Ok(ImportedAccount {
        account,
        source: path.display().to_string(),
    })
}

pub async fn ensure_fresh(account: &mut Account, client: &reqwest::Client) -> Result<bool> {
    if !is_jwt_expired_with_skew(&account.access_token, TOKEN_REFRESH_SKEW_SECONDS) {
        return Ok(false);
    }
    let refresh_token = account.refresh_token.clone().ok_or_else(|| {
        anyhow!(
            "account {} has expired access token and no refresh token",
            account.email
        )
    })?;
    let tokens = refresh_access_token(client, &refresh_token, account.id_token.as_deref()).await?;
    account.id_token = tokens.id_token.or_else(|| account.id_token.clone());
    account.access_token = tokens.access_token;
    account.refresh_token = tokens.refresh_token.or(Some(refresh_token));
    account.updated_at = Utc::now().timestamp_millis();
    Ok(true)
}

pub async fn refresh_access_token(
    client: &reqwest::Client,
    refresh_token: &str,
    current_id_token: Option<&str>,
) -> Result<CodexTokens> {
    let response = client
        .post(TOKEN_ENDPOINT)
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .context("send token refresh request")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read token refresh response")?;
    if !status.is_success() {
        return Err(anyhow!(
            "token refresh failed: status={}, body_len={}",
            status,
            body.len()
        ));
    }
    let value: Value = serde_json::from_str(&body).context("parse token refresh response")?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token refresh response missing access_token"))?
        .to_string();
    let id_token = value
        .get("id_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| current_id_token.map(str::to_string));
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| Some(refresh_token.to_string()));
    Ok(CodexTokens {
        id_token,
        access_token,
        refresh_token,
    })
}

pub fn extract_tokens(value: &Value) -> Option<CodexTokens> {
    let candidates = [
        value.get("tokens"),
        value.get("auth"),
        value.get("oauth"),
        value.get("credentials"),
        Some(value),
    ];
    for candidate in candidates.into_iter().flatten() {
        let access_token = string_field(candidate, "access_token")
            .or_else(|| string_field(candidate, "accessToken"))?;
        let id_token =
            string_field(candidate, "id_token").or_else(|| string_field(candidate, "idToken"));
        let refresh_token = string_field(candidate, "refresh_token")
            .or_else(|| string_field(candidate, "refreshToken"));
        return Some(CodexTokens {
            id_token,
            access_token,
            refresh_token,
        });
    }
    None
}

pub fn decode_jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn is_jwt_expired(token: &str) -> bool {
    is_jwt_expired_with_skew(token, 0)
}

pub fn is_jwt_expired_with_skew(token: &str, skew_seconds: i64) -> bool {
    let Some(payload) = decode_jwt_payload(token) else {
        return false;
    };
    let Some(exp) = payload.get("exp").and_then(Value::as_i64) else {
        return false;
    };
    Utc::now().timestamp() + skew_seconds >= exp
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
pub fn unsigned_jwt(payload: Value) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = serde_json::json!({"alg":"none","typ":"JWT"});
    format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    #[test]
    fn imports_official_tokens_shape() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let access = unsigned_jwt(serde_json::json!({
            "exp": Utc::now().timestamp() + 3600,
            "email": "dev@example.com",
            "sub": "user-1",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-1",
                "poid": "org-1"
            }
        }));
        fs::write(
            &path,
            serde_json::json!({
                "tokens": {
                    "access_token": access,
                    "refresh_token": "refresh-1"
                }
            })
            .to_string(),
        )
        .unwrap();

        let imported = import_auth_file(&path, "main", None).unwrap();
        assert_eq!(imported.account.email, "dev@example.com");
        assert_eq!(imported.account.account_id.as_deref(), Some("acc-1"));
        assert_eq!(imported.account.organization_id.as_deref(), Some("org-1"));
        assert_eq!(imported.account.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[test]
    fn imports_camel_case_tokens_shape_with_email_override() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let access = unsigned_jwt(serde_json::json!({ "exp": Utc::now().timestamp() + 3600 }));
        fs::write(
            &path,
            serde_json::json!({
                "accessToken": access,
                "refreshToken": "refresh-2"
            })
            .to_string(),
        )
        .unwrap();

        let imported = import_auth_file(&path, "main", Some("manual@example.com")).unwrap();
        assert_eq!(imported.account.email, "manual@example.com");
        assert_eq!(imported.account.refresh_token.as_deref(), Some("refresh-2"));
    }

    #[test]
    fn detects_expired_access_token() {
        let expired = unsigned_jwt(serde_json::json!({ "exp": Utc::now().timestamp() - 60 }));
        let fresh = unsigned_jwt(serde_json::json!({ "exp": Utc::now().timestamp() + 3600 }));
        assert!(is_jwt_expired(&expired));
        assert!(!is_jwt_expired(&fresh));
    }
}
