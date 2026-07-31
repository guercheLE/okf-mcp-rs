//! HTTP dispatch for outbound calls to Firecrawl's API. The resilience
//! stack (rate limiter -> circuit breaker -> retrying `dispatch()`) is
//! generic and was originally shared by a now-removed any-operation proxy
//! (`ApiClient::execute`, parameterized by an `EndpointRecord` describing
//! an arbitrary operation) — that machinery existed to handle "any of N
//! declared parameters, generically," which is unnecessary now that this
//! client only ever calls one fixed Firecrawl endpoint. `scrape_url` below
//! is the sole entry point, but still built on the same
//! rate-limiter/circuit-breaker/`dispatch()` foundation.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::auth::auth_manager::AuthManager;
use crate::auth::request_credentials::RequestCredentials;
use crate::core::circuit_breaker::{CircuitBreaker, CircuitBreakerError};
use crate::core::config_schema::Config;
use crate::core::rate_limiter::RateLimiter;

pub struct ApiClient {
    config: Config,
    client: reqwest::Client,
    circuit_breaker: CircuitBreaker,
    rate_limiter: RateLimiter,
}

impl ApiClient {
    pub fn new(config: Config) -> Self {
        let rate_limiter = RateLimiter::new(config.rate_limit as usize, Duration::from_secs(1));
        Self {
            client: reqwest::Client::new(),
            circuit_breaker: CircuitBreaker::default(),
            rate_limiter,
            config,
        }
    }

    /// Fetches `url` via Firecrawl's own scrape endpoint and returns the raw
    /// JSON response (callers extract `data.markdown`).
    pub async fn scrape_url(
        &self,
        url: &str,
        auth_manager: &mut AuthManager,
        request_override: Option<&RequestCredentials>,
    ) -> anyhow::Result<Value> {
        self.rate_limiter.acquire()?;

        let request_url = format!(
            "{}/v2/scrape",
            self.config.firecrawl_base_url.trim_end_matches('/')
        );

        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert(
            "User-Agent".to_string(),
            "okf-mcp/0.1.0 (+https://github.com/guercheLE/okf-mcp-rs)".to_string(),
        );
        let headers = auth_manager
            .apply_auth_headers(
                headers,
                "POST",
                &request_url,
                self.config.transport,
                request_override,
            )
            .await?;

        let body = serde_json::json!({ "url": url });

        match self
            .circuit_breaker
            .execute(|| self.dispatch("POST", &request_url, Some(&body), &headers))
            .await
        {
            Ok(value) => Ok(value),
            Err(CircuitBreakerError::Open) => anyhow::bail!("circuit breaker is open"),
            Err(CircuitBreakerError::Inner(err)) => Err(err),
        }
    }

