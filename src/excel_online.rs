use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::provider::{SheetProvider, SheetWrite};

const GRAPH_API: &str = "https://graph.microsoft.com/v1.0";

#[derive(Deserialize, Clone)]
struct ClientCredentials {
    tenant_id: String,
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct UsedRangeResponse {
    values: Option<Vec<Vec<serde_json::Value>>>,
}

/// `sheet_key` format: `{drive_id}/{item_id}`
fn parse_sheet_key(sheet_key: &str) -> Result<(&str, &str)> {
    sheet_key
        .split_once('/')
        .context("excel_online sheet_key must be '{drive_id}/{item_id}'")
}

fn workbook_url(drive_id: &str, item_id: &str) -> String {
    format!(
        "{}/drives/{}/items/{}/workbook",
        GRAPH_API, drive_id, item_id
    )
}

pub struct ExcelOnlineProvider {
    client: Client,
    creds: ClientCredentials,
    token_info: tokio::sync::Mutex<(String, std::time::Instant)>,
    /// Row count after the last `get_all_values` call; 0 = not yet populated.
    /// Incremented on each `append_row` to avoid re-fetching the row count.
    row_count: AtomicUsize,
}

impl ExcelOnlineProvider {
    pub async fn from_client_credentials() -> Result<Self> {
        let creds_path = dirs::home_dir()
            .context("Cannot determine home directory")?
            .join(".config")
            .join("msgraph")
            .join("client_credentials.json");

        let creds_json = std::fs::read_to_string(&creds_path).with_context(|| {
            format!(
                "Failed to read Microsoft Graph credentials from {}.\n\
                 Create an Azure AD app registration and store tenant_id, client_id, client_secret.",
                creds_path.display()
            )
        })?;

        let creds: ClientCredentials =
            serde_json::from_str(&creds_json).context("Failed to parse client_credentials.json")?;

        let client = Client::new();
        let access_token = Self::fetch_token(&client, &creds).await?;
        let token_info = tokio::sync::Mutex::new((access_token, std::time::Instant::now()));

        Ok(Self {
            client,
            creds,
            token_info,
            row_count: AtomicUsize::new(0),
        })
    }

    async fn fetch_token(client: &Client, creds: &ClientCredentials) -> Result<String> {
        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            creds.tenant_id
        );
        let resp = client
            .post(&token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &creds.client_id),
                ("client_secret", &creds.client_secret),
                ("scope", "https://graph.microsoft.com/.default"),
            ])
            .send()
            .await
            .context("Failed to request Microsoft Graph access token")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to obtain Microsoft Graph token ({status}): {text}");
        }

        let token: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse Microsoft Graph token response")?;

        Ok(token.access_token)
    }

    pub async fn get_token(&self) -> Result<String> {
        let mut guard = self.token_info.lock().await;
        let (ref token, instant) = *guard;
        if instant.elapsed().as_secs() > 3000 {
            log::info!("Microsoft Graph access token expired or near expiry, refreshing...");
            let new_token = Self::fetch_token(&self.client, &self.creds).await?;
            *guard = (new_token.clone(), std::time::Instant::now());
            Ok(new_token)
        } else {
            Ok(token.clone())
        }
    }
}

#[async_trait]
impl SheetProvider for ExcelOnlineProvider {
    async fn get_all_values(&self, sheet_key: &str, worksheet: &str) -> Result<Vec<Vec<String>>> {
        let (drive_id, item_id) = parse_sheet_key(sheet_key)?;
        let url = format!(
            "{}/worksheets/{}/usedRange",
            workbook_url(drive_id, item_id),
            urlencoding::encode(worksheet)
        );
        let token = self.get_token().await?;
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Failed to read Excel Online worksheet")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to read worksheet '{}' ({status}): {text}",
                worksheet
            );
        }

        let data: UsedRangeResponse = resp
            .json()
            .await
            .context("Failed to parse usedRange response")?;

        let rows: Vec<Vec<String>> = data
            .values
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.into_iter().map(value_to_string).collect())
            .collect();
        self.row_count.store(rows.len(), Ordering::Relaxed);
        Ok(rows)
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

        let (drive_id, item_id) = parse_sheet_key(sheet_key)?;
        let token = self.get_token().await?;

        // We process changes in chunks of 20 (Graph API batch limit)
        for chunk in changes.chunks(20) {
            let mut requests = Vec::new();

            for (idx, change) in chunk.iter().enumerate() {
                let (address, values) = match &change.date {
                    Some(date) => {
                        let addr = format!("A{}:B{}", change.row, change.row);
                        (addr, serde_json::json!([[date, change.hours]]))
                    }
                    None => {
                        let addr = format!("B{}", change.row);
                        (addr, serde_json::json!([[change.hours]]))
                    }
                };

                let relative_url = format!(
                    "/drives/{}/items/{}/workbook/worksheets/{}/range(address='{}')",
                    drive_id,
                    item_id,
                    urlencoding::encode(worksheet),
                    address
                );

                let mut headers = std::collections::HashMap::new();
                headers.insert("Content-Type".to_string(), "application/json".to_string());

                requests.push(serde_json::json!({
                    "id": idx.to_string(),
                    "method": "PATCH",
                    "url": relative_url,
                    "headers": headers,
                    "body": { "values": values }
                }));
            }

            let batch_url = format!("{}/$batch", GRAPH_API);
            let resp = self
                .client
                .post(&batch_url)
                .bearer_auth(&token)
                .json(&serde_json::json!({ "requests": requests }))
                .send()
                .await
                .context("Failed to send batch request to Excel Online")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Failed to execute batch update on Excel Online ({status}): {text}");
            }
        }

        Ok(())
    }
}

fn value_to_string(v: serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}
