// Standalone entry point for Docker's HEALTHCHECK instruction.
//
// Loads the same config cascade the main server uses to determine which
// transport is actually running, then makes a transport-appropriate check:
// - HTTP transport: probes `GET /healthz` on the configured host:port
//   (the real liveness signal `http::server::healthz` already reports).
// - stdio transport: there's no listening endpoint to probe at all — the
//   process is a subprocess talking JSON-RPC over stdin/stdout, not a
//   network service — so this falls back to a structural check: can the
//   resolved vault's `.okf/` directory be read. Docker's HEALTHCHECK model
//   assumes one fixed check command regardless of transport, so this
//   binary has to make that transport-aware decision itself rather than
//   the Dockerfile choosing between two different check commands.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use okf_mcp::core::config_manager::load_config;
use okf_mcp::core::config_schema::Transport;
use okf_mcp::core::vault_resolver::resolve_vault;

fn check_http(host: &str, port: u16) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    let mut stream =
        TcpStream::connect(&addr).map_err(|err| format!("cannot connect to {addr}: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(5))))
        .map_err(|err| format!("failed to set socket timeout: {err}"))?;

    let request = format!("GET /healthz HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("failed to send request: {err}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| format!("failed to read response: {err}"))?;

    match response.lines().next() {
        Some(status_line) if status_line.contains(" 200 ") => Ok(()),
        Some(status_line) => Err(format!("unexpected /healthz response: {status_line}")),
        None => Err("empty response from /healthz".to_string()),
    }
}

fn check_structural() -> Result<(), String> {
    let vault_root = resolve_vault(None).map_err(|err| format!("cannot resolve a vault: {err}"))?;
    let marker = vault_root.join(".okf");
    if marker.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "vault '.okf' directory not found at '{}'",
            marker.display()
        ))
    }
}

fn main() {
    let config = match load_config(serde_json::Map::new()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("healthcheck failed: could not load configuration: {err}");
            std::process::exit(1);
        }
    };

    let result = if config.transport == Transport::Http {
        check_http(&config.host, config.port)
    } else {
        check_structural()
    };

    if let Err(err) = result {
        eprintln!("healthcheck failed: {err}");
        std::process::exit(1);
    }
}