    async fn dispatch(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
        headers: &HashMap<String, String>,
    ) -> anyhow::Result<Value> {
        let parsed_method = reqwest::Method::from_bytes(method.as_bytes())?;

        let mut attempt = 0u32;
        loop {
            let mut request = self
                .client
                .request(parsed_method.clone(), url)
                .timeout(Duration::from_millis(self.config.timeout_ms));
            for (key, value) in headers {
                request = request.header(key, value);
            }
            if let Some(body) = body {
                request = request.json(body);
            } else if parsed_method != reqwest::Method::GET
                && parsed_method != reqwest::Method::HEAD
            {
                // Some APIs (e.g. Spotify's) 411 on a body-less PUT/POST/DELETE
                // with no Content-Length header — reqwest/hyper treats a
                // zero-length body the same as no body and still omits the
                // header on its own, so it has to be set explicitly.
                request = request
                    .header(reqwest::header::CONTENT_LENGTH, "0")
                    .body(Vec::new());
            }

            match request.send().await {
                Ok(response) => {
                    let value = response
                        .error_for_status()?
                        .json::<Value>()
                        .await
                        .unwrap_or(Value::Null);
                    return Ok(value);
                }
                Err(err) => {
                    attempt += 1;
                    if attempt > self.config.retry_attempts {
                        return Err(err.into());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::auth::auth_strategy::Credentials;
    use crate::core::config_schema::AuthMethod;

    async fn mock_http(
        status: &'static str,
        body: &'static str,
    ) -> (String, Arc<Mutex<String>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let request = captured.clone();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let headers_end = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                if let Some(headers_end) = headers_end {
                    let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= headers_end + content_length {
                        break;
                    }
                }
            }
            *request.lock().unwrap() = String::from_utf8_lossy(&bytes).into_owned();
            let wire = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
        });
        (format!("http://{address}"), captured, handle)
    }

    async fn disconnecting_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                drop(stream);
            }
        });
        format!("http://{address}")
    }

    fn client(url: String, retry_attempts: u32) -> ApiClient {
        let config: Config = serde_json::from_value(serde_json::json!({
            "url": url,
            "auth_method": "pat",
            "retry_attempts": retry_attempts,
            "timeout_ms": 200
        }))
        .unwrap();
        ApiClient::new(config)
    }

    fn firecrawl_client(base_url: String, retry_attempts: u32) -> ApiClient {
        let config: Config = serde_json::from_value(serde_json::json!({
            "url": "http://unused.invalid",
            "firecrawl_base_url": base_url,
            "auth_method": "pat",
            "retry_attempts": retry_attempts,
            "timeout_ms": 200
        }))
        .unwrap();
        ApiClient::new(config)
    }

    #[tokio::test]
    async fn dispatch_sends_json_and_empty_bodies_and_parses_responses() {
        let (url, request, server) = mock_http("200 OK", r#"{"ok":true}"#).await;
        let response = client(url.clone(), 0)
            .dispatch(
                "POST",
                &url,
                Some(&serde_json::json!({ "name": "coverage" })),
                &HashMap::from([("X-Coverage".to_string(), "yes".to_string())]),
            )
            .await
            .unwrap();
        assert_eq!(response, serde_json::json!({ "ok": true }));
        server.await.unwrap();
        {
            let request = request.lock().unwrap();
            assert!(request.contains(r#"{"name":"coverage"}"#));
            assert!(request.to_ascii_lowercase().contains("x-coverage: yes"));
        }

        let (url, request, server) = mock_http("204 No Content", "").await;
        let response = client(url.clone(), 0)
            .dispatch("DELETE", &url, None, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(response, Value::Null);
        server.await.unwrap();
        assert!(
            request
                .lock()
                .unwrap()
                .to_ascii_lowercase()
                .contains("content-length: 0")
        );
    }

    #[tokio::test]
    async fn dispatch_surfaces_method_status_and_retry_exhaustion_errors() {
        let local_url = disconnecting_server().await;
        let invalid_method = client(local_url.clone(), 0)
            .dispatch("NOT A METHOD", &local_url, None, &HashMap::new())
            .await;
        assert!(invalid_method.is_err());

        let (url, _, server) = mock_http("500 Internal Server Error", "{}").await;
        assert!(
            client(url.clone(), 0)
                .dispatch("GET", &url, None, &HashMap::new())
                .await
                .is_err()
        );
        server.await.unwrap();

        let local_url = disconnecting_server().await;
        assert!(
            client(local_url.clone(), 1)
                .dispatch("GET", &local_url, None, &HashMap::new())
                .await
                .is_err()
        );
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
    async fn scrape_url_posts_to_the_firecrawl_scrape_endpoint_with_auth_and_the_target_url() {
        let (base_url, request, server) =
            mock_http("200 OK", r##"{"success":true,"data":{"markdown":"# Hello"}}"##).await;
        let api_client = firecrawl_client(base_url, 0);
        let mut auth_manager = seeded_auth_manager();

        let response = api_client
            .scrape_url("https://example.com/docs", &mut auth_manager, None)
            .await
            .unwrap();
        assert_eq!(
            response,
            serde_json::json!({"success": true, "data": {"markdown": "# Hello"}})
        );
        server.await.unwrap();

        let request = request.lock().unwrap();
        assert!(request.contains("POST /v2/scrape HTTP/1.1"));
        assert!(request.contains(r#"{"url":"https://example.com/docs"}"#));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer s3cr3t")
        );
    }

    #[tokio::test]
    async fn scrape_url_over_http_transport_requires_a_request_override() {
        let mut manager = AuthManager::new(crate::core::config_schema::AuthMethod::Pat);
        let config: Config = serde_json::from_value(serde_json::json!({
            "url": "http://unused.invalid",
            "firecrawl_base_url": "http://unused.invalid",
            "auth_method": "pat",
            "transport": "http",
        }))
        .unwrap();
        let http_client = ApiClient::new(config);

        let result = http_client
            .scrape_url("https://example.com", &mut manager, None)
            .await;
        assert!(result.is_err());
    }
}
