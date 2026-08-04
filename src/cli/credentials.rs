// `okf-mcp credentials list|clear` — the symmetric "undo" for `okf-mcp
// setup`: inspect and remove saved API keys (the Firecrawl PAT, and every
// LLM provider key, known or custom) without digging through the OS
// keychain by hand.
//
// `list` is read-only and never prints a credential's real value — only
// the account name and whether one is saved, via `credential_exists`
// (never `load_credential`), matching this codebase's existing
// secret-reveal-gating care (see `cli::setup_wizard`'s `prompt_secret_reveal`).
//
// `clear` defaults to requiring confirmation, mirroring
// `cli::compile::confirm_commit`'s `spawn_blocking` + `inquire::Confirm`
// pattern (including its non-interactive short-circuit): pass `--yes` to
// skip it. It prints exactly which accounts it's about to clear before
// prompting, then calls the already-tested, already-idempotent
// `delete_credential` for each one — "delete a non-existent entry" is a
// no-op success per that function's own doc, so no extra
// existence-checking machinery is added here.

use std::io::IsTerminal;

use okf_mcp::core::config_manager;
use okf_mcp::core::credential_storage::{
    all_known_credential_accounts, credential_exists, delete_credential,
};
use okf_mcp::core::output::Output;

/// `list`'s human-readable status lines for `accounts`, given an
/// injectable existence check — mirrors `cli::setup_wizard::
/// provider_default_indices`'s closure-injection pattern so this is
/// testable without touching the real OS keychain. Takes (and returns)
/// only whether a credential exists, never its value.
fn credential_status_lines(
    accounts: &[String],
    mut exists: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            let saved = exists(account)?;
            Ok(format!(
                "{account}: {}",
                if saved { "saved" } else { "not saved" }
            ))
        })
        .collect()
}

pub fn run_list() -> anyhow::Result<()> {
    let config = config_manager::load_config(serde_json::Map::new())?;
    let accounts = all_known_credential_accounts(&config);
    let lines = credential_status_lines(&accounts, credential_exists)?;

    let output = Output::cli();
    for line in &lines {
        output.line(line);
    }
    Ok(())
}

/// This codebase's own `"llm-<name>"` account-naming convention for a
/// provider key, with `"firecrawl"` as the one exception — it's saved
/// under its own bare account name, not `"llm-firecrawl"` (see
/// `auth::auth_manager::CREDENTIAL_ACCOUNT`).
fn account_for_provider(provider: &str) -> String {
    if provider == "firecrawl" {
        "firecrawl".to_string()
    } else {
        format!("llm-{provider}")
    }
}

/// Which accounts `clear` should target, given `--provider`/`--all` —
/// pure over an already-enumerated `known_accounts`, so testable without a
/// confirmation prompt or keychain access. `--provider <name>` is accepted
/// even for a name absent from `known_accounts` (e.g. a custom provider
/// since removed from config, whose credential might still linger in the
/// keychain) — `delete_credential` is already a safe no-op on an account
/// with nothing saved, so no extra validation is added here.
fn accounts_to_clear(
    known_accounts: &[String],
    provider: Option<&str>,
    all: bool,
) -> anyhow::Result<Vec<String>> {
    match (provider, all) {
        (Some(_), true) => anyhow::bail!("--provider and --all cannot both be given"),
        (None, false) => anyhow::bail!("specify either --provider <name> or --all"),
        (Some(name), false) => Ok(vec![account_for_provider(name)]),
        (None, true) => Ok(known_accounts.to_vec()),
    }
}

/// The pre-confirmation summary `clear` prints and prompts with — the
/// indented account lines to print, and the exact `inquire::Confirm`
/// message — pure so it's testable without a real prompt.
fn build_clear_summary(accounts: &[String]) -> (Vec<String>, String) {
    let lines = accounts.iter().map(|a| format!("  {a}")).collect();
    let message = format!(
        "Clear {} credential(s)? This cannot be undone.",
        accounts.len()
    );
    (lines, message)
}

/// Mirrors `cli::compile::confirm_commit`'s `spawn_blocking` + `inquire`
/// pattern for running a blocking prompt from an async context, including
/// its non-interactive short-circuit: a session with no TTY on stdin (CI,
/// a pipe, a background job) short-circuits to "don't clear" rather than
/// hanging on input that can't come.
async fn confirm_clear(message: String, assume_yes: bool) -> anyhow::Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let confirmed = tokio::task::spawn_blocking(move || {
        inquire::Confirm::new(&message).with_default(false).prompt()
    })
    .await??;
    Ok(confirmed)
}

