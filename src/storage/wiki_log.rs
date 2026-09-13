//! `wiki/log.md` — an append-only, human-skimmable summary of each compile
//! finalization pass.
//!
//! There isn't one single finalization point — there are four separate call
//! sites that each finish a run and are worth a line here:
//! [`crate::compiler::driver::compile`] (the core compile pipeline, used by
//! both the CLI and MCP `compile`/`rebuild` tools), `cli::compile`'s
//! `report_and_commit` (the CLI's own fix-then-commit tail), MCP
//! `okf-synthesize-submit` (which never calls `compile()` at all — it
//! applies client-synthesized operations directly), and
//! [`crate::compiler::link_fix::fix_broken_links`] (the LLM-assisted
//! broken-link repair, reachable from both the CLI and MCP `--fix` paths).
//! Each logs its own line rather than sharing one entry, since each is a
//! genuinely distinct "this much work finished, here's how it went" event.
//!
//! Git history already records every change at a much finer grain than this
//! file ever will — `wiki/log.md` exists purely so a human skimming the
//! vault doesn't have to spelunk `git log` to answer "when did this vault
//! last compile, and how did it go".

use std::io::Write;
use std::path::Path;

use crate::core::vault_resolver::sandbox_path;
use crate::storage::fs_ops;

const HEADER: &str = "# Compile Log\n\nAppend-only summary of each compile/fix/commit pass — see git history for the full detail behind each line.\n";

/// Appends one line to `wiki/log.md`, creating the file (with its header)
/// on first use.
///
/// - `source` names which of the finalization paths described in this
///   module's doc comment ran (e.g. `"compile"`, `"report_and_commit"`,
///   `"okf-synthesize-submit"`, `"fix_broken_links"`).
/// - `status` is a short, machine-skimmable tag (`"ok"`/`"error"`) — every
///   call site logs both successful and failed runs, never only the happy
///   path.
/// - `detail` is the free-form human summary (source/job counts, whether a
///   commit happened, etc).
///
/// Deliberately non-fatal to call incorrectly in isolation, but every
/// actual call site here treats this as best-effort: a run that already
/// wrote real content to disk must never fail (or roll back) just because
/// this human-readable summary couldn't be appended — see each call site's
/// own handling of this function's `Result`.
pub fn append(vault_root: &Path, source: &str, status: &str, detail: &str) -> anyhow::Result<()> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let line = format!("- {timestamp} [{source}] {status}: {detail}\n");

    let path = sandbox_path(vault_root, "wiki/log.md")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Gate the header on the file's existence, not on a successful read —
    // this must never mistake "unreadable" (non-UTF-8 content, a
    // permissions hiccup) for "empty" and clobber prior history. A real
    // appending open also keeps this O(1) per call rather than O(n) in the
    // log's size.
    let is_new = !fs_ops::exists(vault_root, "wiki/log.md");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if is_new {
        file.write_all(HEADER.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.write_all(line.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_the_file_with_a_header_on_first_use() {
        let vault = tempfile::tempdir().unwrap();
        append(vault.path(), "compile", "ok", "2 source(s) processed").unwrap();

        let content = fs_ops::read_to_string(vault.path(), "wiki/log.md").unwrap();
        assert!(content.starts_with("# Compile Log"));
        assert!(content.contains("[compile] ok: 2 source(s) processed"));
    }

    #[test]
    fn append_adds_a_new_line_without_touching_earlier_entries() {
        let vault = tempfile::tempdir().unwrap();
        append(vault.path(), "compile", "ok", "first run").unwrap();
        append(vault.path(), "report_and_commit", "error", "lint failed").unwrap();

        let content = fs_ops::read_to_string(vault.path(), "wiki/log.md").unwrap();
        assert!(content.contains("[compile] ok: first run"));
        assert!(content.contains("[report_and_commit] error: lint failed"));
        // The header must appear exactly once, no matter how many entries
        // have been appended.
        assert_eq!(content.matches("# Compile Log").count(), 1);
    }

    #[test]
    fn append_records_a_failed_run_with_an_error_status() {
        let vault = tempfile::tempdir().unwrap();
        append(
            vault.path(),
            "fix_broken_links",
            "error",
            "0 synthesized, 1 failed",
        )
        .unwrap();

        let content = fs_ops::read_to_string(vault.path(), "wiki/log.md").unwrap();
        assert!(content.contains("[fix_broken_links] error: 0 synthesized, 1 failed"));
    }

    #[test]
    fn append_preserves_non_utf8_prior_content_instead_of_treating_it_as_empty() {
        // A read-modify-write built on `read_to_string(...).unwrap_or_default()`
        // would mistake unreadable (non-UTF-8) content for "file is empty"
        // and clobber it with just the header plus the new line. A real
        // appending open must leave existing bytes alone no matter what
        // they are.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("wiki")).unwrap();
        let log_path = vault.path().join("wiki/log.md");
        std::fs::write(&log_path, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        append(vault.path(), "compile", "ok", "after binary content").unwrap();

        let raw = std::fs::read(&log_path).unwrap();
        assert_eq!(&raw[..4], &[0xff, 0xfe, 0x00, 0x01]);
        assert!(String::from_utf8_lossy(&raw).contains("[compile] ok: after binary content"));
        // Existing (non-empty) content means no header is (re)written.
        assert!(!String::from_utf8_lossy(&raw).contains("# Compile Log"));
    }
}
