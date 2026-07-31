//! Parses the LLM's `{"operations": [...]}` response and applies it to
//! disk. This is the one place a model-generated `path` string reaches the
//! filesystem, so every write/delete goes through
//! `core::vault_resolver::sandbox_path` first.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::vault_resolver::sandbox_path;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "action")]
pub enum CompileOperation {
    #[serde(rename = "CREATE_OR_UPDATE")]
    CreateOrUpdate { path: String, content: String },
    #[serde(rename = "DELETE")]
    Delete { path: String, reason: String },
}

#[derive(Debug, Deserialize)]
pub struct CompilePayload {
    pub operations: Vec<CompileOperation>,
}

/// Strips a leading/trailing ```` ```json ```` or ```` ``` ```` fence, in
/// case the model wraps its response despite the system prompt's "no code
/// fences" instruction — cheap tolerance for a common way models deviate
/// from an output-format instruction, not a relaxation of the instruction
/// itself.
fn strip_code_fence(text: &str) -> &str {
    let text = text.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text);
    text.strip_suffix("```").unwrap_or(text).trim()
}

/// Rejects anything that isn't valid `CompilePayload` JSON outright — no
/// partial/best-effort parsing of a malformed or prose-wrapped response.
pub fn parse_compile_payload(raw_response: &str) -> anyhow::Result<CompilePayload> {
    let json = strip_code_fence(raw_response);
    serde_json::from_str(json)
        .map_err(|err| anyhow::anyhow!("LLM response was not valid CompilePayload JSON: {err}"))
}

/// Applies every operation in `payload`, in order, sandboxing each `path`
/// against `vault_root` first. Writes are write-tmp-then-rename for
/// crash-atomicity, matching `manifest::store::save`'s pattern.
pub fn apply_operations(vault_root: &Path, payload: &CompilePayload) -> anyhow::Result<Vec<PathBuf>> {
    let mut touched = Vec::with_capacity(payload.operations.len());
    for operation in &payload.operations {
        match operation {
            CompileOperation::CreateOrUpdate { path, content } => {
                let full_path = sandbox_path(vault_root, path)?;
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp_path = PathBuf::from(format!("{}.tmp", full_path.display()));
                std::fs::write(&tmp_path, content)?;
                std::fs::rename(&tmp_path, &full_path)?;
                touched.push(full_path);
            }
            CompileOperation::Delete { path, .. } => {
                let full_path = sandbox_path(vault_root, path)?;
                if full_path.is_file() {
                    std::fs::remove_file(&full_path)?;
                }
                touched.push(full_path);
            }
        }
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_payload() {
        let json = r#"{"operations": [
            {"action": "CREATE_OR_UPDATE", "path": "wiki/concepts/a.md", "content": "body"},
            {"action": "DELETE", "path": "wiki/concepts/old.md", "reason": "no active sources"}
        ]}"#;
        let payload = parse_compile_payload(json).unwrap();
        assert_eq!(payload.operations.len(), 2);
    }

    #[test]
    fn tolerates_a_json_code_fence_around_the_payload() {
        let fenced = "```json\n{\"operations\": []}\n```";
        assert!(parse_compile_payload(fenced).is_ok());
    }

    #[test]
    fn rejects_prose_wrapped_around_the_payload() {
        let prose = "Sure, here's the JSON:\n{\"operations\": []}\nHope that helps!";
        assert!(parse_compile_payload(prose).is_err());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_compile_payload("{not valid json").is_err());
    }

    #[test]
    fn rejects_an_unknown_action() {
        let json = r#"{"operations": [{"action": "RENAME", "path": "a.md"}]}"#;
        assert!(parse_compile_payload(json).is_err());
    }

    #[test]
    fn apply_operations_creates_and_updates_files() {
        let vault = tempfile::tempdir().unwrap();
        let payload = CompilePayload {
            operations: vec![CompileOperation::CreateOrUpdate {
                path: "wiki/concepts/new.md".to_string(),
                content: "hello".to_string(),
            }],
        };
        let touched = apply_operations(vault.path(), &payload).unwrap();
        assert_eq!(touched.len(), 1);
        assert_eq!(
            std::fs::read_to_string(vault.path().join("wiki/concepts/new.md")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn apply_operations_deletes_an_existing_file() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("wiki/concepts")).unwrap();
        std::fs::write(vault.path().join("wiki/concepts/old.md"), "x").unwrap();

        let payload = CompilePayload {
            operations: vec![CompileOperation::Delete {
                path: "wiki/concepts/old.md".to_string(),
                reason: "obsolete".to_string(),
            }],
        };
        apply_operations(vault.path(), &payload).unwrap();
        assert!(!vault.path().join("wiki/concepts/old.md").exists());
    }

    #[test]
    fn deleting_an_already_missing_file_is_not_an_error() {
        let vault = tempfile::tempdir().unwrap();
        let payload = CompilePayload {
            operations: vec![CompileOperation::Delete {
                path: "wiki/concepts/never-existed.md".to_string(),
                reason: "n/a".to_string(),
            }],
        };
        assert!(apply_operations(vault.path(), &payload).is_ok());
    }

    #[test]
    fn a_path_escaping_the_vault_is_rejected_not_written() {
        let vault = tempfile::tempdir().unwrap();
        let payload = CompilePayload {
            operations: vec![CompileOperation::CreateOrUpdate {
                path: "../../etc/passwd".to_string(),
                content: "malicious".to_string(),
            }],
        };
        assert!(apply_operations(vault.path(), &payload).is_err());
    }
}
