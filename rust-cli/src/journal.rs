//! Per-repo message journal — export bus messages to local NDJSON files.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::models::Message;
use crate::postgres_store;
use crate::redis_bus;
use crate::settings::Settings;

/// Query messages by tag from PG (preferred) or Redis fallback.
///
/// Tries `PostgreSQL` first because it has a GIN index on the `tags` column.
/// Falls back to a Redis scan with client-side filtering when PG is not
/// configured or returns an empty result set.
///
/// # Errors
///
/// Returns an error if both the `PostgreSQL` and Redis queries fail.
pub(crate) fn query_messages_by_tag(
    settings: &Settings,
    tag: &str,
    since_minutes: u64,
    limit: usize,
) -> Result<Vec<Message>> {
    // Try PG first (has GIN index on tags).
    if settings.database_url.is_some() {
        if let Ok(msgs) = postgres_store::list_messages_by_tag(settings, tag, since_minutes, limit)
        {
            if !msgs.is_empty() {
                return Ok(msgs);
            }
        }
    }

    // Fallback: read all from Redis and filter client-side.
    let all = redis_bus::bus_list_messages(
        settings,
        None,
        None,
        since_minutes,
        limit.saturating_mul(5),
        true,
    )?;
    Ok(all
        .into_iter()
        .filter(|m| m.tags.iter().any(|t| t == tag))
        .take(limit)
        .collect())
}

/// Load already-exported message IDs from an NDJSON file.
fn load_existing_ids(path: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(file) = std::fs::File::open(path) else {
        return ids;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                ids.insert(id.to_owned());
            }
        }
    }
    ids
}

/// Append new messages to an NDJSON file (idempotent by message ID).
///
/// Creates the output file and any missing parent directories on first call.
/// Subsequent calls skip messages whose `id` is already present in the file.
///
/// # Errors
///
/// Returns an error if the directory cannot be created or the file cannot be
/// opened or written.
pub(crate) fn export_journal(messages: &[Message], output_path: &Path) -> Result<usize> {
    // Create parent directory if needed.
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).context("failed to create journal directory")?;
        }
    }

    let existing = load_existing_ids(output_path);
    let new_msgs: Vec<&Message> = messages
        .iter()
        .filter(|m| !existing.contains(&m.id))
        .collect();

    if new_msgs.is_empty() {
        return Ok(0);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output_path)
        .context("failed to open journal file")?;

    for msg in &new_msgs {
        let line = serde_json::to_string(msg).unwrap_or_default();
        writeln!(file, "{line}").context("failed to write journal entry")?;
    }

    maybe_deploy_protocol_doc(output_path);

    Ok(new_msgs.len())
}

/// Resolve the user's home directory from environment variables.
///
/// Tries `USERPROFILE` (Windows) then `HOME` (Unix/WSL).
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(std::path::PathBuf::from)
}

/// Deploy `AGENT_COMMUNICATIONS.md` to the nearest repo root if absent.
///
/// Walks up from `output_path` looking for a `.git` directory to identify
/// the repo root. When found and `AGENT_COMMUNICATIONS.md` is missing, it
/// copies the canonical file from
/// `~/.codex/tools/agent-bus-mcp/AGENT_COMMUNICATIONS.md`.
///
/// Failures are silently ignored — the journal export should not fail because
/// of a missing or unwritable documentation file.
fn maybe_deploy_protocol_doc(output_path: &Path) {
    let mut dir = output_path.parent();
    while let Some(d) = dir {
        if d.join(".git").exists() {
            let doc_path = d.join("AGENT_COMMUNICATIONS.md");
            if !doc_path.exists() {
                let canonical = home_dir()
                    .map(|h| h.join(".codex/tools/agent-bus-mcp/AGENT_COMMUNICATIONS.md"));
                if let Some(src) = canonical {
                    if src.exists() && std::fs::copy(&src, &doc_path).is_ok() {
                        tracing::info!(
                            "Deployed AGENT_COMMUNICATIONS.md to {}",
                            doc_path.display()
                        );
                    }
                }
            }
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(id: &str, tag: &str) -> Message {
        Message {
            id: id.to_owned(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "test".to_owned(),
            body: "body".to_owned(),
            thread_id: None,
            tags: if tag.is_empty() {
                vec![].into()
            } else {
                vec![tag.to_owned()].into()
            },
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        }
    }

    #[test]
    fn export_journal_writes_new_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ndjson");
        let msgs = vec![
            make_message("id-1", "repo:test"),
            make_message("id-2", "repo:test"),
        ];

        let written = export_journal(&msgs, &path).unwrap();
        assert_eq!(written, 2);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn export_journal_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ndjson");
        let msgs = vec![make_message("id-1", "repo:test")];

        let first = export_journal(&msgs, &path).unwrap();
        assert_eq!(first, 1);

        let second = export_journal(&msgs, &path).unwrap();
        assert_eq!(second, 0, "duplicate message must not be written again");

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn export_journal_appends_new_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ndjson");
        let batch1 = vec![make_message("id-1", "")];
        let batch2 = vec![make_message("id-1", ""), make_message("id-2", "")];

        export_journal(&batch1, &path).unwrap();
        let written = export_journal(&batch2, &path).unwrap();
        assert_eq!(written, 1, "only the new message should be appended");

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn export_journal_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("out.ndjson");
        let msgs = vec![make_message("id-x", "")];

        let written = export_journal(&msgs, &path).unwrap();
        assert_eq!(written, 1);
        assert!(path.exists());
    }

    #[test]
    fn load_existing_ids_returns_empty_for_missing_file() {
        let ids = load_existing_ids(Path::new("/nonexistent/path/file.ndjson"));
        assert!(ids.is_empty());
    }
}
