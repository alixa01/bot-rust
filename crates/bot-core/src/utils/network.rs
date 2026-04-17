use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Response};

pub async fn fetch_with_timeout(
    client: &Client,
    url: &str,
    timeout_ms: u64,
) -> Result<Response> {
    client
        .get(url)
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
        .with_context(|| format!("request failed for {url}"))
}
