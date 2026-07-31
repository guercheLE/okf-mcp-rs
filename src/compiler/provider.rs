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

struct ProviderSpec {
    adapter_kind: AdapterKind,
    /// `None` for providers that need no key (Ollama, run locally).
    api_key_env: Option<&'static str>,
    /// Env var name that, if set, overrides `default_base_url`.
    base_url_env: Option<&'static str>,
    /// `None` only for `custom`, which has no sensible default — it must
    /// come from `base_url_env` or an explicit per-call override.
    default_base_url: Option<&'static str>,
}

fn provider_spec(provider: &str) -> anyhow::Result<ProviderSpec> {
    Ok(match provider {
        "anthropic" => ProviderSpec {
            adapter_kind: AdapterKind::Anthropic,
            api_key_env: Some("ANTHROPIC_API_KEY"),
            base_url_env: None,
            default_base_url: Some("https://api.anthropic.com"),
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
            "unknown LLM provider '{other}' — supported: anthropic, openai, groq, ollama, custom"
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

/// If `env_name` isn't already set, seeds it from the OS
/// keychain/encrypted-file credential saved under account `"llm-<provider>"`
/// by `okf-mcp setup` — so a key saved there works without the caller
/// having to `export` it manually first.
fn seed_env_from_credential_storage(env_name: &str, provider: &str) {
    if std::env::var(env_name).is_ok() {
        return;
    }
    if let Ok(Some(key)) = load_credential(&format!("llm-{provider}")) {
        // SAFETY: called before any concurrent work touches this process's
        // env vars — `compile`/`rebuild` resolve the model spec once, up
        // front, before spawning any per-source work.
        unsafe {
            std::env::set_var(env_name, key);
        }
    }
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
        Self {
            client: Client::default(),
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
    ) -> anyhow::Result<String> {
        let (provider, model_name) = parse_model_spec(full_model_spec)?;
        let spec = provider_spec(provider)?;

        if let Some(env_name) = spec.api_key_env {
            seed_env_from_credential_storage(env_name, provider);
        }

        let auth = match spec.api_key_env {
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
        let endpoint = Endpoint::from_owned(base_url);

        let target = ServiceTarget {
            endpoint,
            auth,
            model: ModelIden::new(spec.adapter_kind, model_name),
        };

        let chat_req = ChatRequest::new(vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_prompt),
        ]);

        let mut options = ChatOptions::default();
        if let Some(temperature) = temperature {
            options = options.with_temperature(temperature as f64);
        }

        let response = self.client.exec_chat(target, chat_req, Some(&options)).await?;
        response
            .first_text()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("LLM returned an empty response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for provider in ["anthropic", "openai", "groq", "ollama", "custom"] {
            assert!(provider_spec(provider).is_ok(), "{provider} should resolve");
        }
    }

    #[test]
    fn ollama_default_base_url_ends_with_slash() {
        let spec = provider_spec("ollama").unwrap();
        assert_eq!(spec.default_base_url, Some("http://localhost:11434/"));
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
}
