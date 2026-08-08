//! Multi-provider LLM routing for `okf-mcp compile`/`rebuild`, via the
//! `genai` crate. `--model`/the MCP `model` arg always requires an explicit
//! `<provider>/<model_name>` prefix — no "guess the provider from a bare
//! model name" fallback, since that's ambiguous whenever more than one
//! provider's API key happens to be configured at once.

use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget};

use crate::core::credential_storage::load_credential;

/// Every provider `provider_spec` knows how to route — shared by
/// `test_connection`'s multi-provider probe and `cli::config`'s
/// credential-presence listing, so both stay in sync with this module's
/// actual routing table instead of re-declaring the list a third time.
pub const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "groq",
    "gemini",
    "openrouter",
    "deepseek",
    "xai",
    "together",
    "ollama-cloud",
    "moonshot",
    "ollama",
    "foundry-local",
    "claude-max-api-proxy",
    "custom",
];

pub struct ProviderSpec {
    pub adapter_kind: AdapterKind,
    /// `None` for providers that need no key (Ollama, run locally).
    pub api_key_env: Option<&'static str>,
    /// Env var name that, if set, overrides `default_base_url`.
    base_url_env: Option<&'static str>,
    /// `None` only for `custom`, which has no sensible default — it must
    /// come from `base_url_env` or an explicit per-call override.
    default_base_url: Option<&'static str>,
}

pub fn provider_spec(provider: &str) -> anyhow::Result<ProviderSpec> {
    Ok(match provider {
        "anthropic" => ProviderSpec {
            adapter_kind: AdapterKind::Anthropic,
            api_key_env: Some("ANTHROPIC_API_KEY"),
            base_url_env: None,
            // Must include /v1 — genai's Anthropic adapter builds the
            // request URL via simple concatenation (`{base_url}messages`),
            // so a base URL missing the /v1 segment silently produces a
            // 404 on a route that doesn't exist, with an empty body.
            // Matches genai's own AnthropicAdapter::default_endpoint().
            default_base_url: Some("https://api.anthropic.com/v1/"),
        },
        "openai" => ProviderSpec {
            adapter_kind: AdapterKind::OpenAI,
            api_key_env: Some("OPENAI_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.openai.com/v1"),
        },
        "groq" => ProviderSpec {
            adapter_kind: AdapterKind::Groq,
            api_key_env: Some("GROQ_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.groq.com/openai/v1"),
        },
        "gemini" => ProviderSpec {
            adapter_kind: AdapterKind::Gemini,
            api_key_env: Some("GEMINI_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://generativelanguage.googleapis.com/v1beta/"),
        },
        // A gateway to many providers/models behind one key — model names
        // are namespaced by their origin provider, e.g.
        // "openrouter/anthropic/claude-sonnet-4-5".
        "openrouter" => ProviderSpec {
            adapter_kind: AdapterKind::OpenRouter,
            api_key_env: Some("OPEN_ROUTER_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://openrouter.ai/api/v1/"),
        },
        "deepseek" => ProviderSpec {
            adapter_kind: AdapterKind::DeepSeek,
            api_key_env: Some("DEEPSEEK_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.deepseek.com/v1/"),
        },
        "xai" => ProviderSpec {
            adapter_kind: AdapterKind::Xai,
            api_key_env: Some("XAI_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.x.ai/v1/"),
        },
        "together" => ProviderSpec {
            adapter_kind: AdapterKind::Together,
            api_key_env: Some("TOGETHER_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.together.xyz/v1/"),
        },
        // The cloud-hosted sibling of "ollama" (run.ollama.com) — same
        // Ollama protocol, remote instead of local, so it needs a key.
        "ollama-cloud" => ProviderSpec {
            adapter_kind: AdapterKind::OllamaCloud,
            api_key_env: Some("OLLAMA_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://ollama.com/"),
        },
        // Moonshot AI — makers of the Kimi model family.
        "moonshot" => ProviderSpec {
            adapter_kind: AdapterKind::Moonshot,
            api_key_env: Some("MOONSHOT_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.moonshot.cn/v1/"),
        },
        "ollama" => ProviderSpec {
            adapter_kind: AdapterKind::Ollama,
            api_key_env: None,
            base_url_env: Some("OLLAMA_HOST"),
            default_base_url: Some("http://localhost:11434/"),
        },
        // Microsoft Foundry Local — a locally-run, OpenAI-compatible
        // endpoint (no dedicated genai adapter exists for it, so it reuses
        // the OpenAI adapter's wire protocol like "custom" does). No
        // hardcoded default: Foundry Local's port is dynamic per-machine
        // (discovered via `foundry service status`), so it must be set via
        // FOUNDRY_LOCAL_ENDPOINT or an explicit override — same reasoning
        // as "custom" having no default_base_url.
        "foundry-local" => ProviderSpec {
            adapter_kind: AdapterKind::OpenAI,
            api_key_env: None,
            base_url_env: Some("FOUNDRY_LOCAL_ENDPOINT"),
            default_base_url: None,
        },
        // claude-max-api-proxy (github.com/sethschnrt/claude-max-api-proxy
        // and forks) — a local OpenAI-compatible server that shells out to
        // the authenticated `claude` CLI, letting a Claude Pro/Max
        // *subscription* (not an API key) serve `--model` requests with no
        // per-token billing. No key needed: the proxy itself authenticates
        // via the CLI's own logged-in session. Its documented default port
        // is 3456, but the proxy is commonly rebound to 11434 to match
        // Ollama's port for tools that hardcode it — either way the port is
        // a local run-time choice, not something this crate can know in
        // advance, so (like foundry-local) it's overridable via env var.
        "claude-max-api-proxy" => ProviderSpec {
            adapter_kind: AdapterKind::OpenAI,
            api_key_env: None,
            base_url_env: Some("CLAUDE_MAX_API_PROXY_ENDPOINT"),
            default_base_url: Some("http://localhost:11434/v1"),
        },
        // Any other OpenAI-compatible endpoint (self-hosted vLLM, etc.) —
        // reuses the OpenAI adapter's wire protocol against a custom URL.
        "custom" => ProviderSpec {
            adapter_kind: AdapterKind::OpenAI,
            api_key_env: Some("CUSTOM_LLM_API_KEY"),
            base_url_env: Some("CUSTOM_LLM_BASE_URL"),
            default_base_url: None,
        },
        other => anyhow::bail!(
            "unknown LLM provider '{other}' — supported: {}",
            KNOWN_PROVIDERS.join(", ")
        ),
    })
}

