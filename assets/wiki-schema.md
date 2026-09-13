# Wiki Schema

This file documents **this vault's own conventions** for `./wiki/` — it is
not part of the OKF spec itself. The spec deliberately only requires a
`type:` field on a document; everything below (the content directories,
the extra frontmatter fields, the wikilink syntax) is `okf-mcp`'s own
convention, kept here so a human — or another tool — can see the actual
shape of this vault without reading the compiler's source.

## Content directories

Every wiki page lives in one of these content-type directories, each
paired with the graph/lint "kind" it's tagged with:

| Directory           | Kind         | For                                                                  |
|----------------------|--------------|-----------------------------------------------------------------------|
| `wiki/concepts/`     | `concept`    | Abstract ideas, processes, patterns, metrics.                         |
| `wiki/entities/`     | `entity`     | Concrete subjects: people, organizations, places, tools, technologies.|
| `wiki/syntheses/`    | `synthesis`  | Cross-cutting understanding connecting multiple existing concepts/entities. |
| `wiki/comparisons/`  | `comparison` | Explicit A-vs-B analysis of two or more existing entities/concepts.   |
| `wiki/decisions/`    | `decision`   | A durable decision extracted from evidence, not a new subject.        |
| `wiki/questions/`    | `question`   | An open or unresolved question, not requiring full atomic treatment.  |

Most material fits `concepts/` or `entities/` — the other four exist only
for material that genuinely doesn't fit as its own concept or entity. Only
`wiki/concepts/` is created eagerly by `okf-mcp vault create`; every other
directory (including `wiki/entities/`) is created lazily, the first time
the compiler actually routes something into it.

`wiki/index.md` (regenerated on every compile) and `wiki/log.md`
(append-only compile history — see below) live alongside these
directories, not inside any of them.

## Frontmatter

Every page opens with a YAML frontmatter block:

```yaml
---
type: <open string — e.g. concept, Person, synthesis, decision, question>
title: "<page title>"
description: "<one-line summary>"
sources:
  - resource: "/raw/<raw_id>.md"   # or "/raw/<raw_id>--<slug>.md"
    id: "<raw_id>"                 # optional
    title: "<original document title>"  # optional
tags: [tag-one, tag-two]           # optional
timestamp: "<ISO8601>"             # optional
status: draft                      # optional, advisory — not lint-enforced
stale_after: "2026-12-31"          # optional, advisory — not lint-enforced
generated:                         # optional provenance stamp
  by: "okf-mcp-compiler"
  at: "<ISO8601>"
---
```

Notes:

- `type` is an open string, not a fixed enum. `concepts/`/`entities/` pages
  commonly use the directory's own kind (`concept`) or a more specific
  value (`Person`, `Organization`, ...); the four newer directories start
  from their own kind name as a convention (`synthesis`, `comparison`,
  `decision`, `question`) — nothing enforces this.
- `sources` is this vault's provenance list. Every cited source resolves by
  its `raw_id` (the leading token of `resource`), not by matching the
  literal path string — `okf-lint` checks that every citation resolves to
  an actual file under `./raw/`.
- `okf_version` belongs only on the bundle-root `wiki/index.md` (per OKF
  v0.2 §12) — individual pages don't carry it.
- `status`, `stale_after`, and `generated` are advisory only; nothing in
  this vault enforces or acts on them beyond recording what the compiler
  wrote.

## Wikilinks

Cross-references between pages use Obsidian-style double-bracket syntax in
the page body:

- `[[slug]]` — resolves against *any* of this vault's registered content
  directories, not just `wiki/concepts/`.
- `[[slug|Display Text]]` — same resolution, with alternate display text.
- `[[other-vault::slug]]` — an explicit cross-vault reference. `okf-lint`
  flags these for visibility; nothing auto-resolves or auto-generates them.

## Raw sources

`./raw/<raw_id>--<slug>.md` (or, for content ingested before slugged
filenames existed, the bare `./raw/<raw_id>.md`) holds the immutable,
content-addressed evidence a wiki page's `sources:` cites. `raw_id` (the
hash-derived `raw_<first-10-hex-chars>` token) is the only stable identity
— every consumer in this project resolves a citation by extracting that
leading token, never by matching the literal filename.
