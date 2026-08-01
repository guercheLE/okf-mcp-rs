# okf-mcp: fix 13 reported issues from real-world usage, then ship

## Context

The user ran `okf-mcp` end-to-end against a real repo (ingest → compile → lint) and hit a batch of correctness bugs and UX gaps, reported as 13 numbered items plus follow-up discussion. Two research passes traced every item to exact code; this plan fixes all of them, then releases. Two items turned out to be the *same underlying bug wearing two symptoms*, and one turned out not to be an okf-mcp bug at all:

- The Groq 401 (**item 8**) and the earlier-fixed Ollama URL bug (**item 4**) are *not* the same bug — the trailing-slash/scheme normalization already added in commit `00c8279` is generic across all providers already. The 401 is a distinct bug: `seed_env_from_credential_storage` treats a blank/whitespace env var as "already set" and silently skips the correctly-saved keychain credential, with no trimming anywhere in the key path either.
- The excessive lint errors (**item 9**) trace back to the user having force-killed a hung `compile` run (no progress output → looked frozen). A killed/partial compile leaves wiki pages on disk that reference sources or concepts other operations never got to create, and today `okf.json` + the git commit both happen *before* compile's pass/fail check — so broken state gets committed. Fixing progress reporting (**item 5**), commit-gating, and adding a "pending compile" signal together close this loop.
- `product-brief.md` (**item 10a**) is not okf-mcp's doing — no scaffold/create-on-broken-link code exists anywhere in this codebase. That's Obsidian's own client behavior when you click an unresolved `[[link]]`. No code change; call this out explicitly to the user.

**Also discovered during investigation, unrelated to the 13 items but blocking:** the "CI" GitHub Actions workflow has been failing on every push since at least the `v0.2.0`/`v0.2.1` release commits (confirmed via `gh run list` — both show `CI ... failure` while `Release artifacts`/`Docker Build`/`Publish container image` all pass). The actual cause, confirmed by reading the failing run's log: `cargo fmt --check` fails on unformatted code in `tests/cli_smoke.rs`, `tests/manifest_cas.rs`, and `examples/profile_search.rs` (long multi-arg calls not wrapped per rustfmt rules) — this is the *first* step in `ci.yml`, before clippy/tests/coverage ever run, so the job aborts in ~1-1.5 minutes without ever validating anything else. Since this is pre-existing and every commit in this batch would inherit the same red CI otherwise, **fixing it is the very first task, before item 0.**

User decisions made for this plan:
- `vault` gets a full `kb` alias (not a rename) — same commands, both names documented as equivalent, one for Obsidian-oriented users, one for OKF-oriented workflows.
- A `compile`/`rebuild` run with any failed source or lint errors **does not commit or write `okf.json` at all** — leaves the working tree dirty for inspection/re-run, rather than committing a partial/clean subset.
- **Transport-aware output routing applies to all user-facing logging/output, not just long-running-command progress**: CLI direct invocation and MCP-over-HTTP write to stdout; MCP-over-stdio writes to stderr only (stdout there is the JSON-RPC channel). Existing `tracing`-based diagnostic logging (`src/core/logger.rs`) already unconditionally uses stderr and needs no change — that's a stricter, already-correct policy for log *noise*, orthogonal to this rule for structured/result *output*.
- Everything in one pass, each fix as its own commit (Conventional Commits format), each with its own tests, coverage staying at or above the existing CI-enforced 70% production-coverage gate (`scripts/coverage.sh` / `scripts/check_production_coverage.py --minimum 70` — already exists, not a new requirement).
- Once everything is committed and green: bump the version (semver), tag, push, and monitor the resulting Actions runs, fixing anything that fails.

There's already an uncommitted, in-progress `vault remove`/`rm` (registry-only unregister) on disk in `src/cli/vault.rs`/`src/main.rs`/`tests/cli_smoke.rs` — build on it, don't redo it.

**Process note:** before starting item 0, copy this plan into the repo at `docs/plan-2026-08-fixes.md` and commit it on its own (e.g. `docs: add plan for reported-issues batch`) — so the plan lives in the repo's history before any code changes land. At the end of the batch (after item 15), re-commit that file if anything in it picked up fixes/comments/corrections during implementation (e.g. a design that changed once actually coded).

## Implementation order (real code dependencies)