/// Splits `"<provider>/<model_name>"`. No fallback for a bare model name —
/// see this module's top comment for why.
pub fn parse_model_spec(spec: &str) -> anyhow::Result<(&str, &str)> {
    spec.split_once('/').filter(|(provider, model)| !provider.is_empty() && !model.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "model must be '<provider>/<model_name>' (e.g. 'anthropic/claude-3-5-sonnet'), got '{spec}'"
        )
    })
}

/// Whether `name` is set to a real, non-blank value — `std::env::var`
/// alone returns `Ok("")` for a var that's set but empty, which would
/// otherwise make `seed_env_from_credential_storage` treat a stray blank
/// env var (leftover shell export, inherited from an MCP host's spawn
/// env, etc.) as "already configured" and silently skip the correctly
/// saved credential.
pub fn env_var_is_meaningfully_set(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// If `env_name` isn't meaningfully set, seeds it from the OS
/// keychain/encrypted-file credential saved under account `"llm-<provider>"`
/// by `okf-mcp setup` — so a key saved there works without the caller
/// having to `export` it manually first.
fn seed_env_from_credential_storage(env_name: &str, provider: &str) {
    if env_var_is_meaningfully_set(env_name) {
        return;
    }
    if let Ok(Some(key)) = load_credential(&format!("llm-{provider}")) {
        // SAFETY: called before any concurrent work touches this process's
        // env vars — `compile`/`rebuild` resolve the model spec once, up
        // front, before spawning any per-source work.
        unsafe {
            std::env::set_var(env_name, key.trim());
        }
    }
}

/// Deterministic per-name env var for an ad-hoc/named custom provider's API
/// key, e.g. `OKF_CUSTOM_PROVIDER_API_KEY__MYVLLM` for a provider named
/// `"myvllm"` — the single source of truth for this format, used by
/// `resolve_provider_target`'s fallback branch below when no explicit
/// `api_key_env_override` is supplied, and by `setup_wizard.rs` when
/// building that provider's reveal-gate env key, so the two can never
/// drift.
pub fn synthetic_api_key_env_name(provider: &str) -> String {
    let sanitized: String = provider
        .to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("OKF_CUSTOM_PROVIDER_API_KEY__{sanitized}")
}

/// Resolves everything `genai` needs to target a provider — the adapter
/// kind, endpoint (scheme/trailing-slash normalized), and auth — from
/// `provider_spec`'s hardcoded defaults, an optional vault-config
/// `base_url_override`/`api_key_env_override`, and (for the API key) the
/// OS keychain/encrypted-file fallback `okf-mcp setup` writes to. Shared
/// by `execute_compile_prompt` and `test_connection`'s per-provider probe
/// so both resolve a provider's target identically.
///
/// A `provider` name outside `KNOWN_PROVIDERS` (a named custom provider
/// configured via `okf-mcp setup`) is not an error by itself — it's only
/// rejected when there's also no `base_url_override` to fall back on. When
/// an override *is* present, it's treated as an ad-hoc OpenAI-compatible
/// provider (same adapter as the hardcoded `"custom"` slot), which is what
/// lets `--model <name>/<model>` work for any custom provider `okf-mcp
/// setup` saved, not just the one hardcoded `"custom"` name.
pub fn resolve_provider_target(
    provider: &str,
    base_url_override: Option<&str>,
    api_key_env_override: Option<&str>,
) -> anyhow::Result<(AdapterKind, Endpoint, AuthData)> {
    let known_spec = provider_spec(provider);
    if base_url_override.is_none()
        && let Err(err) = known_spec
    {
        // No override to fall back on — surface the exact original error
        // (a genuinely unknown provider with nothing to route it to).
        return Err(err);
    }

    let (adapter_kind, hardcoded_api_key_env, base_url_env, default_base_url) = match &known_spec {
        Ok(spec) => (
            spec.adapter_kind,
            spec.api_key_env,
            spec.base_url_env,
            spec.default_base_url,
        ),
        // Unrecognized name + an override present: treat as an ad-hoc
        // OpenAI-compatible custom provider.
        Err(_) => (AdapterKind::OpenAI, None, None, None),
    };

    // A vault's `.okf/config.toml` [providers.<name>].api_key_env, if set,
    // takes precedence over the hardcoded default for this provider; an
    // unrecognized name with no such override falls back to the
    // deterministic synthetic name so its keychain-saved key still seeds.
    let api_key_env: Option<String> = api_key_env_override
        .map(str::to_string)
        .or_else(|| hardcoded_api_key_env.map(str::to_string))
        .or_else(|| {
            known_spec
                .is_err()
                .then(|| synthetic_api_key_env_name(provider))
        });

    if let Some(env_name) = &api_key_env {
        seed_env_from_credential_storage(env_name, provider);
    }

    let auth = match &api_key_env {
        Some(env_name) => AuthData::from_env(env_name.as_str()),
        // `AdapterKind::OpenAI`'s shared request-building path always
        // resolves `AuthData::single_key_value()` to build the
        // `Authorization` header, which errors
        // (`ResolverAuthDataNotSingleValue`) for `AuthData::None` — unlike
        // `AdapterKind::Ollama`'s adapter, which never calls it, so
        // `ollama`'s own "no key needed" case has always worked. A
        // placeholder single value keeps that header-building step happy
        // for other no-key OpenAI-compatible providers (`foundry-local`
        // today); a local, auth-less server simply ignores whatever's in
        // the header it never checks.
        None if adapter_kind == AdapterKind::OpenAI => AuthData::from_single("not-needed"),
        None => AuthData::None,
    };

    let mut base_url = base_url_override
        .map(str::to_string)
        .or_else(|| base_url_env.and_then(|env_name| std::env::var(env_name).ok()))
        .or_else(|| default_base_url.map(str::to_string))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "provider '{provider}' has no default base URL — set {} or pass a base URL override",
                base_url_env.unwrap_or("its base URL")
            )
        })?;

    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        base_url = format!("http://{base_url}");
    }
    if !base_url.ends_with('/') {
        base_url.push('/');
    }

    Ok((adapter_kind, Endpoint::from_owned(base_url), auth))
}

