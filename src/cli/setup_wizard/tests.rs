//! Unit tests for the setup wizard — moved out of `setup_wizard.rs`
//! verbatim (just de-indented one level, since this is now the module
//! body rather than an inline `mod tests { ... }`). Private to
//! `setup_wizard`, so this still reaches every stage's items via
//! `super::*`.

use std::collections::{HashMap, HashSet};

use super::*;
use logic::{
    CustomProviderChoice, KnownProviderChoice, ProviderKeyResult, RevealableSecret,
    provider_default_indices, validate_custom_provider_name,
};
use okf_mcp::compiler::provider;
use okf_mcp::core::config_schema::{AuthMethod, CustomProviderEntry};
use okf_mcp::core::credential_storage::delete_credential;
use persistence::{persist_selection, persist_selection_at, write_config_yaml};
use prompts::{
    SecretDecision, prompt_custom_provider_name, prompt_provider_api_key, prompt_secret_decision,
};

/// A queue of canned answers, consumed in call order — lets
/// `prompt_*`/`run_setup_wizard_with` be tested end-to-end without a
/// real TTY. Panics (rather than erroring) on an exhausted queue or a
/// type mismatch, since either means the test's script doesn't match
/// what the code under test actually asked for.
#[derive(Debug)]
enum ScriptedAnswer {
    Text(String),
    Confirm(bool),
    Password(String),
    Select(&'static str),
    MultiSelect(Vec<&'static str>),
}

struct ScriptedPrompts {
    answers: std::sync::Mutex<std::collections::VecDeque<ScriptedAnswer>>,
}

impl ScriptedPrompts {
    fn new(answers: Vec<ScriptedAnswer>) -> Self {
        Self {
            answers: std::sync::Mutex::new(answers.into_iter().collect()),
        }
    }

    fn next(&self) -> ScriptedAnswer {
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("ScriptedPrompts ran out of scripted answers")
    }
}

#[async_trait]
impl Prompts for ScriptedPrompts {
    async fn text(&self, _message: String, _default: Option<String>) -> anyhow::Result<String> {
        match self.next() {
            ScriptedAnswer::Text(value) => Ok(value),
            other => panic!("expected a scripted Text answer, got {other:?}"),
        }
    }

    async fn confirm(&self, _message: String, _default: bool) -> anyhow::Result<bool> {
        match self.next() {
            ScriptedAnswer::Confirm(value) => Ok(value),
            other => panic!("expected a scripted Confirm answer, got {other:?}"),
        }
    }

    async fn password(&self, _message: String) -> anyhow::Result<String> {
        match self.next() {
            ScriptedAnswer::Password(value) => Ok(value),
            other => panic!("expected a scripted Password answer, got {other:?}"),
        }
    }

    async fn select(
        &self,
        _message: String,
        _choices: Vec<&'static str>,
        _starting_cursor: usize,
    ) -> anyhow::Result<&'static str> {
        match self.next() {
            ScriptedAnswer::Select(value) => Ok(value),
            other => panic!("expected a scripted Select answer, got {other:?}"),
        }
    }

    async fn multi_select(
        &self,
        _message: String,
        _choices: Vec<&'static str>,
        _default_indices: Vec<usize>,
    ) -> anyhow::Result<Vec<&'static str>> {
        match self.next() {
            ScriptedAnswer::MultiSelect(value) => Ok(value),
            other => panic!("expected a scripted MultiSelect answer, got {other:?}"),
        }
    }
}

#[test]
fn runtime_settings_use_the_canonical_firecrawl_env_var_names() {
    let (env, config) =
        build_runtime_settings("https://api.example/v1", "default", Transport::Stdio).unwrap();

    assert_eq!(env["OKF_MCP_FIRECRAWL_API_URL"], "https://api.example/v1");
    assert_eq!(env["OKF_MCP_API_VERSION"], "default");
    assert_eq!(env["OKF_MCP_TRANSPORT"], "stdio");
    assert_eq!(config["firecrawl_base_url"], "https://api.example/v1");
    assert_eq!(config["transport"], "stdio");
    // No auth_method or legacy `url` field — dropped entirely, since
    // AuthMethod has exactly one variant and `url` is unread legacy.
    assert!(config.get("auth_method").is_none());
    assert!(config.get("url").is_none());
}

#[test]
fn apply_reveals_includes_only_opted_in_secrets() {
    let secrets = vec![
        RevealableSecret {
            label: "a".to_string(),
            env_key: "SECRET_A".to_string(),
            value: "value-a".to_string(),
        },
        RevealableSecret {
            label: "b".to_string(),
            env_key: "SECRET_B".to_string(),
            value: "value-b".to_string(),
        },
    ];
    let mut revealed = HashSet::new();
    revealed.insert("SECRET_A".to_string());

    let mut env = HashMap::new();
    apply_reveals(&mut env, &secrets, &revealed);

    assert_eq!(env.get("SECRET_A"), Some(&"value-a".to_string()));
    assert_eq!(env.get("SECRET_B"), None);
}