pub async fn run_clear(provider: Option<&str>, all: bool, yes: bool) -> anyhow::Result<()> {
    let config = config_manager::load_config(serde_json::Map::new())?;
    let known_accounts = all_known_credential_accounts(&config);
    let accounts = accounts_to_clear(&known_accounts, provider, all)?;

    let output = Output::cli();
    let (lines, message) = build_clear_summary(&accounts);
    output.line("About to clear:");
    for line in &lines {
        output.line(line);
    }

    if !confirm_clear(message, yes).await? {
        output.line("Aborted — nothing was cleared.");
        return Ok(());
    }

    for account in &accounts {
        delete_credential(account)?;
    }
    output.line(&format!("Cleared {} credential(s).", accounts.len()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_status_lines_reports_saved_and_not_saved() {
        let accounts = vec!["firecrawl".to_string(), "llm-anthropic".to_string()];
        let lines =
            credential_status_lines(&accounts, |account| Ok(account == "firecrawl")).unwrap();
        assert_eq!(
            lines,
            vec![
                "firecrawl: saved".to_string(),
                "llm-anthropic: not saved".to_string(),
            ]
        );
    }

    #[test]
    fn credential_status_lines_propagates_an_existence_check_error() {
        let accounts = vec!["firecrawl".to_string()];
        let result = credential_status_lines(&accounts, |_| anyhow::bail!("keychain unavailable"));
        assert!(result.is_err());
    }

    #[test]
    fn account_for_provider_special_cases_firecrawl() {
        assert_eq!(account_for_provider("firecrawl"), "firecrawl");
        assert_eq!(account_for_provider("anthropic"), "llm-anthropic");
    }

    #[test]
    fn accounts_to_clear_targets_one_account_for_provider() {
        let known = vec!["firecrawl".to_string(), "llm-anthropic".to_string()];
        let accounts = accounts_to_clear(&known, Some("anthropic"), false).unwrap();
        assert_eq!(accounts, vec!["llm-anthropic".to_string()]);
    }

    #[test]
    fn accounts_to_clear_special_cases_firecrawl_as_a_provider_name() {
        let known = vec!["firecrawl".to_string()];
        let accounts = accounts_to_clear(&known, Some("firecrawl"), false).unwrap();
        assert_eq!(accounts, vec!["firecrawl".to_string()]);
    }

    #[test]
    fn accounts_to_clear_accepts_a_provider_not_in_the_known_set() {
        let known = vec!["firecrawl".to_string()];
        let accounts = accounts_to_clear(&known, Some("zz-removed-custom"), false).unwrap();
        assert_eq!(accounts, vec!["llm-zz-removed-custom".to_string()]);
    }

    #[test]
    fn accounts_to_clear_targets_everything_known_for_all() {
        let known = vec!["firecrawl".to_string(), "llm-anthropic".to_string()];
        let accounts = accounts_to_clear(&known, None, true).unwrap();
        assert_eq!(accounts, known);
    }

    #[test]
    fn accounts_to_clear_rejects_both_provider_and_all() {
        let known = vec!["firecrawl".to_string()];
        assert!(accounts_to_clear(&known, Some("anthropic"), true).is_err());
    }

    #[test]
    fn accounts_to_clear_rejects_neither_provider_nor_all() {
        let known = vec!["firecrawl".to_string()];
        assert!(accounts_to_clear(&known, None, false).is_err());
    }

    #[test]
    fn build_clear_summary_lists_each_account_and_counts_them_in_the_message() {
        let accounts = vec!["firecrawl".to_string(), "llm-anthropic".to_string()];
        let (lines, message) = build_clear_summary(&accounts);
        assert_eq!(
            lines,
            vec!["  firecrawl".to_string(), "  llm-anthropic".to_string()]
        );
        assert_eq!(message, "Clear 2 credential(s)? This cannot be undone.");
    }

    #[tokio::test]
    async fn confirm_clear_short_circuits_to_true_when_yes_is_passed() {
        assert!(confirm_clear("clear?".to_string(), true).await.unwrap());
    }

    #[tokio::test]
    async fn confirm_clear_short_circuits_to_false_when_not_interactive() {
        // `cargo test`'s stdin is never a TTY, so this exercises the
        // non-interactive short-circuit without hanging on a real prompt.
        assert!(!confirm_clear("clear?".to_string(), false).await.unwrap());
    }
}