/// Whether `err` looks like the provider rejected the *model name* itself
/// (as opposed to e.g. an auth failure or a network error) — an HTTP
/// 404/400 whose body mentions "model". Used to decide whether to append
/// the provider's available-model list to the error.
fn looks_like_model_not_found(err: &genai::Error) -> bool {
    let status_body = match err {
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { status, body, .. } => Some((*status, body)),
            _ => None,
        },
        _ => None,
    };
    status_body.is_some_and(|(status, body)| {
        (status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::BAD_REQUEST)
            && body.to_lowercase().contains("model")
    })
}

/// `genai::Client::default()` builds its underlying `reqwest::Client` with
/// no request timeout at all (confirmed in genai 0.6.5's
/// `WebClient::default()`/`WebConfig::default()` — `timeout: None`), so a
/// stalled or never-responding provider (e.g. a rate-limited/queued free
/// OpenRouter model) hangs `compile`/`rebuild`/`run` forever with no way to
/// distinguish "still working" from "will never return". Generous enough
/// not to false-positive on a slow local Ollama model on constrained
/// hardware (see `--model`'s own help text on 12B+ being "very slow"), but
/// bounded so a stuck request eventually surfaces as a normal error instead
/// of an indefinite hang.
const LLM_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Builds the `ChatOptions` `execute_compile_prompt` sends with each chat
/// request — pulled out as its own pure function so `CompileOptions`'s
/// `temperature`/`max_tokens` plumbing into `genai`'s `ChatOptions` builder
/// can be unit-tested directly (via `ChatOptions`'s own public fields)
/// without a real/mocked network call.
fn build_chat_options(temperature: Option<f32>, max_tokens: Option<u32>) -> ChatOptions {
    let mut options = ChatOptions::default();
    if let Some(temperature) = temperature {
        options = options.with_temperature(temperature as f64);
    }
    if let Some(max_tokens) = max_tokens {
        options = options.with_max_tokens(max_tokens);
    }
    options
}

