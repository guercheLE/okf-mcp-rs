# Design gaps: what the planning docs missed

A retrospective, in [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)-style dated entries, of what `docs/okf-pipeline-design.md` and `docs/okf-mcp-implementation-plan.md` failed to specify clearly enough — and what shipped as a bug or a missing feature as a direct result. Each entry pairs **what was missing** with **the fix/feature it took**, and closes with a **lesson** meant to transfer to *other* projects' planning docs, not just this one.

Update this file in tandem with [CHANGELOG.md](../CHANGELOG.md): whenever a future bug traces back to an ambiguous or absent design-doc requirement, add an entry here alongside the CHANGELOG entry for the fix.

## [0.3.0] - 2026-08-01

### Gap: example values not marked illustrative vs. binding
**What was missing:** `docs/okf-pipeline-design.md` shows the raw-source frontmatter schema twice — an early illustrative example (line 59) uses `okf_version: "1.0"`, while the later "reference implementation" code snippet (line 222) uses `okf_version: "0.2"`. Neither is marked as the authoritative one.
**What it took:** `src/ingest/frontmatter.rs` implemented against the first example (`"1.0"`), while every other schema location (compiler prompt, bundle, wiki fixtures) implemented against the second (`"0.2"`) — two hardcoded constants drifted apart for over a hundred commits before anyone noticed, surfacing only as a lint/consistency complaint from real usage.
**Lesson:** when a design doc shows the same artifact's shape more than once (an early "here's roughly what this looks like" example and a later "here's the actual struct/code" reference), keep every literal value identical across all of them, or explicitly label one "illustrative, not binding." An implementer — human or LLM — will treat whichever version they read first as ground truth.

### Gap: no full CRUD lifecycle specified for a stateful, user-managed entity
**What was missing:** the design doc introduces the vault concept (a `.okf/`-marked directory, registered in `~/.config/okf/vaults.toml`) but only ever discusses *registering* one — no mention of scaffolding a brand-new vault, or of removing one from the registry vs. deleting its data.
**What it took:** three separate follow-up commands (`vault create`, `vault remove`, `vault delete --force`) added after the fact, each requiring its own destructive-action safety design (confirm before delete, delete-then-unregister ordering) that a single up-front CRUD enumeration would have surfaced together.
**Lesson:** the moment a design introduces a stateful, user-managed entity, write out its full CRUD lifecycle explicitly — create, list, update, delete — and for anything filesystem-backed, distinguish "register/adopt existing" from "create new," and "unregister" from "permanently delete data." Doing this once, together, catches the destructive-action safety questions (confirmation, ordering) in one pass instead of piecemeal.

### Gap: precedence-chain predicate left implicit
**What was missing:** neither doc specifies what "the env var takes priority over the keychain" actually means at the boundary — is a variable that's *set but empty* "configured" or not?
**What it took:** `seed_env_from_credential_storage` used `std::env::var(name).is_ok()`, which is `true` even for `""`, so any stray blank env var silently shadowed a correctly-saved credential and produced a 401 that looked like a setup problem.
**Lesson:** when specifying any precedence/fallback chain that involves environment variables, state the exact predicate for "already configured" (e.g. "present *and* non-empty after trimming"). "X overrides Y" is ambiguous about edge values, and implementers will reach for the shortest correct-looking check, which is usually wrong at the boundary.

### Gap: pipeline diagram implied an invariant it never stated
**What was missing:** the design doc's pipeline diagram shows `Validator → [ Validated OKF Bundle ] (Immutable Git commit + manifest bundle)` as one downstream box, implying validate-then-commit — but nowhere does the doc say, as an explicit rule, "never commit if validation failed."
**What it took:** `report_and_commit` wrote `okf.json` and committed unconditionally, checking pass/fail *afterward* only to set the exit code — a partial/broken compile's inconsistent state landed in git history before the failure was even reported.
**Lesson:** when a pipeline diagram implies an ordering or gating relationship between two steps, promote it from the diagram into an explicit numbered invariant ("never do X if Y failed"). Diagrams are skimmed, not read as specifications, and their implied control flow is easy to accidentally invert while writing the real sequential code.

