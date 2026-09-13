# Raw/wiki extension: what shipped

Record of the five-item scope agreed for extending `okf-mcp-rs`'s raw/wiki
model toward the OKF layout proposed in a shared ChatGPT conversation. This
is the as-built companion to the plan that was approved before
implementation — kept because the plan document itself reads as a
forward-looking proposal, and this file instead says what actually landed,
in which commit, and where the implementation deviated from (or extended)
the plan text.

## Context

`okf-mcp-rs` already implemented the large majority of a `raw/ → wiki/ →
.okf/` architecture before this change: content-addressed `raw/`
(SHA-256 → `raw_<hash10>`), an append-only CAS manifest, an LLM compiler
pipeline writing `wiki/concepts/` and `wiki/entities/` with `sources:`
provenance frontmatter, and a `.okf/` layer holding only disposable state.
Google's own OKF spec is deliberately minimal (only `type:` is required),
so the gap here was scoped, reviewed by a second model reading the live
codebase, and narrowed to five items, implemented in this order:

1. Formalize `status`, `stale_after`, and `generated` in `WikiFrontmatter`.
2. Human-readable raw filenames (`raw/<raw_id>--<slug>.md`), with every
   consumer that treated a raw path as a literal filename converted to
   resolve it via `raw_id` instead — done as an explicit two-phase rollout
   (resolver-and-consumers first, filename change second) to keep the
   riskiest single item bisectable.
3. Four new optional `wiki/` content-type directories: `syntheses/`,
   `comparisons/`, `decisions/`, `questions/`.
4. `wiki/schema.md` + `wiki/log.md` scaffolding.
5. A provenance-traversal MCP tool, `okf-trace-provenance`.

`confidence` stayed explicitly out of scope throughout (never implemented,
no defined producer/consumer).

## Commits

All six landed on `main`, each `feat`/`refactor` scoped to one item (item 2
split into its two rollout phases), Sonnet-implemented with an Opus review
pass against the plan and `docs/design-gaps.md` before moving to the next
item, per the plan's delivery process:

| Commit | Item | Summary |
|---|---|---|
| `0616215` | 1 | `feat(wiki): model status, stale_after, and generated in WikiFrontmatter` |
| `ee39912` | 2 (phase 1) | `refactor(raw): resolve raw blobs by id instead of literal path` |
| `3ddf725` | 2 (phase 2) | `feat(raw): write human-readable slugged filenames for raw blobs` |
| `f9fb209` | 3 | `feat(wiki): add syntheses/comparisons/decisions/questions content dirs` |
| `129b1f2` | 4 | `feat(wiki): scaffold wiki/schema.md and wiki/log.md` |
| `59657e7` | 5 | `feat(mcp): add okf-trace-provenance tool` |

## Item 1 — `status`, `stale_after`, `generated`

Landed exactly as planned in `src/validator/frontmatter.rs`: three optional,
`#[serde(default)]` fields on `WikiFrontmatter` (`status: Option<String>`,
`stale_after: Option<String>`, `generated: Option<Generated { by, at }>`),
purely additive and advisory — no `lint` enforcement. The compiler prompt
already emitted all three; this only gave the struct a place to hold them,
so `okf-trace-provenance` (item 5) could read them directly instead of a
generic YAML re-parse.

## Item 2 — Human-readable raw filenames

