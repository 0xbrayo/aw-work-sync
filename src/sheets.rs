use anyhow::{Context, Result};
use chrono::Utc;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::Client;
use serde::{Deserialize, Serialize};

const SHEETS_API: &str = "https://sheets.googleapis.com/v4/spreadsheets";

#[derive(Deserialize)]
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
    access_token: String,
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

        let client = Client::new();
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

        Ok(Self {
            client,
            access_token: token.access_token,
        })
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
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.access_token)
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

    /// Update column B of the given 1-indexed row with a new hours value.
    pub async fn update_cell(
        &self,
        sheet_key: &str,
        worksheet: &str,
        row: usize,
        hours: f64,
    ) -> Result<()> {
        let url = format!(
            "{}?valueInputOption=USER_ENTERED",
            self.range_url(sheet_key, worksheet, &format!("B{}", row))
        );
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({"values": [[hours]]}))
            .send()
            .await
            .context("Failed to update cell")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to update cell ({status}): {text}");
        }
        Ok(())
    }

    /// Append [date, hours] as a new row after the last data row.
    pub async fn append_row(
        &self,
        sheet_key: &str,
        worksheet: &str,
        date: &str,
        hours: f64,
    ) -> Result<()> {
        let full_range = format!("'{}'!A:B", worksheet);
        let url = format!(
            "{}/{}/values/{}:append?valueInputOption=USER_ENTERED",
            SHEETS_API,
            sheet_key,
            urlencoding::encode(&full_range)
        );
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({"values": [[date, hours]]}))
            .send()
            .await
            .context("Failed to append row")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to append row ({status}): {text}");
        }
        Ok(())
    }
}