### Gap: no UX requirement for long-running, latency-variable operations
**What was missing:** neither doc says anything about progress/liveness feedback for `compile` (which depends on third-party or local LLM inference — inherently variable latency, sometimes minutes).
**What it took:** a user force-killed a `compile` run after it sat silent for a long time, assuming it had hung — which then produced a partially-compiled, inconsistent wiki (closing the loop with the previous gap).
**Lesson:** any planned operation that depends on an external, latency-variable resource needs an explicit UX requirement for progress indication in the design doc, not just its data-flow shape. "No output for N minutes" is indistinguishable from "hung" to a user, and this is easy to omit from a doc focused on architecture rather than interaction.

### Gap: CWD-relative resolution pattern not scoped to what should use it
**What was missing:** the design doc establishes "vaults resolve relative to CWD" as a core pattern but never states which *other* state must deliberately avoid that pattern.
**What it took:** the local embedding model's cache directory defaulted to a CWD-relative path (an upstream crate default, never overridden), so the ~415 MiB model re-downloaded on every invocation from a different working directory — especially likely as an MCP subprocess spawned by a host app.
**Lesson:** when a design commits to resolving state relative to the current working directory as a core pattern, explicitly call out which other state (caches, temp files, downloaded artifacts) must instead be pinned to a stable, absolute location. It's an easy default to reach for by analogy, and produces cache behavior that silently depends on invocation context.

### Gap: two independently-persisted pieces of state behind one wizard flow, without a stated separation
**What was missing:** the design separates credential storage (OS keychain) from config persistence (YAML/env/print) as two concerns, but never requires the setup wizard's *prompts* to make that separation legible in the moment.
**What it took:** a user picked "save as global YAML" expecting it to also persist their LLM provider key (already saved to the keychain, unconditionally, before that question was even asked) — a reasonable but wrong mental model that sent them looking in the wrong place when debugging.
**Lesson:** whenever a design combines two independently-persisted pieces of state behind one user-facing flow, require the flow's own copy to state which choice applies to which piece — otherwise the natural assumption ("everything I just set up saves together") goes unchallenged.

### Gap: a two-part feature's second half was never scheduled
**What was missing:** `.okf/config.toml`'s `[providers.<name>]` table was fully specified, implemented, and unit-tested (parsing) — but nothing in the plan tracked "and then actually consult it when resolving a provider," so that half never happened.
**What it took:** a user set a custom provider base URL/key-env in vault config and had it silently ignored for months, since `compiler::provider` hardcoded its own routing table independently.
**Lesson:** when a feature naturally splits into "parse/store config" and "consume config," treat it as one unsplittable unit of work, or add an acceptance test that asserts the config actually *changes behavior* — not just that it round-trips through serde. The parsing half looks completely done (compiles, has tests, matches the doc's example) while the consuming half silently never ships.

### Gap: no error-message quality bar for unvalidated user input
**What was missing:** neither doc sets an expectation for what a CLI error should look like when the tool has enough context to make it self-service — here, an invalid `<provider>/<model>` string, with a live provider connection available to list what *would* have worked.
**What it took:** an invalid model name surfaced whatever raw HTTP error the provider happened to return, with no indication of valid alternatives, until a follow-up feature added a same-request model-list lookup on failure.
**Lesson:** for any CLI whose primary interaction is "pass a string identifier the tool doesn't validate ahead of time," design docs should set an explicit bar for error quality — especially when the tool already has the information on hand (a live connection, a known enum) to make the error self-service instead of sending the user elsewhere.

### Gap: diagnostic/introspection commands not re-scoped after an architecture pivot
**What was missing:** this project's earlier incarnation was a single-integration Firecrawl proxy (see `feat!: rebuild okf-mcp as an OKF pipeline engine, replacing the generic Firecrawl proxy`); `test-connection` and `config` were never revisited after the pivot to a multi-provider pipeline, so they stayed Firecrawl-only.
**What it took:** both commands needed a full rewrite — `test-connection` to probe every configured LLM provider, `config` to show vault-level and per-provider state — neither of which the post-pivot design doc flagged as follow-up work.
**Lesson:** when a project pivots its core architecture, explicitly audit every diagnostic/introspection surface (health checks, connection tests, config dumps, `--help` text) for assumptions baked in under the old architecture. These commands sit off the main data-flow path a redesign usually focuses on, but they're exactly what a confused user reaches for first — and stale assumptions there are invisible until someone hits them.
