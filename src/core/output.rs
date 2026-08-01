//! Transport-aware routing for user-facing structured/result output —
//! progress lines, command summaries, confirmations. Separate from
//! `core::logger`'s `tracing`-based diagnostic logging, which is
//! unconditionally routed to stderr (a stricter, already-correct policy
//! for log *noise*, orthogonal to this module's concern).
//!
//! Routing rule:
//! - CLI direct invocation (`okf-mcp <command>` run directly) -> stdout.
//! - MCP over HTTP transport -> stdout: HTTP's protocol framing lives on
//!   the network socket, not stdio, so stdout is free to use.
//! - MCP over stdio transport -> stderr: stdout there *is* the JSON-RPC
//!   channel to the client and must never carry anything else.

use super::config_schema::Transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy)]
pub struct Output(OutputStream);

impl Output {
    /// CLI direct invocation — always stdout, no transport concern.
    pub fn cli() -> Self {
        Output(OutputStream::Stdout)
    }

    /// An MCP tool invocation's output target, chosen by which transport
    /// is actually serving this call.
    pub fn for_transport(transport: Transport) -> Self {
        Output(if transport == Transport::Http {
            OutputStream::Stdout
        } else {
            OutputStream::Stderr
        })
    }

    pub fn line(&self, msg: &str) {
        match self.0 {
            OutputStream::Stdout => println!("{msg}"),
            OutputStream::Stderr => eprintln!("{msg}"),
        }
    }
}

/// Per-item progress for a loop over N sources/documents — `compile`,
/// `rebuild`, and `reindex --embeddings` all fire these around each item
/// in their existing loops. `label` carries the full "verb + subject"
/// text (e.g. `"compiling <uri>"`, `"indexing <path>"`) since the right
/// verb differs per caller; this type only owns the "[i/N] .../done/
/// failed" framing.
pub enum ProgressEvent {
    Started {
        index: usize,
        total: usize,
        label: String,
    },
    Finished {
        index: usize,
        total: usize,
        label: String,
        error: Option<String>,
    },
}

impl std::fmt::Display for ProgressEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgressEvent::Started {
                index,
                total,
                label,
            } => {
                write!(f, "[{index}/{total}] {label}...")
            }
            ProgressEvent::Finished { error: None, .. } => write!(f, "  done"),
            ProgressEvent::Finished {
                error: Some(err), ..
            } => write!(f, "  failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_transport_routes_http_to_stdout_and_stdio_to_stderr() {
        assert_eq!(
            Output::for_transport(Transport::Http).0,
            OutputStream::Stdout
        );
        assert_eq!(
            Output::for_transport(Transport::Stdio).0,
            OutputStream::Stderr
        );
    }

    #[test]
    fn cli_always_routes_to_stdout() {
        assert_eq!(Output::cli().0, OutputStream::Stdout);
    }

    #[test]
    fn started_event_formats_as_indexed_label_with_ellipsis() {
        let event = ProgressEvent::Started {
            index: 2,
            total: 5,
            label: "compiling https://example.com/a".to_string(),
        };
        assert_eq!(
            event.to_string(),
            "[2/5] compiling https://example.com/a..."
        );
    }

    #[test]
    fn finished_event_without_error_formats_as_done() {
        let event = ProgressEvent::Finished {
            index: 1,
            total: 1,
            label: "compiling https://example.com/a".to_string(),
            error: None,
        };
        assert_eq!(event.to_string(), "  done");
    }

    #[test]
    fn finished_event_with_error_formats_the_error_message() {
        let event = ProgressEvent::Finished {
            index: 1,
            total: 1,
            label: "compiling https://example.com/a".to_string(),
            error: Some("LLM call failed".to_string()),
        };
        assert_eq!(event.to_string(), "  failed: LLM call failed");
    }
}
