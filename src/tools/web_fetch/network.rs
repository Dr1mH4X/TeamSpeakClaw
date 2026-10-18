//! Public-address resolution and address-pinned HTTP transport for `web_fetch`.

use super::policy::{ip_literal, is_public_ip};
use crate::tools::http::USER_AGENT;
use anyhow::Result;
use std::net::SocketAddr;
use std::time::Duration;
use url::Url;

const DNS_TIMEOUT_SECS: u64 = 5;
const REQUEST_TIMEOUT_SECS: u64 = 20;

/// Build a client whose DNS answer set is validated and pinned for this request.
pub(crate) async fn request(url: &Url) -> Result<reqwest::Response> {
    let client = build_client(url).await?;
    client
        .get(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,text/*;q=0.9,application/json;q=0.8",
        )
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("web fetch timed out")
            } else {
                anyhow::anyhow!("web fetch failed: {error}")
            }
        })
}

async fn build_client(url: &Url) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .user_agent(USER_AGENT);

    if let Some(ip) = ip_literal(url) {
        if !is_public_ip(ip) {
            anyhow::bail!(
                "URL host {} is not a public address",
                url.host_str().unwrap_or("?")
            );
        }
        return builder
            .build()
            .map_err(|error| anyhow::anyhow!("web fetch client failed: {error}"));
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("URL has no port"))?;
    let addresses = resolve_public_addresses(host, port).await?;
    builder
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|error| anyhow::anyhow!("web fetch client failed: {error}"))
}

async fn resolve_public_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let resolved = tokio::time::timeout(
        Duration::from_secs(DNS_TIMEOUT_SECS),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("DNS lookup timed out for {host}"))?
    .map_err(|error| anyhow::anyhow!("DNS lookup failed for {host}: {error}"))?;

    let mut addresses = Vec::new();
    for address in resolved {
        if !is_public_ip(address.ip()) {
            anyhow::bail!("URL hostname {host} resolves to a non-public IP address");
        }
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        anyhow::bail!("URL hostname {host} resolved to no addresses");
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_loopback_before_opening_a_connection() {
        let url = Url::parse("http://127.0.0.1/").unwrap();
        let error = request(&url).await.unwrap_err().to_string();
        assert!(error.contains("not a public address"), "{error}");
    }

    #[tokio::test]
    async fn resolves_public_ip_literals_without_dns() {
        let url = Url::parse("https://8.8.8.8/").unwrap();
        assert!(build_client(&url).await.is_ok());
    }
}