**Phase 1** (`ee39912`) introduced the three resolver helpers in
`src/ingest/frontmatter.rs` — `raw_id_from_resource`, `raw_id_from_filename`,
`resolve_raw_path` — and converted every literal-path consumer the review
had identified: `validator/rules.rs`'s `missing_sources` check,
`validator/fix.rs`'s `.md`-append auto-fix, `explorer/graph.rs`
(`raw_source_title`), `explorer/render.rs`, `explorer/server.rs`
(`node_id_for_path`), `search/query.rs::collect_documents`,
`storage/bundle.rs`, `compiler/driver.rs::read_raw_body` and
`compiler/link_fix.rs`, `ingest/pipeline.rs`'s delete/purge path, and the
`assets/explorer.html` frontend (which stopped reconstructing
`'raw/' + id.slice(4) + '.md'` client-side and now resolves purely through
the backend `/api/page/raw:<raw_id>` endpoint). `manifest/model.rs::
SourceVersion` gained `#[serde(default)] raw_path: Option<String>`
(populated via a new `record_raw_path`, since `record_ingest` runs before
the blob is written and can't know the path yet) so `resolve_raw_path` gets
an O(1) manifest lookup with a directory-scan fallback for pre-existing
entries. Behavior-preserving by design: on-disk filenames were unchanged in
this commit.

**Phase 2** (`3ddf725`) flipped `write_raw_blob` to actually add the slug:
`raw/<raw_id>--<slug>.md`, slug derived (in order) from the raw body's own
H1 — via the existing `search::query::raw_title_and_body` extraction — then
the local file's stem, then the URL's last non-empty path segment, then its
host; sanitized to lowercase ASCII kebab-case, capped at 60 bytes, collapsed
so an embedded `--` in the source text can never survive into the slug
(which would be ambiguous with the `raw_id--slug` separator). When every
candidate sanitizes to nothing (all-unicode title, unrecognized source),
the file falls back to the bare `raw_id.md` shape, matching pre-existing
vaults exactly. Before writing, `write_raw_blob` checks `resolve_raw_path`
for an existing file at that `raw_id` (a second source URI hashing to the
same content) and reuses it rather than writing a second physical file,
preserving the "one `raw_id` = one physical file" invariant. `storage/
bundle.rs`'s `BundleRawSource` gained a `path` field recording the resolved
physical path (falling back to the pre-slug shape on resolution failure) so
external OKF consumers can locate the file without this project's
resolver. `docs/okf-pipeline-design.md` got an inline implementation note
at its raw-frontmatter section pointing at the real on-disk shape and the
concrete resolver functions, rather than leaving the doc's original
bare-hash examples uncorrected.

**Deviation from the plan text:** the plan flagged that `validator/fix.rs`'s
`.md`-append auto-fix "becomes largely moot" once resolution is by
`raw_id` and asked to "confirm what (if anything) still needs auto-fixing."
The answer turned out to be *nothing* — with `missing_sources` resolving by
`raw_id`, a `sources:` entry missing its `.md` extension already resolves
correctly and is never reported as missing in the first place, so that
whole code path (the `fixed_sources` field, its rewrite logic, its tests)
was deleted rather than kept. `FixReport` now only tracks
`fixed_frontmatter_typos` (the pre-existing `tid:`→`id:` repair).

**Not done:** the plan called this "a user-visible on-disk contract
change" and said to "bump the version and add a `CHANGELOG.md` entry" as
part of item 2. Neither happened in these six commits — no `CHANGELOG.md`
edit, and `Cargo.toml` is still at `0.12.2` as of `59657e7`. The plan's own
delivery process defers the version bump/tag/push to a later, separate
step once every item is implemented and verified; that step had not yet
run as of this writing.

## Item 3 — New `wiki/` content-type directories

`core::vault_resolver::wiki_content_dirs` widened from a fixed
`[PathBuf; 2]` to `Vec<(PathBuf, &'static str)>` pairing each directory
with its graph "kind" (`concept`, `entity`, `synthesis`, `comparison`,
`decision`, `question`). Every consumer that destructured the old
fixed-size array was updated to iterate the paired `Vec`: `validator/
fix.rs`, `validator/rules.rs`, `mcp_server.rs::find_concept_path`,
`storage/bundle.rs::build_bundle`, `search/query.rs::collect_documents`,
`compiler/driver.rs` (`referenced_raw_ids` and `regenerate_index`), and
`explorer/graph.rs` (which had its own hand-rolled
`wiki_content_dirs(...).zip(["concept", "entity"])`, replaced with the new
paired `Vec` directly). `assets/explorer.html`'s `KINDS`/`KIND_LABEL`/
`--c-*` color variables/`.k-*` classes/hub-filter checkboxes were extended
for the four new kinds.

Two explicit decisions the plan asked to be made during this item, both
resolved and tested:

- **Orphan-lint exemption**: `question`/`decision` pages are exempt from
  `okf-lint --strict`'s orphan check (`exempt_from_orphan_check` in
  `validator/rules.rs`) — they're naturally terminal in the graph rather
  than link targets. `synthesis`/`comparison` pages are *not* exempt; they
  are still expected to be reachable like concepts/entities.
- **`wiki/index.md` grouping**: `regenerate_index` now emits one `##`
  section per content directory, in `wiki_content_dirs`'s fixed order,
  each sorted by title, with a directory that has no pages yet omitted
  entirely rather than printing an empty heading — this replaced the
  single flat title-sorted list the plan noted would otherwise interleave
  six kinds together.

The compiler prompt (`src/compiler/prompts.rs`) generalized its Core
Mission line, the entities-vs-concepts routing rule, wikilink-target
resolution (now against *any* registered content dir), the Output Format
JSON examples, and the page-template note, adding lightweight routing
guidance for the four new categories exactly as scoped. `build_link_fix_user_prompt`
was deliberately left restricted to `concepts`/`entities` only, per the
plan's reasoning that letting link-fix freely decide a missing target is a
`questions`/`decisions` page risks fabricating unwarranted content.

## Item 4 — `wiki/schema.md` + `wiki/log.md`

