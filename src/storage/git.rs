//! Shells out to the system `git` binary (via `std::process::Command`,
//! deliberately not the `git2`/libgit2 dependency) for the automated commit
//! after validation — design doc Q1: "Once validated, okf creates a
//! deterministic okf.json bundle manifest and executes an automated Git
//! commit." Assumes the vault is already a git repository; this module
//! never runs `git init` — okf-mcp doesn't own repo lifecycle.

use std::path::Path;
use std::process::{Command, Output};

pub struct CommitOutcome {
    pub committed: bool,
    pub stdout: String,
}

fn run_git(vault_root: &Path, args: &[&str]) -> anyhow::Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(vault_root)
        .args(args)
        .output()
        .map_err(|err| anyhow::anyhow!("failed to run 'git {}': {err}", args.join(" ")))
}

pub fn is_git_repository(vault_root: &Path) -> bool {
    run_git(vault_root, &["rev-parse", "--is-inside-work-tree"])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Stages `paths` (vault-relative) and commits with `message`. No-ops
/// (`committed: false`, nothing run) when `paths` is empty or nothing ends
/// up staged after `git add` — matching a `compile`/`lint` run that didn't
/// actually change any tracked file, which shouldn't produce an empty
/// commit.
pub fn commit(vault_root: &Path, paths: &[String], message: &str) -> anyhow::Result<CommitOutcome> {
    if !is_git_repository(vault_root) {
        anyhow::bail!(
            "'{}' is not a git repository — run 'git init' there first",
            vault_root.display()
        );
    }
    if paths.is_empty() {
        return Ok(CommitOutcome {
            committed: false,
            stdout: String::new(),
        });
    }

    let mut add_args = vec!["add", "--"];
    add_args.extend(paths.iter().map(String::as_str));
    let add_output = run_git(vault_root, &add_args)?;
    if !add_output.status.success() {
        anyhow::bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&add_output.stderr)
        );
    }

    // `git diff --cached --quiet` exits 0 when there's nothing staged to
    // commit, non-zero when there is — the inverse of most git plumbing,
    // but it's the documented way to check this without parsing output.
    let staged_diff = run_git(vault_root, &["diff", "--cached", "--quiet"])?;
    if staged_diff.status.success() {
        return Ok(CommitOutcome {
            committed: false,
            stdout: String::new(),
        });
    }

    let commit_output = run_git(vault_root, &["commit", "-m", message])?;
    if !commit_output.status.success() {
        anyhow::bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit_output.stderr)
        );
    }

    Ok(CommitOutcome {
        committed: true,
        stdout: String::from_utf8_lossy(&commit_output.stdout).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        assert!(run_git(dir, &["init", "--quiet"]).unwrap().status.success());
        assert!(
            run_git(dir, &["config", "user.email", "test@example.com"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            run_git(dir, &["config", "user.name", "Test"])
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn a_plain_directory_is_not_reported_as_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_git_repository(dir.path()));
    }

    #[test]
    fn commit_errors_when_the_vault_is_not_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "content").unwrap();
        let result = commit(dir.path(), &["a.md".to_string()], "message");
        assert!(result.is_err());
    }

    #[test]
    fn commit_stages_and_commits_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.md"), "content").unwrap();

        let outcome = commit(dir.path(), &["a.md".to_string()], "add a.md").unwrap();
        assert!(outcome.committed);

        let log = run_git(dir.path(), &["log", "--oneline"]).unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("add a.md"));
    }

    #[test]
    fn commit_with_no_paths_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let outcome = commit(dir.path(), &[], "nothing to commit").unwrap();
        assert!(!outcome.committed);
    }

    #[test]
    fn recommitting_unchanged_content_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.md"), "content").unwrap();
        commit(dir.path(), &["a.md".to_string()], "first").unwrap();

        let second = commit(dir.path(), &["a.md".to_string()], "second").unwrap();
        assert!(!second.committed);
    }
}
