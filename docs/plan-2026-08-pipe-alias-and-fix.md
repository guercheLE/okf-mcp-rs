# okf-mcp: pipe-alias wikilinks + `--fix` auto-repair

## Context

While cleaning up broken links in a real `okf` vault by hand, every currently-reported "broken link" turned out to point at a concept page that genuinely exists on disk. The common thread: all of them were Obsidian-style piped links (`[[target|display text]]`) — `okf-mcp lint`'s wikilink parser had no support for the `|` alias separator, so it treated the entire `target|display text` string as the link's identity, which never matches a real filename and is always reported broken. This is a real bug in `okf-mcp`, not a vault-content problem, and hand-editing every piped link out of the vault's markdown would have been the wrong fix — the parser needed to support the syntax it's supposed to validate against.

Separately, that same cleanup pass surfaced two other classes of defect worth fixing at the tool level rather than by hand every time: a `sources:` `resource:` path missing its `.md` extension (a mechanical, single-correct-answer bug), and — reported from prior real-world runs — the LLM compiler occasionally emitting `tid:` where the OKF v0.2 schema requires `id:`, which made `okf-mcp lint` abort entirely rather than report it as one finding among many (a single malformed page hid every other page's findings). The user asked for `okf-mcp` itself to gain a `--fix` capability so none of this requires manual intervention next time, split into what's safely mechanical (no LLM) and what genuinely needs content synthesis (LLM-assisted, reusing the existing compile pipeline, gated behind explicit confirmation before anything gets committed).

See [docs/design-gaps.md](design-gaps.md) for what `okf-pipeline-design.md`/`okf-mcp-implementation-plan.md` failed to specify clearly enough to let each of these ship in the first place — updated in tandem with this plan.

## User decisions made for this plan

- LLM-driven synthesis of missing concept pages is bundled under a single `--fix` flag on `compile`/`rebuild`/`run` (not a separate flag) — `--fix` there does both the mechanical repairs and, for any broken links still remaining afterward, the LLM-assisted synthesis, in one pass. `lint --fix` stays mechanical-only and never touches an LLM, matching `lint`'s own design intent ("not framed as AI-powered anything, just a structural report").
- LLM-synthesized stub pages are grounded only in context that already exists: the pages that link to the missing slug, plus the raw sources those referencing pages themselves already cite. A missing slug with no such grounding is skipped, never guessed at from nothing.
- Committing LLM-synthesized changes requires confirmation, not silent auto-commit: a new `--yes`/`-y` flag skips the prompt; without it, an interactive terminal is prompted (`inquire::Confirm`, default declined); a non-interactive session (no TTY on stdin — CI, a pipe, a background job) never blocks waiting for input it can't get and defaults to *not* committing. The mechanical fixes (missing-`.md`-extension, `tid:`/`id:` typo) are low-risk and deterministic and are folded into the normal auto-commit flow without asking, same trust level as any other compile output.

## Implementation order (real code dependencies)

```
1. Pipe-alias parsing fix (validator::wikilink) — root cause, always-on, no flag, no dependents
2. Mechanical --fix (validator::fix) — depends on nothing but (1) being in place first
3. LLM-assisted --fix (compiler::link_fix) — depends on (2)'s LintReport shape
4. CLI wiring (main.rs, cli/{lint,compile,rebuild,run}.rs) — depends on (2) and (3) existing
5. MCP wiring (core/mcp_server.rs) — depends on (2) and (3), independent of (4)
```

## 1. Pipe-alias parsing fix

`src/validator/wikilink.rs`'s `parse_link` only special-cased `::` (cross-vault). Added a `|`-alias strip *before* the `::` split (order matters — splitting on `::` first would leave the alias glued onto `concept` in `[[vault::concept|Display]]`):

```rust
fn parse_link(inner: &str) -> WikiLink {
    let target = inner.split_once('|').map_or(inner, |(target, _display)| target);
    match target.split_once("::") {
        Some((vault, concept)) => WikiLink::CrossVault { vault: vault.to_string(), concept: concept.to_string() },
        None => WikiLink::Local(target.to_string()),
    }
}
```

`WikiLink`/`extract_wikilinks` have no consumers outside `wikilink.rs`/`rules.rs`, so discarding the display text is safe. Tests added in `wikilink.rs` (piped local/cross-vault links, multiple pipes use only the first, empty target before `|`) and `rules.rs` (a piped link to an existing page is not broken; a piped link to a missing page is still reported broken on the pre-`|` slug only).

## 2. Mechanical `--fix` (no LLM, deterministic only)

New module `src/validator/fix.rs`, exposing `fix_bundle(vault_root) -> anyhow::Result<(FixReport, LintReport)>` and `summary_line(&FixReport) -> String`. Two independent repairs, run in sequence:

- **`tid:` → `id:` frontmatter typo** (`fix_id_field_typos`, run *first*, before `lint_bundle` is ever called): a page whose frontmatter fails to parse makes `lint_bundle` itself return `Err` for the whole vault (a deliberate "surface as an error" design for genuinely malformed pages) — which would otherwise block every other fix, including the one below, from running at all. Only touches pages that currently fail to parse, only rewrites the `tid:` line inside the frontmatter block, and verifies the rewrite actually fixes parsing before keeping it; a page whose problem isn't this exact typo is left untouched.
- **Missing `.md` extension on `sources[].resource`**: for each `missing_sources` finding, checks whether appending `.md` resolves to a real file; if so, rewrites the exact quoted string in place (targeted string replace, not a full YAML round-trip — avoids reformatting hand-authored/LLM frontmatter).

`LintReport` gained `#[derive(Clone)]` so callers can hold a post-fix copy independently of `CompileReport`. Broken links and orphan pages are explicitly untouched by this module — see part 3.

CLI: `okf-mcp lint --fix` (mechanical only, never touches git — `lint` has no commit logic). MCP: `fix: Option<bool>` on `LintArgs`, folded into the `okf-lint` JSON response.

## 3. LLM-assisted `--fix` for broken links

`compile`/`rebuild` already require a model and already call the LLM compiler to turn raw sources into concept pages — extending that machinery to synthesize a missing *linked-to* concept page is a natural generalization. New module `src/compiler/link_fix.rs`, exposing `fix_broken_links(vault_root, model_spec, options, on_progress) -> anyhow::Result<LinkFixReport>`:

- Groups `lint_bundle`'s `broken_links` by missing slug.
- For each slug: loads every referencing page's full content, unions+dedups the raw-source ids their frontmatter cites, loads each as a `RawBlob`. Zero cited raw sources → `LinkFixStatus::SkippedNoSourceContext`, no LLM call.
- Otherwise builds a prompt via a new `build_link_fix_user_prompt` in `src/compiler/prompts.rs` (same section-labeled style as the existing `build_compile_user_prompt`, reusing `COMPILER_SYSTEM_PROMPT`), calls the LLM, parses+applies operations exactly like `compile_one_source` (`driver::read_raw_body` bumped from private to `pub(crate)` so this sibling module can reuse it).
- Emits `ProgressEvent::Started`/`Finished` per slug, mirroring `compile()`'s own loop.

## Commit gating

`cli/compile.rs`'s shared `report_and_commit` became `async fn report_and_commit(vault_root, report, commit_summary, fix, assume_yes, model_spec, options)`. When `fix` is set: runs the mechanical fix first (always folded in without asking), then, only if broken links remain, the LLM pass. Both passes' summaries print; both passes' touched paths merge into the git-staged path list; the post-fix `LintReport` replaces `report.lint_report` for the existing has-errors bail check (unchanged gate, now potentially already satisfied).

Before the actual `git::commit` call, if the LLM pass synthesized anything: a new `confirm_commit(message, assume_yes)` helper (mirrors `cli/setup_wizard.rs`'s existing `spawn_blocking` + `inquire::Confirm` pattern). If declined, the bundle is still written but the commit is skipped, non-error exit, pointing at `git commit`/`--yes` for next time.

`cli/compile.rs::run`, `cli/rebuild.rs::run`, `cli/run.rs::run` all thread `fix`/`yes` straight through (they already compute `model_spec`/`options` locally). New `--fix`/`--yes` (`-y`) flags on `Command::Compile`/`Command::Rebuild`/`Command::Run` in `src/main.rs`.

MCP `okf-compile`/`okf-rebuild` never call `report_and_commit` (no commit logic there at all) — `fix: Option<bool>` on `CompileArgs`/`RebuildArgs` runs both fix passes via a shared `apply_fix_if_requested` helper after `compiler::compile()` returns and folds `FixReport`/`LinkFixReport` into the JSON response (`"fix"`/`"link_fix"` keys); no prompt is possible or needed since nothing commits there.

## Deviations from the original design

None material. The frontmatter-typo fix (`tid:`/`id:`) was added mid-implementation, prompted by a report of the same failure mode from prior real-world `okf-mcp` runs — folded into the same mechanical-fix tier as the missing-`.md`-extension repair, run first since it unblocks `lint_bundle` itself.

## Verification

- `cargo test` (whole workspace: 258 lib + 14 bin + 15 `cli_smoke` + 2 `manifest_cas` + 3 `vault_sandboxing` — all green).
- `cargo clippy --all-targets` and `cargo fmt --check` — both clean.
- Manual: the real vault that surfaced this bug now lints clean on its 15 previously-broken piped links with zero file changes needed (parser fix alone resolves them).