`wiki/schema.md` is a static, hand-maintained template
(`assets/wiki-schema.md`, embedded via `include_str!` as
`compiler::driver::WIKI_SCHEMA_MD`) documenting the final content-dir list
from item 3, the frontmatter shape (including the item-1 fields), and
wikilink syntax — framed explicitly as this project's own convention, not
an OKF-spec requirement. `cli::vault::create` scaffolds it eagerly for new
vaults; `compiler::driver::ensure_wiki_schema` (called from
`regenerate_index`, write-if-missing, never clobbers a hand-edited file)
gives every pre-existing vault the file on its next compile/rebuild/
synthesize-submit, with no separate migration step, matching the plan.

`wiki/log.md` (`src/storage/wiki_log.rs`) is an append-only,
human-skimmable compile history. All **four** finalization paths the plan
identified now log a line each, both on success and failure:
`compiler::driver::compile` (used by both CLI and MCP compile/rebuild),
`cli::compile::report_and_commit` (the CLI's fix-then-commit tail),
`compiler::link_fix::fix_broken_links`, and MCP `okf-synthesize-submit`
(logged once per vault touched by a batch, mirroring how that same call
already batches its `regenerate_index` reindex). Every `wiki_log::append`
call is deliberately best-effort (`let _ = ...`) so a logging failure can
never turn an otherwise-successful run into an error, or mask a real one.
`report_and_commit`'s hand-built staged-path list — which the plan flagged
as the thing that would silently swallow `wiki/log.md` if forgotten — now
pushes both `wiki/schema.md` unconditionally and `wiki/log.md`
conditionally (only when it exists, since the append is best-effort and a
vault-relative git-add on a missing pathspec would otherwise fail the
whole commit).

## Item 5 — `okf-trace-provenance`

Added as `OkfServer::trace_provenance` in `src/core/mcp_server.rs`, MCP-only
(no CLI counterpart), matching the plan's own framing of this item as "a
new tool," not a CLI command. Resolves the target wiki page via the
item-3-generalized `find_concept_path`, parses it, and for each
`sources:` entry extracts `raw_id` via `raw_id_from_resource`, resolves the
physical file via `resolve_raw_path`, parses its `RawFrontmatter`, and
joins against the loaded manifest via a new `Manifest::find_version`-style
lookup. Returns the shape the plan specified: `{ page: { path, type, title,
status, stale_after, generated }, sources: [{ resource, raw_id, source_url,
checksum, ingested_at, manifest_status }] }`.

**Extension beyond the plan text:** the plan named three manifest statuses
a traced source could report — `ACTIVE`, `SUPERSEDED`, `TOMBSTONED` — plus
a graceful `"unresolvable"` for a malformed/unresolvable `resource` value.
The implementation adds a fourth, `UNTRACKED`, for a raw file that resolves
fine on disk but was never (or is no longer) recorded in the manifest —
e.g. dropped into `raw/` by hand — so that case reports honestly instead of
falsely claiming `ACTIVE`. `UNRESOLVABLE` itself carries a `reason` string
(no extractable `raw_id`; no file for that `raw_id`; the raw file's own
frontmatter fails to parse) rather than a bare status tag, giving one bad
citation on a page a diagnosable report instead of just an opaque flag —
and, per the plan's requirement, never aborts the whole call: the other
sources on the same page still trace normally.

The single-level-only limitation the plan called out up front — `resource`
always points directly at a raw file, never at another wiki page, so no
recursive graph walk was needed — held exactly as anticipated; no
multi-level claim citation was implemented, matching scope.

## Verification status

`cargo check` is clean as of `59657e7` (confirmed while writing this
document). Each item's own module tests were extended alongside the code
per the plan's per-track testing approach (visible in every commit's diff
above — e.g. the new orphan-exemption test, the `regenerate_index`
section-grouping tests, the slug-derivation edge-case tests, the
`wiki/log.md` line-presence assertions in `cli/compile.rs`,
`compiler/link_fix.rs`, and `mcp_server.rs`). This document does not itself
re-run the full `cargo test`/`cargo clippy`/`cargo fmt --check` gate or the
plan's manual end-to-end smoke pass (a live `okf compile` run against a
fixture corpus to confirm the LLM actually routes into the four new
directories, live provenance-tool calls against active/superseded/draft
pages, an `okf-lint --strict` pass over `questions/`/`decisions/` pages) —
those remain open verification steps if not already run interactively
before these commits landed.

## Outstanding from the plan's delivery process

Per the plan's own closing section, once every item lands the remaining
steps are: bump `Cargo.toml`'s version per semver (a minor bump, being a
`feat` set with one on-disk contract change), tag the release, and push.
As of `59657e7` none of that has happened yet — `Cargo.toml` is still
`0.12.2`, there is no new `CHANGELOG.md` entry for this work, and no tag
has been cut for it.
