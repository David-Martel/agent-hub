#![allow(dead_code)]
//! MCP tool-awareness discovery from a `.claude/mcp.json` configuration file.
//!
//! Reads a repo's `.claude/mcp.json` and produces a compact, human-readable
//! summary of available MCP servers and their tools, suitable for injection
//! into an agent preamble.
//!
//! # Example
//!
//! ```no_run
//! use agent_bus::mcp_discovery::discover_mcp_tools;
//!
//! let summary = discover_mcp_tools("/home/user/my-repo");
//! println!("{summary}");
//! // "MCP servers available: ast-grep (structural-search), serena (rust-lsp)"
//! ```

use std::path::Path;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Wire types — intentionally lenient (all fields optional)
// ---------------------------------------------------------------------------

/// Top-level `mcp.json` structure.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct McpConfig {
    /// Named server map, keyed by server identifier.
    #[serde(alias = "mcpServers")]
    servers: std::collections::HashMap<String, McpServer>,
}

/// A single server entry inside `mcp.json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct McpServer {
    /// Human-readable description (non-standard extension, present in some configs).
    description: Option<String>,
    /// The command used to start the server (e.g. `"npx"`, `"uvx"`).
    command: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read `<repo_path>/.claude/mcp.json` and return a formatted summary of
/// available MCP servers for injection into an agent preamble.
///
/// The return value is always a non-empty string: if the file is absent or
/// unparseable, a fallback message is returned instead of an error.
///
/// # Examples
///
/// ```no_run
/// let summary = discover_mcp_tools("C:/Users/david/my-repo");
/// assert!(!summary.is_empty());
/// ```
pub(crate) fn discover_mcp_tools(repo_path: impl AsRef<Path>) -> String {
    let mcp_path = repo_path.as_ref().join(".claude").join("mcp.json");

    let Ok(text) = std::fs::read_to_string(&mcp_path) else {
        return format!("No MCP config found at {}", mcp_path.display());
    };

    let config: McpConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(err) => {
            return format!(
                "MCP config at {} could not be parsed: {err}",
                mcp_path.display()
            );
        }
    };

    if config.servers.is_empty() {
        return format!(
            "MCP config loaded from {} — no servers defined",
            mcp_path.display()
        );
    }

    // Sort server names for a stable, deterministic preamble.
    let mut entries: Vec<(&String, &McpServer)> = config.servers.iter().collect();
    entries.sort_by_key(|(name, _)| name.as_str());

    let server_list: Vec<String> = entries
        .iter()
        .map(|(name, server)| format_server_entry(name, server))
        .collect();

    format!(
        "MCP servers available ({}): {}",
        server_list.len(),
        server_list.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_server_entry(name: &str, server: &McpServer) -> String {
    match &server.description {
        Some(desc) => format!("{name} ({desc})"),
        None => match &server.command {
            Some(cmd) => format!("{name} [cmd:{cmd}]"),
            None => name.to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(json: &str) -> McpConfig {
        serde_json::from_str(json).expect("valid JSON")
    }

    #[test]
    fn empty_servers_map_deserializes() {
        let cfg = parse_config(r#"{"servers": {}}"#);
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn mcp_servers_alias_deserializes() {
        let cfg = parse_config(
            r#"{"mcpServers": {"ast-grep": {"command": "npx", "description": "structural search"}}}"#,
        );
        assert!(cfg.servers.contains_key("ast-grep"));
    }

    #[test]
    fn format_entry_uses_description_when_present() {
        let server = McpServer {
            description: Some("structural search".to_owned()),
            command: Some("npx".to_owned()),
        };
        let result = format_server_entry("ast-grep", &server);
        assert_eq!(result, "ast-grep (structural search)");
    }

    #[test]
    fn format_entry_falls_back_to_command() {
        let server = McpServer {
            description: None,
            command: Some("uvx".to_owned()),
        };
        let result = format_server_entry("serena", &server);
        assert_eq!(result, "serena [cmd:uvx]");
    }

    #[test]
    fn format_entry_uses_name_only_when_both_absent() {
        let server = McpServer {
            description: None,
            command: None,
        };
        let result = format_server_entry("mystery", &server);
        assert_eq!(result, "mystery");
    }

    #[test]
    fn discover_mcp_tools_returns_fallback_for_missing_file() {
        let result = discover_mcp_tools("/nonexistent/path/that/does/not/exist");
        assert!(result.starts_with("No MCP config found"));
    }

    #[test]
    fn discover_mcp_tools_round_trip_with_temp_file() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join("mcp_discovery_test");
        let claude_dir = dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let json = r#"{
            "mcpServers": {
                "ast-grep": {"command": "npx", "description": "structural search"},
                "serena":   {"command": "uvx"}
            }
        }"#;
        let mcp_path = claude_dir.join("mcp.json");
        let mut f = std::fs::File::create(&mcp_path).unwrap();
        f.write_all(json.as_bytes()).unwrap();

        let summary = discover_mcp_tools(&dir);
        // Both servers should appear.
        assert!(summary.contains("ast-grep"), "summary: {summary}");
        assert!(summary.contains("serena"), "summary: {summary}");
        // Description takes precedence over command.
        assert!(summary.contains("structural search"), "summary: {summary}");
        // Count matches.
        assert!(summary.contains("(2)"), "summary: {summary}");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