#[tokio::test]
async fn noninteractive_output_paths_cover_both_transports() {
    let (env, config) =
        build_runtime_settings("https://api.example/v1", "default", Transport::Http).unwrap();
    persist_selection(
        "Print a ready-to-run CLI invocation",
        &env,
        &config,
        &HashSet::new(),
    )
    .await
    .unwrap();
    print_mcp_client_config(Transport::Stdio, AuthMethod::Pat, &env);
    print_mcp_client_config(Transport::Http, AuthMethod::Pat, &env);
}

#[tokio::test]
async fn file_persistence_writes_env_local_yaml_and_global_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let local_dir = dir.path().join("local");
    let global_config = dir.path().join("global/config.yml");
    std::fs::create_dir_all(&local_dir).unwrap();

    let (env, config) =
        build_runtime_settings("https://api.example/v1", "default", Transport::Stdio).unwrap();
    persist_selection_at(
        "Write a .env file",
        &env,
        &config,
        &HashSet::new(),
        &local_dir,
        &global_config,
    )
    .await
    .unwrap();
    persist_selection_at(
        "Write a config.yml file (local — ./okf-mcp.config.yml, read only from this directory)",
        &env,
        &config,
        &HashSet::new(),
        &local_dir,
        &global_config,
    )
    .await
    .unwrap();
    persist_selection_at(
        "Write a config.yml file (global — ~/.okf-mcp/config.yml, read on every invocation on this machine)",
        &env,
        &config,
        &HashSet::new(),
        &local_dir,
        &global_config,
    )
    .await
    .unwrap();

    assert!(local_dir.join(".env").is_file());
    assert!(local_dir.join("okf-mcp.config.yml").is_file());
    assert!(global_config.is_file());
}

#[tokio::test]
async fn env_persistence_scrubs_a_stale_secret_when_this_run_declines_to_reveal_it() {
    let dir = tempfile::tempdir().unwrap();
    let local_dir = dir.path().join("local");
    std::fs::create_dir_all(&local_dir).unwrap();
    let global_config = dir.path().join("global/config.yml");

    // A previous run revealed the secret into .env.
    std::fs::write(
        local_dir.join(".env"),
        "OKF_MCP_FIRECRAWL_API_PAT=old-secret-value\nOKF_MCP_TRANSPORT=stdio\n",
    )
    .unwrap();

    let (env, config) =
        build_runtime_settings("https://api.example/v1", "default", Transport::Stdio).unwrap();
    let mut secret_keys = HashSet::new();
    secret_keys.insert("OKF_MCP_FIRECRAWL_API_PAT".to_string());

    persist_selection_at(
        "Write a .env file",
        &env,
        &config,
        &secret_keys,
        &local_dir,
        &global_config,
    )
    .await
    .unwrap();

    let contents = std::fs::read_to_string(local_dir.join(".env")).unwrap();
    assert!(!contents.contains("old-secret-value"));
    assert!(!contents.contains("OKF_MCP_FIRECRAWL_API_PAT"));
    assert!(contents.contains("OKF_MCP_TRANSPORT=stdio"));
}

#[tokio::test]
async fn config_yaml_persistence_merges_with_an_existing_file_instead_of_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("okf-mcp.config.yml");
    std::fs::write(
        &path,
        "log_level: debug\ncustom_providers:\n  existing-provider:\n    base_url: https://existing.example/v1\n",
    )
    .unwrap();

    let mut env = HashMap::new();
    let mut config_fields = serde_json::json!({
        "firecrawl_base_url": "https://api.example/v1",
        "transport": "stdio",
    });
    let custom = vec![CustomProviderChoice {
        name: "new-provider".to_string(),
        base_url: "https://new.example/v1".to_string(),
        key: ProviderKeyResult {
            value: None,
            freshly_entered: false,
        },
    }];
    merge_custom_providers(&mut env, &mut config_fields, &custom);

    write_config_yaml(&path, &config_fields).await.unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("log_level: debug"));
    assert!(contents.contains("existing-provider"));
    assert!(contents.contains("new-provider"));
    assert!(contents.contains("transport: stdio"));
}

