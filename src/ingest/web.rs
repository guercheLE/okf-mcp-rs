//! Web ingestion: fetches a URL via Firecrawl's scrape API and returns
//! clean markdown directly — Firecrawl already does HTML-to-markdown
//! conversion server-side, so no separate `readability`/`htmd`-style crate
//! is needed here.

use crate::auth::auth_manager::AuthManager;
use crate::auth::request_credentials::RequestCredentials;
use crate::core::config_schema::Config;
use crate::services::api_client::ApiClient;

pub async fn fetch_and_clean_url(
    url: &str,
    config: &Config,
    auth_manager: &mut AuthManager,
    request_override: Option<&RequestCredentials>,
) -> anyhow::Result<String> {
    let client = ApiClient::new(config.clone());
    let response = client
        .scrape_url(url, auth_manager, request_override)
        .await?;

    response
        .get("data")
        .and_then(|data| data.get("markdown"))
        .and_then(|markdown| markdown.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Firecrawl scrape response for '{url}' had no data.markdown field: {response}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::auth_strategy::Credentials;
    use crate::core::config_schema::AuthMethod;

    fn config_for(base_url: String) -> Config {
        serde_json::from_value(serde_json::json!({
            "url": "http://unused.invalid",
            "firecrawl_base_url": base_url,
            "auth_method": "pat",
        }))
        .unwrap()
    }

    fn seeded_auth_manager() -> AuthManager {
        let mut manager = AuthManager::new(AuthMethod::Pat);
        manager.set_credentials(Credentials::from([(
            "token".to_string(),
            "s3cr3t".to_string(),
        )]));
        manager
    }

    #[tokio::test]
    async fn extracts_markdown_from_a_successful_scrape_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            let body = r##"{"success":true,"data":{"markdown":"# Title\n\nBody"}}"##;
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
        });

        let config = config_for(format!("http://{address}"));
        let mut auth_manager = seeded_auth_manager();

        let markdown = fetch_and_clean_url("https://example.com", &config, &mut auth_manager, None)
            .await
            .unwrap();
        assert_eq!(markdown, "# Title\n\nBody");
    }

    #[tokio::test]
    async fn errors_when_the_response_has_no_markdown_field() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            let body = r#"{"success":false}"#;
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
        });

        let config = config_for(format!("http://{address}"));
        let mut auth_manager = seeded_auth_manager();

        let result =
            fetch_and_clean_url("https://example.com", &config, &mut auth_manager, None).await;
        assert!(result.is_err());
    }
}
