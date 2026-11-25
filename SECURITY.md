# Security Policy

## Supported Versions

Only the latest released version of `okf-mcp` is supported with security fixes. There is no LTS branch — please upgrade to the latest release before reporting an issue.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report it privately through one of these channels:

- **Preferred:** [GitHub Security Advisories](https://github.com/guercheLE/okf-mcp-rs/security/advisories/new) for this repository — lets us coordinate a fix and a disclosure timeline privately before anything is public.
- **Email:** guerchele@gmail.com — include as much detail as you can (affected version, reproduction steps, impact).

Please include:

- A description of the vulnerability and its potential impact.
- Steps to reproduce it, or a minimal proof of concept.
- The affected version(s) and platform(s), if known.

## What to Expect

- Acknowledgement of your report within a few days.
- An assessment of the issue and, where applicable, a fix released as soon as reasonably possible.
- Credit in the release notes, if you'd like it — let us know your preference when reporting.

## Scope

This covers the `okf-mcp` binary, its MCP tool surface, and the CLI. Vulnerabilities in third-party dependencies should generally be reported upstream as well; if a dependency issue affects `okf-mcp` directly (e.g. a vulnerable version we depend on), reporting it here is also welcome so it can be tracked and patched.