#[test]
fn validate_custom_provider_name_rejects_empty_slash_and_taken_names() {
    let taken = vec!["anthropic".to_string(), "myvllm".to_string()];
    assert_eq!(
        validate_custom_provider_name("", &taken),
        Err("name can't be empty")
    );
    assert_eq!(
        validate_custom_provider_name("   ", &taken),
        Err("name can't be empty")
    );
    assert_eq!(
        validate_custom_provider_name("foo/bar", &taken),
        Err("name can't contain '/'")
    );
    assert_eq!(
        validate_custom_provider_name("anthropic", &taken),
        Err("name already in use")
    );
    assert_eq!(
        validate_custom_provider_name("myvllm", &taken),
        Err("name already in use")
    );
    assert_eq!(
        validate_custom_provider_name("brand-new-name", &taken),
        Ok(())
    );
}

#[test]
fn provider_default_indices_selects_only_already_configured_names() {
    let names = ["anthropic", "openai", "ollama"];
    let indices = provider_default_indices(&names, |name| name == "openai");
    assert_eq!(indices, vec![1]);
}

#[test]
fn collect_revealable_secrets_covers_firecrawl_known_and_custom_keys() {
    let mut credentials = HashMap::new();
    credentials.insert("token".to_string(), "firecrawl-pat".to_string());

    let known = vec![KnownProviderChoice {
        name: "anthropic".to_string(),
        key: ProviderKeyResult {
            value: Some("anthropic-key".to_string()),
            freshly_entered: true,
        },
    }];
    let custom = vec![CustomProviderChoice {
        name: "myvllm".to_string(),
        base_url: "https://myvllm.example/v1".to_string(),
        key: ProviderKeyResult {
            value: Some("myvllm-key".to_string()),
            freshly_entered: true,
        },
    }];

    let secrets = collect_revealable_secrets(&credentials, &known, &custom).unwrap();
    assert_eq!(secrets.len(), 3);
    assert!(
        secrets
            .iter()
            .any(|s| s.env_key == "OKF_MCP_FIRECRAWL_API_PAT" && s.value == "firecrawl-pat")
    );
    assert!(
        secrets
            .iter()
            .any(|s| s.env_key == "ANTHROPIC_API_KEY" && s.value == "anthropic-key")
    );
    assert!(secrets.iter().any(
        |s| s.env_key == provider::synthetic_api_key_env_name("myvllm") && s.value == "myvllm-key"
    ));
}

#[test]
fn collect_revealable_secrets_omits_providers_with_no_resolved_key() {
    let credentials = HashMap::new();
    let known = vec![KnownProviderChoice {
        name: "ollama".to_string(),
        key: ProviderKeyResult {
            value: None,
            freshly_entered: false,
        },
    }];
    let secrets = collect_revealable_secrets(&credentials, &known, &[]).unwrap();
    assert!(secrets.is_empty());
}

#[tokio::test]
async fn prompt_base_url_returns_the_provided_url() {
    // Port 1 is reserved (nothing listens there), so this fails fast
    // via connection-refused rather than waiting out the 5s
    // reachability timeout — keeps the test quick while still
    // exercising the unreachable-URL branch.
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Text(
        "http://127.0.0.1:1/".to_string(),
    )]);
    let url = prompt_base_url(&prompts, "https://api.firecrawl.dev")
        .await
        .unwrap();
    assert_eq!(url, "http://127.0.0.1:1/");
}

#[tokio::test]
async fn prompt_transport_maps_the_selected_choice_to_a_transport() {
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Select(
        "http (a standalone server the MCP client connects to over the network)",
    )]);
    let transport = prompt_transport(&prompts, Transport::Stdio).await.unwrap();
    assert!(matches!(transport, Transport::Http));
}

#[tokio::test]
async fn prompt_secret_decision_prompts_for_a_new_value_when_nothing_is_saved() {
    let account = "zz-test-prompt-secret-decision-account-1";
    let _ = delete_credential(account);
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Password("new-value".to_string())]);
    let decision = prompt_secret_decision(&prompts, "test secret", account)
        .await
        .unwrap();
    assert!(matches!(decision, SecretDecision::New(v) if v == "new-value"));
    let _ = delete_credential(account);
}

#[tokio::test]
async fn prompt_secret_decision_offers_keep_when_a_value_is_already_saved() {
    let account = "zz-test-prompt-secret-decision-account-2";
    save_credential(account, "already-saved").unwrap();
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Confirm(true)]);
    let decision = prompt_secret_decision(&prompts, "test secret", account)
        .await
        .unwrap();
    assert!(matches!(decision, SecretDecision::KeepExisting));
    let _ = delete_credential(account);
}

#[tokio::test]
async fn prompt_provider_api_key_treats_a_blank_answer_as_not_needed() {
    let account = "zz-test-provider-api-key-account-1";
    let _ = delete_credential(account);
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Password("   ".to_string())]);
    let result = prompt_provider_api_key(&prompts, "test provider", account, "API key:")
        .await
        .unwrap();
    assert_eq!(result.value, None);
    assert!(!result.freshly_entered);
    let _ = delete_credential(account);
}

