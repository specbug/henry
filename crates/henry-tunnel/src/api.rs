//! Cloudflare API client for tunnel and DNS management.

use crate::TunnelError;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Cloudflare API client.
pub struct CloudflareClient {
    api_token: String,
    zone_id: String,
    client: reqwest::Client,
}

impl CloudflareClient {
    const BASE_URL: &'static str = "https://api.cloudflare.com/client/v4";

    /// Create a new Cloudflare API client.
    pub fn new(api_token: impl Into<String>, zone_id: impl Into<String>) -> Self {
        Self {
            api_token: api_token.into(),
            zone_id: zone_id.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Create a DNS record.
    pub async fn create_dns_record(
        &self,
        name: &str,
        record_type: &str,
        content: &str,
        proxied: bool,
    ) -> Result<DnsRecord, TunnelError> {
        let url = format!("{}/zones/{}/dns_records", Self::BASE_URL, self.zone_id);

        let body = CreateDnsRecordRequest {
            record_type: record_type.to_string(),
            name: name.to_string(),
            content: content.to_string(),
            proxied,
            ttl: 1, // Auto TTL
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(TunnelError::CloudflareApi(format!(
                "Failed to create DNS record: {} - {}",
                status, text
            )));
        }

        let result: CloudflareResponse<DnsRecord> =
            serde_json::from_str(&text).map_err(|e| TunnelError::CloudflareApi(e.to_string()))?;

        if !result.success {
            let errors: Vec<String> = result
                .errors
                .into_iter()
                .map(|e| e.message)
                .collect();
            return Err(TunnelError::CloudflareApi(errors.join(", ")));
        }

        let record = result
            .result
            .ok_or_else(|| TunnelError::CloudflareApi("No result in response".to_string()))?;

        info!("Created DNS record: {} -> {}", name, content);
        Ok(record)
    }

    /// Delete a DNS record by ID.
    pub async fn delete_dns_record(&self, record_id: &str) -> Result<(), TunnelError> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            Self::BASE_URL, self.zone_id, record_id
        );

        let response = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await?;
            return Err(TunnelError::CloudflareApi(format!(
                "Failed to delete DNS record: {} - {}",
                status, text
            )));
        }

        info!("Deleted DNS record: {}", record_id);
        Ok(())
    }

    /// List DNS records.
    pub async fn list_dns_records(
        &self,
        name_filter: Option<&str>,
    ) -> Result<Vec<DnsRecord>, TunnelError> {
        let mut url = format!("{}/zones/{}/dns_records", Self::BASE_URL, self.zone_id);

        if let Some(name) = name_filter {
            url.push_str(&format!("?name={}", name));
        }

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(TunnelError::CloudflareApi(format!(
                "Failed to list DNS records: {} - {}",
                status, text
            )));
        }

        let result: CloudflareResponse<Vec<DnsRecord>> =
            serde_json::from_str(&text).map_err(|e| TunnelError::CloudflareApi(e.to_string()))?;

        result
            .result
            .ok_or_else(|| TunnelError::CloudflareApi("No result in response".to_string()))
    }

    /// Get tunnel information.
    pub async fn get_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<TunnelInfo, TunnelError> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}",
            Self::BASE_URL, account_id, tunnel_id
        );

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(TunnelError::CloudflareApi(format!(
                "Failed to get tunnel: {} - {}",
                status, text
            )));
        }

        let result: CloudflareResponse<TunnelInfo> =
            serde_json::from_str(&text).map_err(|e| TunnelError::CloudflareApi(e.to_string()))?;

        result
            .result
            .ok_or_else(|| TunnelError::CloudflareApi("No result in response".to_string()))
    }
}

/// Cloudflare API response wrapper.
#[derive(Debug, Deserialize)]
struct CloudflareResponse<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<CloudflareError>,
}

/// Cloudflare API error.
#[derive(Debug, Deserialize)]
struct CloudflareError {
    #[serde(default)]
    code: i32,
    message: String,
}

/// Request to create a DNS record.
#[derive(Debug, Serialize)]
struct CreateDnsRecordRequest {
    #[serde(rename = "type")]
    record_type: String,
    name: String,
    content: String,
    proxied: bool,
    ttl: u32,
}

/// DNS record response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DnsRecord {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: String,
    pub content: String,
    pub proxied: bool,
    pub ttl: u32,
}

/// Tunnel information.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TunnelInfo {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub connections: Vec<TunnelConnection>,
}

/// Tunnel connection information.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TunnelConnection {
    pub colo_name: String,
    pub is_pending_reconnect: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cloudflare_client_creation() {
        let client = CloudflareClient::new("test-token", "test-zone");
        assert_eq!(client.api_token, "test-token");
        assert_eq!(client.zone_id, "test-zone");
    }
}