```
0.  fix CI (rustfmt) + pre-commit hook — unblocks every subsequent commit's CI signal and prevents recurrence; do this first
1.  vault remove          — finish committing the already-in-progress work
2.  okf_version fix        — trivial, isolated
3.  credential shadowing   — isolated, provider.rs
4.  embedding cache dir    — isolated, embedding_service.rs
5.  setup wizard messaging — isolated, setup_wizard.rs
6.  vault create/delete + kb alias — independent feature
7.  manifest compiled_hash — foundation for 8 (both touch compiler::driver::compile's loop)
8.  skip commit on failure — built on 7's per-source manifest write
9.  pending-compile lint section — built on 7's compiled_hash field
10. output routing (progress + all cli/*.rs logging, transport-aware) — sequence right after 7/8/9
11. vault-level provider config — touches CompileOptions/provider.rs/CLI call sites
12. model-not-found hint  — touches execute_compile_prompt right after 11
13. test-connection multi-provider — reuses provider.rs surface from 11/12
14. config command redesign — reuses config_manager + provider.rs surface from 13
15. release: version bump, tag, push, monitor/fix Actions
```

(Numbering here is implementation sequence, not the user's original item numbers — mapped inline below.)

---

## 0. Fix CI: rustfmt drift blocking every build, and add a pre-commit hook to prevent recurrence

**Bug:** `cargo fmt --check` (the first step in `.github/workflows/ci.yml`) currently fails against `main` — confirmed locally with `cargo fmt --check`, which reports diffs in `tests/cli_smoke.rs`, `tests/manifest_cas.rs`, and `examples/profile_search.rs` (unwrapped multi-argument calls). Every later step in the job (clippy, `cargo test`, the profiling smoke test, `scripts/coverage.sh`) never runs as a result, so this is currently the *only* thing CI actually reports.

**Fix (commit 1):** run `cargo fmt` across the repo, review the diff (formatting-only, no logic change expected), commit as its own conventional commit (e.g. `style: fix rustfmt formatting drift blocking CI`).

**Fix (commit 2 — prevent recurrence, per user decision):** add a local **pre-commit** git hook that runs `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` (the same two fast gates CI checks first) and blocks the commit if either fails, so this class of bug can't land locally again in the first place.
- No existing hook infrastructure in this repo (confirmed — no `.githooks/`, no `.pre-commit-config.yaml`, no `cargo-husky` dependency, `core.hooksPath` unset). Since `.git/hooks/` isn't version-controlled, check a script into the repo instead: `.githooks/pre-commit` (shell script running `cargo fmt --check` then `cargo clippy --all-targets -- -D warnings`, exiting non-zero — and printing which check failed — if either does).
- Wire it up: `git config core.hooksPath .githooks` — run this once for the current checkout as part of landing this item, and document the one-time setup step (e.g. in `CONTRIBUTING.md` or the README) so other/future clones activate it too; it isn't automatic on `git clone`.
- Commit the hook script and any doc addition as its own commit (e.g. `chore: add pre-commit hook enforcing fmt+clippy`).

**Verify:** `cargo fmt --check` passes locally; push (or open the PR) and confirm the CI workflow gets past the format step — ideally confirm the whole job goes green here, before layering the rest of the batch on top, so every subsequent commit's CI status is meaningful rather than inheriting a pre-existing red X. For the hook: make a throwaway unformatted change locally and confirm `git commit` is blocked with a clear message; fix it and confirm the commit then succeeds.

---

## 1. Finish `vault remove` (commit the in-progress work)

Files already modified, uncommitted: `src/cli/vault.rs`, `src/main.rs`, `tests/cli_smoke.rs`. Review the diff, run `cargo test` (covers the new `vault_add_list_and_default_round_trip` remove/`rm` coverage already in `tests/cli_smoke.rs`), and confirm the full suite passes — **not just this file's tests** — before committing. Only commit once green.

---

## 2. Fix `okf_version` inconsistency (user item 3)

**Bug:** `src/ingest/frontmatter.rs:11` hardcodes `const OKF_VERSION: &str = "1.0"` for every `raw/*.md` blob, while every other schema location (`compiler/prompts.rs:37`, `storage/bundle.rs:16`, all wiki fixtures) uses `"0.2"`. The design doc's actual reference implementation (`docs/okf-pipeline-design.md:222`) confirms `"0.2"` is correct — the `"1.0"` in `frontmatter.rs` was a leftover from an earlier illustrative doc example.

**Fix:**
- Add `src/core/okf_schema.rs`: `pub const OKF_SCHEMA_VERSION: &str = "0.2";` (single source of truth), registered via `pub mod okf_schema;` in `src/core/mod.rs`.
- `src/ingest/frontmatter.rs:11,63`: delete the local constant, use `crate::core::okf_schema::OKF_SCHEMA_VERSION`.
- `src/storage/bundle.rs:16,81`: same — delete `BUNDLE_OKF_VERSION`, use the shared constant.
- Leave `compiler/prompts.rs`'s `"0.2"` as a literal — it's LLM-facing prompt text, not a Rust value that can share the constant.

**Tests:** update the existing assertion in `frontmatter.rs`'s test module (currently asserts `"1.0"`) to `"0.2"`. Existing `bundle.rs` tests already hardcode `"0.2"` fixtures and should keep passing unchanged.

---

## 3. Fix credential shadowing + missing whitespace trim (user item 8, root cause of the Groq 401)

**Bug:** `src/compiler/provider.rs`, `seed_env_from_credential_storage`:
```rust
fn seed_env_from_credential_storage(env_name: &str, provider: &str) {
    if std::env::var(env_name).is_ok() {  // true even for an EMPTY string
        return;
    }
    ...
}
```
`std::env::var` returns `Ok("")` for a var that's set-but-blank. Any stray/blank `GROQ_API_KEY` (leftover shell export, inherited from an MCP host's spawn env, etc.) silently skips the correctly-saved keychain key — confirmed via `setup_wizard.rs:339-346`, the key really is saved to the OS keychain/encrypted fallback (`save_credential`), not any YAML file, so the credential-storage side is fine; the skip logic is the bug. Also confirmed: no code anywhere trims whitespace from the key, at save (`setup_wizard.rs`'s password prompt) or load time — a pasted trailing newline would 401 the same way. This is a shared code path for every key-bearing provider (anthropic/openai/groq/custom), not Groq-specific.

**Fix in `provider.rs`:**
```rust
fn env_var_is_meaningfully_set(name: &str) -> bool {
    std::env::var(name).map(|v| !v.trim().is_empty()).unwrap_or(false)
}
```
Use it in `seed_env_from_credential_storage`'s early-return check; trim the seeded value too (`std::env::set_var(env_name, key.trim())`) so already-saved-untrimmed keys self-heal without re-running setup.

**Fix in `setup_wizard.rs`:** trim before saving — `prompt_llm_provider`'s return becomes `Ok(Some((provider, Some(key.trim().to_string()))))`.

**Tests:** new unit test `env_var_is_meaningfully_set_treats_blank_and_whitespace_only_as_unset` (empty string → false, whitespace-only → false, real value → true, unset → false). Existing `seed_env_from_credential_storage_does_not_overwrite_an_already_set_var` test keeps passing unchanged.

**Note for the user:** the identical blank-and-unwhitespace-trimmed exposure exists for the Firecrawl PAT (`prompt_credentials`) — same bug class, different code path, not touched by this fix (flagging, not fixing, since it's a separate provider entirely).

---

## 4. Fix ONNX/embedding model re-downloading every run (user item 7)

**Bug:** `src/services/embedding_service.rs` builds `TextEmbedding` via `TextInitOptions::new(EmbeddingModel::AllMpnetBaseV2)` with no cache dir set. `fastembed` then defaults to the **relative** path `./.fastembed_cache` (relative to CWD at call time). Since okf-mcp never pins this and CWD varies by invocation (especially as an MCP subprocess spawned by a host app from an arbitrary directory), the ~415 MiB model re-downloads whenever CWD differs from a prior run.

**Fix:** resolve the cache dir with an explicit 3-tier precedence, all absolute/CWD-independent, all in a single dedicated function (deliberately *not* delegating to `credential_storage::resolve_home_dir()`, since that helper's own last-resort fallback is `"."` — relative to CWD — which would silently reintroduce the exact bug being fixed; this needs to *detect* "no home dir available" to fall through to tier 3, not just return a relative path):

```rust
fn resolve_models_cache_dir() -> std::path::PathBuf {
    // 1. Respect an explicit FASTEMBED_CACHE_DIR if the user/environment already sets one.
    if let Ok(dir) = std::env::var("FASTEMBED_CACHE_DIR") {
        if !dir.trim().is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    // 2. User's home directory.
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return std::path::PathBuf::from(home).join(".okf-mcp/models");
    }
    // 3. No env var, no resolvable home — fall back to the directory containing
    // the okf-mcp executable itself (mirrors config_manager's existing
    // `install_dir` = `std::env::current_exe()?.parent()` pattern). Still
    // absolute and invocation-CWD-independent.
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("okf-mcp-models")))
        .unwrap_or_else(|| std::path::PathBuf::from(".fastembed_cache")) // last-ditch: only if current_exe() itself fails
}
```
Call `.with_cache_dir(resolve_models_cache_dir())` on `TextInitOptions` (confirmed present in `fastembed` 5.17.4's API) when building the model in `embedding_service.rs`.

**Tests:** unit tests covering all three tiers by manipulating env vars in-process (same pattern as `credential_storage.rs`'s existing home-dir tests): `FASTEMBED_CACHE_DIR` set → used verbatim; unset + `HOME` set to a tempdir → `<tempdir>/.okf-mcp/models`; both unset → falls through to the executable's directory (assert the result is absolute and does *not* equal a relative `.fastembed_cache`). No network/model download involved in any case. Manual verification: run `okf-mcp reindex --embeddings` from two different working directories and confirm the second run doesn't redownload.

---

## 5. Fix setup wizard messaging (surfaced during this investigation)

**Bug:** `prompt_persistence`'s "How should these settings be saved?" question only affects Firecrawl fields (`url`, `auth_method`, etc.) — the LLM provider key is saved to credential storage unconditionally, *before* this question is even asked. A user can reasonably (and did) believe picking "global yaml" also persisted their LLM key there.

**Fix:** in `run_setup_wizard`, right after the existing `println!("Saved {provider} API key.")`, add a line making the independence explicit: `"Saved to your OS keychain (or the encrypted-file fallback) — independent of the persistence choice below, which only applies to Firecrawl settings."`

**Tests:** none automatable (fully `inquire`-interactive); verify manually by running `okf-mcp setup`.

---

## 6. Vault lifecycle: `create`, `delete`, `kb` alias (user items 1 & 2)

**Add `vault create <path> --name <name> [--description]`:** scaffolds a brand-new vault (`.okf/` via `manifest::store::save`'s existing dir-creation, plus new `wiki/concepts/` and `raw/` dirs), then registers it by calling the existing `add` logic. Errors clearly if `path` already exists and is non-empty, pointing at `vault add` for adopting an existing directory instead.

**Add `vault delete <name> [--force]`:** destructive — requires `--force` (bails with a clear message otherwise), prints the exact path before deleting, deletes the on-disk directory tree **first**, then unregisters (reuses `remove`). Deleting before unregistering means a failed `remove_dir_all` (permissions, locked file) leaves the vault still registered and retryable, rather than dangling with no backing directory.

**`kb` alias:** add `#[command(alias = "kb")]` on the `Command::Vault(VaultCommand)` variant in `main.rs` (not per-subcommand) — `okf-mcp kb list/add/create/remove/delete/default` all work identically to `okf-mcp vault ...`. Make the equivalence explicit in help text: the doc comment on that variant reads something like `"Manage vaults / knowledge bases (alias: kb) — 'vault' and 'kb' are the same commands under two names: 'vault' for Obsidian-oriented workflows, 'kb' for OKF-oriented ones."` so `--help` output states outright that they're identical, not just that one is an alias of the other.

**Tests:** new `tests/cli_smoke.rs` cases — `vault create` then assert `.okf`/`wiki/concepts`/`raw` exist and it's registered; re-running `create` on the same (now non-empty) path fails with the `vault add` pointer; `vault delete` without `--force` fails; `vault delete --force` removes both the directory and the registry entry. A separate test asserting `okf-mcp kb list` and `okf-mcp vault list` produce identical output.

---

## 7. Manifest `compiled_hash` — resumability foundation (user item 10c, follow-up discussion)

**Design (confirmed via code, refined through discussion, not the original "new manifest field" idea I started with — it turned out simpler once the existing `select_sources` diff-only mechanism was traced):** `compile`/`rebuild` (no `--force`) already skip sources whose raw_id is referenced by an existing wiki page (`select_sources`'s `referenced_raw_ids` scan, `driver.rs`) — real resumability already exists today, just implicitly, by scanning wiki files on every run. The gap: that check only proves "*a* page mentions this source," not "this source's *entire* intended operation batch finished" — if one source's compile produces 3 pages and operation 2 of 3 fails mid-`apply_operations`, the source looks "referenced" (and gets permanently skipped on future diff-only runs) even though it's actually half-done.

**Fix:** add an explicit, authoritative completion marker to the existing manifest ledger, extending its established `ACTIVE`/`SUPERSEDED`/`TOMBSTONED` status pattern rather than inventing a second file or putting mutable state into the derived, snapshot-only `okf.json`:

`src/manifest/model.rs`, `SourceEntry`:
```rust
#[serde(default)]
pub compiled_hash: Option<String>,
```
Two new `Manifest` methods: `mark_compiled(&mut self, uri: &str)` (sets `compiled_hash = active_hash.clone()`) and `is_compiled_at_current_hash(&self, uri: &str) -> bool` (`compiled_hash.is_some() && compiled_hash == active_hash`). Re-ingesting a URI changes `active_hash`, which automatically invalidates a stale `compiled_hash` — no separate "clear" step needed, and this doubles as "recompile when source content changes" for free.

`driver.rs`'s `select_sources`: OR the new authoritative check with the existing wiki-scan (keep the scan as a safety net for vaults compiled before this ships, where `compiled_hash` is still `None` even though the wiki already reflects them).

`driver.rs`'s `compile` loop: change `manifest` to `mut`; on each source's success, call `manifest.mark_compiled(&uri)` and **save immediately, per source** (not batched at the end) — this is what actually delivers resumability against a crash/kill mid-run. Confirmed this write is independent of item 8's commit-gating below: `manifest.json` is never part of the git-staged path list, so it must keep persisting per-source regardless of whether the overall run later gets treated as failed.

No new CLI flag needed for "recompile everything" — `rebuild --force` already sets `diff_only = false`, bypassing `select_sources`'s filter entirely.

**Tests:** `manifest/model.rs` — `mark_compiled_sets_compiled_hash_to_the_current_active_hash`, `is_compiled_at_current_hash_is_false_until_marked`, `re_ingesting_after_a_compile_invalidates_the_compiled_flag`. `driver.rs` — `select_sources_diff_only_also_skips_sources_already_compiled_at_the_current_hash` (mark compiled but write **no** wiki page, so only the new check — not the old scan — catches it). Manual: run `compile` twice in a row against an unchanged vault, confirm the second run reports 0 sources to compile and makes no LLM call.

---

## 8. Skip commit/`okf.json` entirely on any failure (user item 9 root cause, decided behavior)

**Bug:** `src/cli/compile.rs`'s `report_and_commit` currently calls `bundle::write_bundle` and `storage::git::commit` **unconditionally**, and only checks `sources_failed()`/`lint_report.has_errors()` afterward to set the exit code — so a partial/broken compile's inconsistent wiki state gets committed before the failure is even reported.

**Fix:** reorder — check `sources_failed() > 0 || lint_report.has_errors()` **before** `write_bundle`/`git::commit`; if either is true, print a clear message ("N source(s) failed / lint found errors — not committing; fix and re-run compile.") and return an error without touching `okf.json` or git at all. `rebuild.rs` and `run.rs` both call this same `report_and_commit`, so this one change fixes all three commands.

**Confirmed edge case (per earlier discussion):** `./raw/` blobs and per-source manifest ingest-history entries are written entirely during `ingest`, before `compile` ever runs, and are structurally untouched by this gate — only `wiki/`'s new/updated pages for the *failed* run, `okf.json`, and the git commit are held back. (Successfully-compiled sources from *within* that same failed run still get their `wiki/concepts/*.md` pages written to disk per source, per item 7's per-source-apply design — they just don't get bundled/committed until a subsequent clean run.)

**Tests:** new `#[cfg(test)]` module in `cli/compile.rs` — construct a `CompileReport` directly (no LLM call needed) with one failing source, call `report_and_commit` against a tempdir vault, assert it returns `Err` and `okf.json` was **not** written. A second test with an all-clean report against a real `git init`'d tempdir asserts `okf.json` **is** written and a commit **is** created (happy path unchanged). Manual: force a failure (e.g. bogus model name), confirm `git status` shows no new commit.

---

## 9. "Pending compile" section in `lint` (follow-up discussion, closes the loop on user item 9)

**Design:** `lint_bundle` (`src/validator/rules.rs`) currently only scans `wiki/concepts/*.md` — no manifest access, so a vault with unfinished/never-compiled sources just produces a confusing wall of broken-link/missing-source symptoms with no indication *why*. With item 7's `compiled_hash` in place, `lint_bundle` can load the manifest and directly ask "which active sources are ingested but not (yet, or successfully) compiled at their current content" — covering both "never ran compile" and "compile was interrupted" in one check.

**Fix:** `lint_bundle` loads the manifest (`manifest::store::load`), adds a `pending_compiles: Vec<String>` field to `LintReport` (the list of URIs where `!manifest.is_compiled_at_current_hash(uri)`). `report.rs`'s `to_text` prints this as a **new first section**, before broken links: `"Pending compile (N source(s) not yet fully compiled — run 'okf-mcp compile'):"`. Treat it as a warning tier like orphans (not counted in `has_errors()`) — "not compiled yet" is normal mid-workflow state, not a defect; the point is giving the user the likely root cause up front instead of just symptoms.

**Tests:** `rules.rs` — a manifest with one active, uncompiled source produces exactly that URI in `pending_compiles`; marking it compiled clears it. `report.rs` — `to_text` renders the new section first and doesn't affect the overall OK/FAILED verdict.

---

## 10. Transport-aware output routing for ALL logging (user item 5, broadened twice over discussion)

**Bug:** `compiler::compile`'s loop (`driver.rs`) prints nothing per-source — confirmed no progress-bar crate (`indicatif` etc.) is even a dependency; the terminal is silent for the entire run, then one summary line at the end. The same silence applies to `search::reindex` (loops over every document, recomputing embeddings for changed ones) and, to a lesser extent, `ingest` (a single, potentially slow fetch/parse with no "I'm working" indication). Per the user, this isn't just a compile-specific gap: **the same transport-aware discipline should govern all user-facing output across the CLI, not just progress lines for long-running commands.**

**Output routing rule (applies everywhere, not just progress):**
- **CLI direct invocation** (any `okf-mcp <command>` run directly, no MCP client involved) → **stdout**.
- **MCP over HTTP transport** (`okf-mcp http`) → **stdout** — HTTP's protocol framing lives on the network socket, not stdio, so stdout is free to use.
- **MCP over stdio transport** (`okf-mcp start`) → **stderr** — stdout there *is* the JSON-RPC channel to the client and must never carry anything else.
- `tracing`-based diagnostic logs (`src/core/logger.rs`) are **left unchanged** — already unconditionally routed to stderr (confirmed, with an explicit existing comment about stdio safety), which is a *stricter* and already-correct policy for log noise. That's a separate concern from structured/result output (e.g. `okf-mcp config`'s JSON, meant to be piped/parsed) and shouldn't be merged with it.

**Design:** one shared, dependency-free output primitive in a new `src/core/output.rs`, used everywhere a command currently calls `println!`/`eprintln!` for user-facing results — not a one-off just for progress:
```rust
pub enum OutputStream { Stdout, Stderr }
pub struct Output(OutputStream);
impl Output {
    pub fn cli() -> Self { Output(OutputStream::Stdout) }
    pub fn for_transport(transport: Transport) -> Self {
        Output(if transport == Transport::Http { OutputStream::Stdout } else { OutputStream::Stderr })
    }
    pub fn line(&self, msg: &str) {
        match self.0 { OutputStream::Stdout => println!("{msg}"), OutputStream::Stderr => eprintln!("{msg}") }
    }
}
pub enum ProgressEvent {
    Started { index: usize, total: usize, label: String },
    Finished { index: usize, total: usize, label: String, error: Option<String> },
}
```
`compiler::compile`/`rebuild` and `search::reindex` each gain `on_progress: Option<&Output>`, firing `Started`/`Finished` (formatted via `ProgressEvent`'s `Display`) around each source/document in their existing loops (the same loop item 7 already touches for `compile`). `ingest::process_ingest`'s single-fetch CLI wrapper (`cli/ingest.rs`) doesn't need the full event enum — just `output.line(&format!("Fetching '{source}'..."))` before the call.

Beyond progress: as a consistency pass, convert the **existing** result-printing call sites across `cli/*.rs` (`vault.rs`'s list/add/create/remove/delete/default confirmations, `config.rs`, `compile.rs`'s summary lines, `lint`'s report printing, `test_connection.rs`, `reindex.rs`) to go through `Output::cli()` instead of raw `println!`/`eprintln!` — this is what makes the transport-aware rule structural rather than something each new call site has to remember on its own. (Confirmed via `grep` that `core/mcp_server.rs`'s tool handlers today call library functions directly and return structured values — zero raw `println!`/`eprintln!`/`cli::*` calls exist there currently — so this pass is about making the *convention* consistent and enforced everywhere going forward, not patching a live leak in the MCP layer today.)

Construction: CLI call sites always use `Output::cli()`. `core/mcp_server.rs`'s tool handlers build `Output::for_transport(self.config.transport)` before invoking anything that takes an `Output`.

**Tests:** extract the pure formatting (`ProgressEvent` → display string) into a small helper and unit-test it directly, independent of any stream. `Output::for_transport`'s stream selection is a pure function, unit-testable without spawning a server. End-to-end firing order/stream correctness can't be fully unit-tested without an LLM mock (none exists today for `compile`) — verify manually: run `compile`/`reindex` directly (stdout has per-item lines); run `okf-mcp http` and drive a compile tool call (stdout has per-item lines, no protocol corruption since HTTP doesn't use stdio); run `okf-mcp start` (stdio) and drive a compile tool call (per-item lines on stderr only, confirm zero stray stdout so the JSON-RPC stream stays clean).

---

## 11. Vault-level provider config actually takes effect (user item 4's deeper ask)

**Bug:** `.okf/config.toml`'s `[providers.<name>]` table (`base_url`, `api_key_env`) is parsed into `OkfVaultConfig` but never consulted anywhere outside its own unit tests — `compiler::provider::provider_spec` hardcodes everything itself, and CLI call sites always pass `CompileOptions::default()`. So a user editing vault config to point a provider at a custom URL sees it silently ignored.

**Fix:** add `api_key_env_override: Option<String>` to `CompileOptions` (alongside existing `base_url_override`). New `compiler::vault_provider_options(vault_root, model_spec)` reads the matching `[providers.<provider>]` entry (if any) from vault config and returns a populated `CompileOptions`; CLI call sites (`compile.rs`, `rebuild.rs`, `run.rs`) call this instead of `CompileOptions::default()`. `execute_compile_prompt` gains the new parameter, threading it into both the credential-seeding call and `AuthData` resolution (override takes precedence over the hardcoded `spec.api_key_env`). The existing generic scheme+trailing-slash normalization is untouched — vault-config-sourced URLs flow through the exact same resolution point and get the same normalization for free.

**Tests:** `driver.rs` — `vault_provider_options_reads_the_matching_providers_base_url_and_api_key_env` (populated for a matching provider, `None`/`None` for a vault with no matching entry). Manual: point `.okf/config.toml`'s `[providers.custom].base_url` at a local test server, run `compile --model custom/...`, confirm the request goes there.

**Noted, not fixed here:** MCP's `CompileArgs`/`RebuildArgs` already support their own explicit `base_url_override` and aren't changed — falling back to vault config when the MCP arg is `None` is a reasonable low-risk follow-up, out of this batch's scope.

---

## 12. Show available models on invalid-model error (user item 6)

**Bug:** no client-side model-existence check exists; an invalid model surfaces whatever raw error the provider's HTTP call returns. `genai::Client::all_model_names(adapter_kind, provider_config)` already exists in the dependency — Ollama does a live query, other adapters return a static baked-in list.

**Fix:** in `execute_compile_prompt`, detect a model-not-found-shaped error from `genai`'s `Error::WebModelCall`/`WebAdapterCall` variants (HTTP 404/400 whose body mentions "model"), and on match, call `all_model_names` for that adapter and append the resulting list to the error message before returning it. Since every `--model`-accepting command (compile/rebuild/run, CLI and MCP) funnels through this one function, this single change covers all of them.

**Tests:** unit tests constructing `genai::Error` variants directly (no network) to verify the detection predicate matches a 404/model-not-found body and doesn't false-positive on unrelated errors. Manual: `compile --model anthropic/not-a-real-model` with a valid key configured, confirm the error lists real model names.

---

## 13. `test-connection` covers all configured LLM providers (user item 11)

**Bug:** currently only probes Firecrawl.

**Fix:** extract endpoint/auth resolution out of `execute_compile_prompt` into a shared `resolve_provider_target(provider, ...)` (reused by item 12's fix too), add a public `KNOWN_PROVIDERS` list. `test-connection` iterates it: for each provider with a resolvable key (or Ollama, which needs none), attempt `all_model_names` and report status. Explicitly label the result per-provider: Ollama's is a real live reachability check; the other four are validated only as "we resolved a non-empty credential," since `genai`'s model list for them is static, not a live call — say so in the output rather than implying a full auth check happened.

**Tests:** the "is this provider configured" check reuses item 3's `env_var_is_meaningfully_set` pattern, unit-testable directly. Manual: run with no keys configured (all "not configured" except Ollama), then with one key set.

---

## 14. `config` command shows layered config by source (user item 12)

**Bug:** `cli/config.rs` currently only dumps the process-level Firecrawl `Config` struct — no vault config, no credential-storage presence, no visibility into which env vars are actually set.

**Fix:** rewrite to output a grouped structure: global config file (full path, exists?, sanitized contents), local config file (same), env vars actually set (`OKF_MCP_*` plus provider key env-var *presence*, redacted), a note that a `.env` in CWD is detected but never auto-loaded (confirmed: no `dotenv`/`dotenvy` dependency), vault-level `.okf/config.toml` if a vault resolves (path + sanitized contents), and per-provider credential-storage presence (`llm-<provider>` saved: true/false — never the value). Reuse `sanitizer.rs`'s existing redaction helper throughout.

**Tests:** update the existing `tests/cli_smoke.rs` config test — it currently asserts a flat `config_json["firecrawl_base_url"]` shape, which changes to nested `config_json["resolved"]["firecrawl_base_url"]` under the new output. New smoke test asserting the new top-level sections exist and credentials never leak raw values. Manual: run from inside a vault with `.okf/config.toml` present, then from outside any vault (should show `null` vault section, not error).

---

## Not a code fix: `product-brief.md` (user item 10a)

No scaffold/create-on-broken-link logic exists anywhere in okf-mcp. This is Obsidian's own behavior when clicking an unresolved `[[link]]` in its UI. Communicate this to the user; no commit needed.

---

## 15. Release: version bump, tag, push, monitor Actions

Once every item above is committed (each its own Conventional Commit — `fix:`, `feat:`, `test:`, `style:`, etc. as appropriate — and each landed only after its own `cargo test` is green, per the standing rule below) and the full suite passes with `scripts/coverage.sh`'s existing 70% production-coverage gate satisfied:

1. **Version bump**: this batch mixes bug fixes with genuine new features (`vault create`/`delete`, `kb` alias, vault-level provider config actually taking effect, model-not-found hints, multi-provider `test-connection`) and a CLI output *shape* change (`config`'s JSON restructure) — recommend a **minor** bump following this repo's existing convention (`0.2.0` → `0.2.1` was fix-only/patch; this batch is feature-adding) → **`0.3.0`**. Commit as `chore(release): bump version to 0.3.0`, matching the exact message style already used for `0.2.0`/`0.2.1`.
2. **Tag**: `git tag v0.3.0` (matching existing `v0.2.0`/`v0.2.1` tag convention).
3. **Push**: push commits and the tag to the remote — confirm with the user immediately before this step specifically, since it's the one action in this batch that's visible to others/hard to reverse (this is standing practice, not a plan caveat — flagging it here so it isn't skipped).
4. **Monitor**: watch the resulting Actions runs (`CI`, `Release artifacts`, `Docker Build`, `Publish container image`) via `gh run list`/`gh run watch`. Item 0 should mean `CI` is green from the very first commit onward, but confirm on the actual push — if anything fails (including the release-specific workflows, which haven't been exercised against this batch of changes), diagnose from the run log and fix in a follow-up commit, the same way item 0 was diagnosed.

---

## Verification (whole batch)

- **Every item's commit only lands once its own tests are green** — `cargo test` after each individual change, not just once at the end of the whole batch (applies to item 0 as much as every subsequent item).
- `cargo clippy` clean, `cargo fmt --check` clean (the latter is exactly what item 0 fixes and every subsequent commit must preserve).
- Coverage stays at or above the existing 70% production-coverage gate (`scripts/coverage.sh`) — this is enforced by CI already, not a new bar to build tooling for.
- End-to-end manual pass against a real small vault: `vault create` → `kb list` (confirm alias) → `ingest` a couple of sources (watch the "Fetching..." line) → `compile` (watch per-source progress lines on stdout) → kill/re-run to confirm resumability → `compile` again cleanly → `lint` (confirm pending-compile section behaves) → `reindex --embeddings` from two different working directories (confirm no re-download, watch progress) → `test-connection` → `config` (confirm layered output) → `vault delete --force`. Additionally: `okf-mcp http` with a tool client driving a `compile` call (progress on stdout, no protocol errors) and `okf-mcp start` with a stdio client driving the same (progress on stderr only, zero stray stdout).
- After release (item 15): confirm all GitHub Actions workflows are green on the pushed tag/commit, not just `CI`.