#[tokio::test]
async fn prompt_provider_api_key_records_a_freshly_entered_value() {
    let account = "zz-test-provider-api-key-account-2";
    let _ = delete_credential(account);
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Password("real-key".to_string())]);
    let result = prompt_provider_api_key(&prompts, "test provider", account, "API key:")
        .await
        .unwrap();
    assert_eq!(result.value.as_deref(), Some("real-key"));
    assert!(result.freshly_entered);
    let _ = delete_credential(account);
}

#[tokio::test]
async fn prompt_custom_provider_name_retries_after_an_invalid_answer() {
    let prompts = ScriptedPrompts::new(vec![
        ScriptedAnswer::Text("anthropic".to_string()), // taken (known provider) -> invalid
        ScriptedAnswer::Text("brand-new-name".to_string()), // valid
    ]);
    let name = prompt_custom_provider_name(&prompts, &[]).await.unwrap();
    assert_eq!(name, "brand-new-name");
}

#[tokio::test]
async fn prompt_llm_providers_returns_empty_when_declined() {
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Confirm(false)]);
    let (known, custom) = prompt_llm_providers(&prompts, &HashMap::new())
        .await
        .unwrap();
    assert!(known.is_empty());
    assert!(custom.is_empty());
}

#[tokio::test]
async fn prompt_llm_providers_configures_a_no_key_known_provider_and_one_custom_provider() {
    let custom_provider_name = "zz-test-custom-provider-for-prompt-llm-providers";
    let custom_account = format!("llm-{custom_provider_name}");
    let _ = delete_credential(&custom_account);

    let prompts = ScriptedPrompts::new(vec![
        ScriptedAnswer::Confirm(true),               // configure providers? yes
        ScriptedAnswer::MultiSelect(vec!["ollama"]), // select ollama (no key needed)
        ScriptedAnswer::Confirm(true),               // add a custom provider? yes
        ScriptedAnswer::Text(custom_provider_name.to_string()), // custom provider name
        ScriptedAnswer::Text("https://zz-test.example/v1".to_string()), // endpoint
        ScriptedAnswer::Password("custom-key".to_string()), // api key
        ScriptedAnswer::Confirm(false),              // add another? no
    ]);

    let (known, custom) = prompt_llm_providers(&prompts, &HashMap::new())
        .await
        .unwrap();

    assert_eq!(known.len(), 1);
    assert_eq!(known[0].name, "ollama");
    assert_eq!(known[0].key.value, None);

    assert_eq!(custom.len(), 1);
    assert_eq!(custom[0].name, custom_provider_name);
    assert_eq!(custom[0].base_url, "https://zz-test.example/v1");
    assert_eq!(custom[0].key.value.as_deref(), Some("custom-key"));

    let _ = delete_credential(&custom_account);
}

#[tokio::test]
async fn prompt_llm_providers_reuses_a_saved_custom_provider_endpoint_silently() {
    let mut existing = HashMap::new();
    existing.insert(
        "zz-test-existing-custom".to_string(),
        CustomProviderEntry {
            base_url: "https://already-saved.example/v1".to_string(),
        },
    );
    let account = "llm-zz-test-existing-custom";
    let _ = delete_credential(account);

    // No Text answer for the endpoint — if the code asked for one
    // despite finding a saved entry, this test would panic on an
    // exhausted queue.
    let prompts = ScriptedPrompts::new(vec![
        ScriptedAnswer::Confirm(true),
        ScriptedAnswer::MultiSelect(vec![]),
        ScriptedAnswer::Confirm(true),
        ScriptedAnswer::Text("zz-test-existing-custom".to_string()),
        ScriptedAnswer::Password("".to_string()),
        ScriptedAnswer::Confirm(false),
    ]);

    let (_, custom) = prompt_llm_providers(&prompts, &existing).await.unwrap();
    assert_eq!(custom.len(), 1);
    assert_eq!(custom[0].base_url, "https://already-saved.example/v1");
    assert_eq!(custom[0].key.value, None);

    let _ = delete_credential(account);
}

#[tokio::test]
async fn prompt_secret_reveal_returns_the_scripted_confirm_value() {
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Confirm(true)]);
    assert!(prompt_secret_reveal(&prompts, "a secret").await.unwrap());
}

#[tokio::test]
async fn prompt_persistence_delegates_to_the_selected_choice() {
    let prompts = ScriptedPrompts::new(vec![ScriptedAnswer::Select(
        "Print a ready-to-run CLI invocation (nothing written to disk)",
    )]);
    let (env, config) =
        build_runtime_settings("https://api.example/v1", "default", Transport::Stdio).unwrap();
    prompt_persistence(&prompts, &env, &config, &HashSet::new())
        .await
        .unwrap();
}
