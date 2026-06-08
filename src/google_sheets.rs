use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::provider::{SheetProvider, SheetWrite};

const SHEETS_API: &str = "https://sheets.googleapis.com/v4/spreadsheets";

#[derive(Deserialize, Clone)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Serialize)]
struct JwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: i64,
    exp: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct ValuesResponse {
    values: Option<Vec<Vec<String>>>,
}

pub struct SheetsClient {
    client: Client,
    sa: ServiceAccount,
    token_info: tokio::sync::Mutex<(String, std::time::Instant)>,
}

impl SheetsClient {
    /// Authenticate using a gspread-compatible service account JSON at
    /// ~/.config/gspread/service_account.json  (macOS/Linux)
    /// %APPDATA%\..\Roaming\.config\gspread\service_account.json  (Windows)
    pub async fn from_service_account() -> Result<Self> {
        let sa_path = dirs::home_dir()
            .context("Cannot determine home directory")?
            .join(".config")
            .join("gspread")
            .join("service_account.json");

        let sa_json = std::fs::read_to_string(&sa_path).with_context(|| {
            format!(
                "Failed to read service account from {}.\n\
                 See: https://gspread.readthedocs.io/en/latest/oauth2.html#for-bots-using-service-account",
                sa_path.display()
            )
        })?;

        let sa: ServiceAccount =
            serde_json::from_str(&sa_json).context("Failed to parse service account JSON")?;

        let client = Client::new();
        let access_token = Self::fetch_token(&client, &sa).await?;
        let token_info = tokio::sync::Mutex::new((access_token, std::time::Instant::now()));

        Ok(Self {
            client,
            sa,
            token_info,
        })
    }

    async fn fetch_token(client: &Client, sa: &ServiceAccount) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = JwtClaims {
            iss: sa.client_email.clone(),
            scope: "https://www.googleapis.com/auth/spreadsheets".to_string(),
            aud: sa.token_uri.clone(),
            iat: now,
            exp: now + 3600,
        };

        let key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
            .context("Failed to parse service account private key (expected RSA PEM)")?;
        let jwt = encode(&Header::new(Algorithm::RS256), &claims, &key)?;

        let resp = client
            .post(&sa.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", jwt.as_str()),
            ])
            .send()
            .await
            .context("Failed to request Google access token")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to obtain Google access token ({}): {}",
                status,
                text
            );
        }

        let token: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse Google token response")?;

        Ok(token.access_token)
    }

    pub async fn get_token(&self) -> Result<String> {
        let mut guard = self.token_info.lock().await;
        let (ref token, instant) = *guard;
        if instant.elapsed().as_secs() > 3000 {
            log::info!("Google access token expired or near expiry, refreshing...");
            let new_token = Self::fetch_token(&self.client, &self.sa).await?;
            *guard = (new_token.clone(), std::time::Instant::now());
            Ok(new_token)
        } else {
            Ok(token.clone())
        }
    }

    fn range_url(&self, sheet_key: &str, worksheet: &str, range: &str) -> String {
        let full_range = format!("'{}'!{}", worksheet, range);
        format!(
            "{}/{}/values/{}",
            SHEETS_API,
            sheet_key,
            urlencoding::encode(&full_range)
        )
    }

    pub async fn get_all_values(
        &self,
        sheet_key: &str,
        worksheet: &str,
    ) -> Result<Vec<Vec<String>>> {
        let url = self.range_url(sheet_key, worksheet, "A:Z");
        let token = self.get_token().await?;
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Failed to send request to Google Sheets")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to read worksheet '{}' ({}): {}",
                worksheet,
                status,
                text
            );
        }

        let data: ValuesResponse = resp
            .json()
            .await
            .context("Failed to parse Google Sheets response")?;
        Ok(data.values.unwrap_or_default())
    }
}

#[async_trait]
impl SheetProvider for SheetsClient {
    async fn get_all_values(&self, sheet_key: &str, worksheet: &str) -> Result<Vec<Vec<String>>> {
        self.get_all_values(sheet_key, worksheet).await
    }

    async fn write_changes(
        &self,
        sheet_key: &str,
        worksheet: &str,
        changes: Vec<SheetWrite>,
    ) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }

        let mut data = Vec::new();
        for change in changes {
            let (range, values) = match &change.date {
                Some(date) => {
                    let r = format!("'{}'!A{}:B{}", worksheet, change.row, change.row);
                    (r, serde_json::json!([[date, change.hours]]))
                }
                None => {
                    let r = format!("'{}'!B{}", worksheet, change.row);
                    (r, serde_json::json!([[change.hours]]))
                }
            };
            data.push(serde_json::json!({
                "range": range,
                "values": values
            }));
        }

        let url = format!("{}/{}/values:batchUpdate", SHEETS_API, sheet_key);

        let token = self.get_token().await?;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "valueInputOption": "USER_ENTERED",
                "data": data
            }))
            .send()
            .await
            .context("Failed to send batch update to Google Sheets")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to batch update Google Sheets ({status}): {text}");
        }

        Ok(())
    }
}
