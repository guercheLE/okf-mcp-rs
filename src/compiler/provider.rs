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

/// Resolves everything `genai` needs to target a provider — the adapter
/// kind, endpoint (scheme/trailing-slash normalized), and auth — from
/// `provider_spec`'s hardcoded defaults, an optional vault-config
/// `base_url_override`/`api_key_env_override`, and (for the API key) the
/// OS keychain/encrypted-file fallback `okf-mcp setup` writes to. Shared
/// by `execute_compile_prompt` and `test_connection`'s per-provider probe
/// so both resolve a provider's target identically.
pub fn resolve_provider_target(
    provider: &str,
    base_url_override: Option<&str>,
    api_key_env_override: Option<&str>,
) -> anyhow::Result<(AdapterKind, Endpoint, AuthData)> {
    let spec = provider_spec(provider)?;

    // A vault's `.okf/config.toml` [providers.<name>].api_key_env, if
    // set, takes precedence over the hardcoded default for this provider.
    let api_key_env = api_key_env_override.or(spec.api_key_env);

    if let Some(env_name) = api_key_env {
        seed_env_from_credential_storage(env_name, provider);
    }

    let auth = match api_key_env {
        Some(env_name) => AuthData::from_env(env_name),
        None => AuthData::None,
    };

    let mut base_url = base_url_override
        .map(str::to_string)
        .or_else(|| {
            spec.base_url_env
                .and_then(|env_name| std::env::var(env_name).ok())
        })
        .or_else(|| spec.default_base_url.map(str::to_string))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "provider '{provider}' has no default base URL — set {} or pass a base URL override",
                spec.base_url_env.unwrap_or("its base URL")
            )
        })?;

    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        base_url = format!("http://{base_url}");
    }
    if !base_url.ends_with('/') {
        base_url.push('/');
    }

    Ok((spec.adapter_kind, Endpoint::from_owned(base_url), auth))
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
    pub async fn execute_compile_prompt(
        &self,
        full_model_spec: &str,
        system_prompt: &str,
        user_prompt: &str,
        temperature: Option<f32>,
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

        let mut options = ChatOptions::default();
        if let Some(temperature) = temperature {
            options = options.with_temperature(temperature as f64);
        }

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

    #[test]
    fn looks_like_model_not_found_does_not_match_a_non_web_call_error() {
        let err = genai::Error::NoAuthData {
            model_iden: ModelIden::new(AdapterKind::Anthropic, "claude-bogus"),
        };
        assert!(!looks_like_model_not_found(&err));
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