pub struct LLMCompilerDriver {
    client: Client,
}

impl Default for LLMCompilerDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl LLMCompilerDriver {
    pub fn new() -> Self {
        let web_config = genai::WebConfig::default().with_timeout(LLM_REQUEST_TIMEOUT);
        Self {
            client: Client::builder().with_web_config(web_config).build(),
        }
    }

    /// Runs one system+user prompt through the resolved provider, returning
    /// the model's raw text response (compile::operations is responsible
    /// for parsing it as a `CompilePayload`).
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_compile_prompt(
        &self,
        full_model_spec: &str,
        system_prompt: &str,
        user_prompt: &str,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        base_url_override: Option<&str>,
        api_key_env_override: Option<&str>,
    ) -> anyhow::Result<String> {
        let (provider, model_name) = parse_model_spec(full_model_spec)?;
        let (adapter_kind, endpoint, auth) =
            resolve_provider_target(provider, base_url_override, api_key_env_override)?;

        let target = ServiceTarget {
            endpoint: endpoint.clone(),
            auth: auth.clone(),
            model: ModelIden::new(adapter_kind, model_name),
        };

        let chat_req = ChatRequest::new(vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_prompt),
        ]);

        let options = build_chat_options(temperature, max_tokens);

        let response = match self
            .client
            .exec_chat(target, chat_req, Some(&options))
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let mut message = err.to_string();
                if looks_like_model_not_found(&err) {
                    match self
                        .client
                        .all_model_names(adapter_kind, (endpoint, auth))
                        .await
                    {
                        Ok(models) => message.push_str(&format!(
                            "\n\navailable models for '{provider}': {}",
                            models.join(", ")
                        )),
                        Err(list_err) => message.push_str(&format!(
                            "\n\n(could not list available models for '{provider}': {list_err})"
                        )),
                    }
                }
                return Err(anyhow::anyhow!(message));
            }
        };
        response
            .first_text()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("LLM returned an empty response"))
    }

    /// Lists a provider's available model names — same target resolution
    /// as `execute_compile_prompt` (vault-config overrides, keychain-seeded
    /// credentials), but without sending a chat request. Per `genai`'s own
    /// doc comment on `all_model_names`: only the Ollama adapter does a
    /// live query against its host; the other adapters return a static,
    /// crate-baked-in list — a successful call for those only proves the
    /// endpoint/auth resolved, not that the key is actually valid.
    pub async fn list_models(
        &self,
        provider: &str,
        base_url_override: Option<&str>,
        api_key_env_override: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let (adapter_kind, endpoint, auth) =
            resolve_provider_target(provider, base_url_override, api_key_env_override)?;
        self.client
            .all_model_names(adapter_kind, (endpoint, auth))
            .await
            .map_err(|err| anyhow::anyhow!("could not list models for '{provider}': {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_model_call_error(status: reqwest::StatusCode, body: &str) -> genai::Error {
        genai::Error::WebModelCall {
            model_iden: ModelIden::new(AdapterKind::Anthropic, "claude-bogus"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status,
                body: body.to_string(),
                headers: Box::new(reqwest::header::HeaderMap::new()),
            },
        }
    }

    #[test]
    fn looks_like_model_not_found_matches_a_404_mentioning_model() {
        let err = web_model_call_error(
            reqwest::StatusCode::NOT_FOUND,
            "model: claude-bogus not found",
        );
        assert!(looks_like_model_not_found(&err));
    }

    #[test]
    fn looks_like_model_not_found_matches_a_400_mentioning_model_case_insensitively() {
        let err = web_model_call_error(
            reqwest::StatusCode::BAD_REQUEST,
            "The Model 'claude-bogus' does not exist",
        );
        assert!(looks_like_model_not_found(&err));
    }

    #[test]
    fn looks_like_model_not_found_does_not_match_an_unrelated_404() {
        let err = web_model_call_error(reqwest::StatusCode::NOT_FOUND, "route not found");
        assert!(!looks_like_model_not_found(&err));
    }

    #[test]
    fn looks_like_model_not_found_does_not_match_a_500() {
        let err = web_model_call_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "model unavailable",
        );
        assert!(!looks_like_model_not_found(&err));
    }

    #[test]
    fn llm_compiler_driver_builds_with_a_bounded_request_timeout() {
        // Regression guard: `genai::Client::default()` has no request
        // timeout at all, which let a stalled/never-responding provider
        // (e.g. a queued free-tier model) hang `compile` indefinitely.
        // `LLMCompilerDriver::new()` must not regress to the timeout-less
        // default — this doesn't exercise the network path (that needs a
        // live/mock server and 5 real minutes to observe), but it pins the
        // constant so a future refactor can't silently drop the
        // `.with_web_config(...)` call and reintroduce the hang.
        assert_eq!(LLM_REQUEST_TIMEOUT, std::time::Duration::from_secs(300));
        let _driver = LLMCompilerDriver::new();
    }

    /// Minimal HTTP/1.1 server returning a fixed response to the first
    /// request it receives, matching `cli::test_connection`'s own mock
    /// server pattern.
    async fn mock_server(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn list_models_returns_ollamas_live_model_list() {
        // Ollama's adapter is the one genai adapter that does a real
        // network call for `all_model_names` (`GET {base_url}api/tags`) —
        // exercising it end-to-end here (mock server, no real Ollama
        // needed) covers `LLMCompilerDriver::list_models`'s full path:
        // `resolve_provider_target` -> `client.all_model_names`.
        let base_url = mock_server(
            "200 OK",
            r#"{"models":[{"name":"llama3.1:8b"},{"name":"llama3.1:70b"}]}"#,
        )
        .await;

        let driver = LLMCompilerDriver::new();
        let models = driver
            .list_models("ollama", Some(&base_url), None)
            .await
            .unwrap();
        assert_eq!(models, vec!["llama3.1:8b", "llama3.1:70b"]);
    }

    #[tokio::test]
    async fn list_models_rejects_an_unknown_provider() {
        let driver = LLMCompilerDriver::new();
        let err = driver
            .list_models("not-a-real-provider", None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown LLM provider"));
    }

    #[tokio::test]
    async fn list_models_for_custom_without_a_base_url_fails_clearly() {
        // "custom" has no hardcoded default base URL — without a vault
        // override or CUSTOM_LLM_BASE_URL set, resolution must fail with a
        // clear message rather than panicking or hanging.
        // SAFETY: test-only env mutation; ensures a stray var from another
        // test/process doesn't make this one flaky.
        unsafe {
            std::env::remove_var("CUSTOM_LLM_BASE_URL");
        }
        let driver = LLMCompilerDriver::new();
        let err = driver.list_models("custom", None, None).await.unwrap_err();
        assert!(err.to_string().contains("no default base URL"));
    }

    #[test]
    fn resolve_provider_target_treats_an_unrecognized_name_with_an_override_as_openai_compatible() {
        let (adapter_kind, endpoint, auth) = resolve_provider_target(
            "totally-unknown-provider",
            Some("https://example.com/v1"),
            None,
        )
        .unwrap();
        assert_eq!(adapter_kind, AdapterKind::OpenAI);
        assert_eq!(endpoint.base_url(), "https://example.com/v1/");
        match auth {
            AuthData::FromEnv(env_name) => {
                assert_eq!(
                    env_name,
                    synthetic_api_key_env_name("totally-unknown-provider")
                );
            }
            _ => panic!("expected AuthData::FromEnv"),
        }
    }

    #[test]
    fn resolve_provider_target_prefers_an_explicit_api_key_env_override_over_the_synthetic_name() {
        let (_, _, auth) = resolve_provider_target(
            "another-unrecognized-provider",
            Some("https://example.com/v1"),
            Some("SOME_EXPLICIT_ENV_VAR"),
        )
        .unwrap();
        match auth {
            AuthData::FromEnv(env_name) => assert_eq!(env_name, "SOME_EXPLICIT_ENV_VAR"),
            _ => panic!("expected AuthData::FromEnv"),
        }
    }

    #[test]
    fn resolve_provider_target_still_rejects_a_genuinely_unknown_provider_with_no_override() {
        let err = resolve_provider_target("not-a-real-provider", None, None).unwrap_err();
        assert!(err.to_string().contains("unknown LLM provider"));
    }

    #[test]
    fn resolve_provider_target_gives_a_no_key_openai_adapter_provider_a_resolvable_auth_value() {
        // Regression test: `foundry-local` (api_key_env: None,
        // AdapterKind::OpenAI) used to resolve to `AuthData::None`, which
        // the shared OpenAI-adapter request path can't turn into an
        // `Authorization` header — every real compile call against it
        // failed with `ResolverAuthDataNotSingleValue` before this fix,
        // even though the provider needs no key at all.
        unsafe {
            std::env::set_var("FOUNDRY_LOCAL_ENDPOINT", "http://localhost:5272/v1/");
        }
        let (adapter_kind, _, auth) = resolve_provider_target("foundry-local", None, None).unwrap();
        assert_eq!(adapter_kind, AdapterKind::OpenAI);
        assert!(
            auth.single_key_value().is_ok(),
            "AuthData must resolve to a single value so the OpenAI adapter's \
             Authorization-header-building step doesn't error"
        );
        unsafe {
            std::env::remove_var("FOUNDRY_LOCAL_ENDPOINT");
        }
    }

    #[test]
    fn claude_max_api_proxy_default_base_url_matches_the_ollama_port_convention() {
        // Pinned deliberately: the proxy's own documented default is 3456,
        // but it's commonly rebound to 11434 (Ollama's port) — this
        // provider's default follows that common setup, same reasoning as
        // foundry-local having no hardcoded default at all (dynamic port).
        let spec = provider_spec("claude-max-api-proxy").unwrap();
        assert_eq!(spec.default_base_url, Some("http://localhost:11434/v1"));
        assert_eq!(spec.api_key_env, None);
        assert_eq!(spec.base_url_env, Some("CLAUDE_MAX_API_PROXY_ENDPOINT"));
    }

    #[test]
    fn resolve_provider_target_resolves_claude_max_api_proxy_default_and_override() {
        // Combines the no-key-auth-resolves regression (same class as
        // foundry-local's own test above: AdapterKind::OpenAI with no key
        // must not resolve to AuthData::None, or every real request fails
        // with ResolverAuthDataNotSingleValue) with the env-var-override
        // check, in one test — both touch the same process-global env var,
        // and cargo test runs tests concurrently by default, so splitting
        // them into two tests would race on CLAUDE_MAX_API_PROXY_ENDPOINT.
        unsafe {
            std::env::remove_var("CLAUDE_MAX_API_PROXY_ENDPOINT");
        }
        let (adapter_kind, endpoint, auth) =
            resolve_provider_target("claude-max-api-proxy", None, None).unwrap();
        assert_eq!(adapter_kind, AdapterKind::OpenAI);
        assert_eq!(endpoint.base_url(), "http://localhost:11434/v1/");
        assert!(
            auth.single_key_value().is_ok(),
            "AuthData must resolve to a single value so the OpenAI adapter's \
             Authorization-header-building step doesn't error"
        );

        // The env var exists specifically because the proxy's actual port is
        // a local run-time choice (3456 default, 11434 when rebound to match
        // Ollama, or anything else) — must be overridable like foundry-local.
        unsafe {
            std::env::set_var("CLAUDE_MAX_API_PROXY_ENDPOINT", "http://localhost:3456/v1");
        }
        let (_, endpoint, _) = resolve_provider_target("claude-max-api-proxy", None, None).unwrap();
        assert_eq!(endpoint.base_url(), "http://localhost:3456/v1/");
        unsafe {
            std::env::remove_var("CLAUDE_MAX_API_PROXY_ENDPOINT");
        }
    }

    #[test]
    fn resolve_provider_target_leaves_ollamas_no_key_auth_data_as_none() {
        // Ollama's own adapter never calls `AuthData::single_key_value()`,
        // so it must keep using `AuthData::None` unchanged — this pins
        // that the OpenAI-adapter-specific fix above doesn't also touch
        // Ollama's already-working no-key path.
        let (adapter_kind, _, auth) = resolve_provider_target("ollama", None, None).unwrap();
        assert_eq!(adapter_kind, AdapterKind::Ollama);
        assert!(matches!(auth, AuthData::None));
    }

    #[test]
    fn looks_like_model_not_found_does_not_match_a_non_web_call_error() {
        let err = genai::Error::NoAuthData {
            model_iden: ModelIden::new(AdapterKind::Anthropic, "claude-bogus"),
        };
        assert!(!looks_like_model_not_found(&err));
    }

    #[test]
    fn build_chat_options_sets_max_tokens_when_configured() {
        // Regression guard for the `CompileOptions::max_tokens` ->
        // `ChatOptions::with_max_tokens` plumbing — `.okf/config.toml`'s
        // `[compiler].max_tokens` must actually reach the outgoing request.
        let options = build_chat_options(None, Some(2048));
        assert_eq!(options.max_tokens, Some(2048));
        assert_eq!(options.temperature, None);
    }

    #[test]
    fn build_chat_options_sets_both_temperature_and_max_tokens_when_both_are_configured() {
        let options = build_chat_options(Some(0.5), Some(4096));
        assert_eq!(options.temperature, Some(0.5));
        assert_eq!(options.max_tokens, Some(4096));
    }

    #[test]
    fn build_chat_options_leaves_both_unset_when_neither_is_configured() {
        let options = build_chat_options(None, None);
        assert_eq!(options.temperature, None);
        assert_eq!(options.max_tokens, None);
    }

    #[test]
    fn parse_model_spec_splits_provider_and_model() {
        assert_eq!(
            parse_model_spec("anthropic/claude-3-5-sonnet").unwrap(),
            ("anthropic", "claude-3-5-sonnet")
        );
    }

    #[test]
    fn parse_model_spec_rejects_a_bare_model_name() {
        assert!(parse_model_spec("gpt-4o").is_err());
    }

    #[test]
    fn parse_model_spec_rejects_an_empty_provider_or_model() {
        assert!(parse_model_spec("/claude-3-5-sonnet").is_err());
        assert!(parse_model_spec("anthropic/").is_err());
    }

    #[test]
    fn parse_model_spec_allows_a_model_name_that_itself_contains_a_slash() {
        // e.g. custom/my-org/my-model — split_once takes the FIRST slash.
        assert_eq!(
            parse_model_spec("custom/my-org/my-model").unwrap(),
            ("custom", "my-org/my-model")
        );
    }

    #[test]
    fn provider_spec_rejects_an_unknown_provider() {
        assert!(provider_spec("not-a-real-provider").is_err());
    }

    #[test]
    fn provider_spec_covers_every_documented_provider() {
        for provider in KNOWN_PROVIDERS {
            assert!(provider_spec(provider).is_ok(), "{provider} should resolve");
        }
    }

    #[test]
    fn ollama_default_base_url_ends_with_slash() {
        let spec = provider_spec("ollama").unwrap();
        assert_eq!(spec.default_base_url, Some("http://localhost:11434/"));
    }

    #[test]
    fn anthropic_default_base_url_includes_the_v1_path_segment() {
        // Regression test: genai's Anthropic adapter builds the request URL
        // via `{base_url}messages` string concatenation — a base URL
        // missing /v1 silently produces a 404 on a nonexistent route
        // (https://api.anthropic.com/messages) instead of the real one
        // (https://api.anthropic.com/v1/messages).
        let spec = provider_spec("anthropic").unwrap();
        assert_eq!(spec.default_base_url, Some("https://api.anthropic.com/v1/"));
    }

    #[test]
    fn new_providers_default_base_urls_match_genais_own_adapter_defaults() {
        // Each pinned against genai 0.6.5's own AdapterImpl::default_endpoint()
        // for that adapter, the same class of check that caught item 3's
        // Anthropic /v1 bug — a value that diverges from genai's own
        // default should be a deliberate, documented choice, not a typo.
        let cases = [
            (
                "gemini",
                "https://generativelanguage.googleapis.com/v1beta/",
            ),
            ("openrouter", "https://openrouter.ai/api/v1/"),
            ("deepseek", "https://api.deepseek.com/v1/"),
            ("xai", "https://api.x.ai/v1/"),
            ("together", "https://api.together.xyz/v1/"),
            ("ollama-cloud", "https://ollama.com/"),
            ("moonshot", "https://api.moonshot.cn/v1/"),
        ];
        for (provider, expected_url) in cases {
            let spec = provider_spec(provider).unwrap();
            assert_eq!(
                spec.default_base_url,
                Some(expected_url),
                "{provider}'s default_base_url"
            );
        }
    }

    #[test]
    fn new_providers_use_their_documented_api_key_env_var() {
        let cases = [
            ("gemini", "GEMINI_API_KEY"),
            ("openrouter", "OPEN_ROUTER_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("xai", "XAI_API_KEY"),
            ("together", "TOGETHER_API_KEY"),
            ("ollama-cloud", "OLLAMA_API_KEY"),
            ("moonshot", "MOONSHOT_API_KEY"),
        ];
        for (provider, expected_env) in cases {
            let spec = provider_spec(provider).unwrap();
            assert_eq!(
                spec.api_key_env,
                Some(expected_env),
                "{provider}'s api_key_env"
            );
        }
    }

    #[test]
    fn seed_env_from_credential_storage_does_not_overwrite_an_already_set_var() {
        // SAFETY: test-only env mutation; this var name is unique to this
        // test so it can't race with other tests touching real provider vars.
        unsafe {
            std::env::set_var("OKF_TEST_SEED_VAR", "already-set");
        }
        seed_env_from_credential_storage("OKF_TEST_SEED_VAR", "does-not-matter");
        assert_eq!(std::env::var("OKF_TEST_SEED_VAR").unwrap(), "already-set");
        unsafe {
            std::env::remove_var("OKF_TEST_SEED_VAR");
        }
    }

    #[test]
    fn env_var_is_meaningfully_set_treats_blank_and_whitespace_only_as_unset() {
        // SAFETY: test-only env mutation; this var name is unique to this test.
        unsafe {
            std::env::remove_var("OKF_TEST_MEANINGFUL_VAR");
        }
        assert!(!env_var_is_meaningfully_set("OKF_TEST_MEANINGFUL_VAR"));

        unsafe {
            std::env::set_var("OKF_TEST_MEANINGFUL_VAR", "");
        }
        assert!(!env_var_is_meaningfully_set("OKF_TEST_MEANINGFUL_VAR"));

        unsafe {
            std::env::set_var("OKF_TEST_MEANINGFUL_VAR", "   ");
        }
        assert!(!env_var_is_meaningfully_set("OKF_TEST_MEANINGFUL_VAR"));

        unsafe {
            std::env::set_var("OKF_TEST_MEANINGFUL_VAR", "real-value");
        }
        assert!(env_var_is_meaningfully_set("OKF_TEST_MEANINGFUL_VAR"));

        unsafe {
            std::env::remove_var("OKF_TEST_MEANINGFUL_VAR");
        }
    }
}
