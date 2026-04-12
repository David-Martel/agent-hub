//! Inventory queries: active repos, active sessions, agents-by-repo, claims-by-repo.
//!
//! All functions work with existing data — no new Redis keys are required.
//! Repo and session identifiers are extracted from `repo:*` and `session:*` tags
//! on presence metadata and recent messages.

use std::collections::BTreeSet;

use crate::error::Result;

use crate::channels::{self, OwnershipClaim};
use crate::models::Presence;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Aggregated inventory of active repos and sessions discovered on the bus.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InventoryResult {
    /// Sorted unique repo names extracted from `repo:*` tags.
    pub active_repos: Vec<String>,
    /// Sorted unique session identifiers extracted from `session:*` tags.
    pub active_sessions: Vec<String>,
}

/// Repo-scoped view: agents and claims associated with a specific repository.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoInventory {
    /// The repository name that was queried.
    pub repo: String,
    /// Sorted unique agent names that have presence or recent messages tagged
    /// with `repo:<name>`.
    pub agents: Vec<String>,
    /// Open claims whose resource path starts with the repo name (or whose
    /// `repo_scopes` field contains it).
    pub claims: Vec<OwnershipClaim>,
}

// ---------------------------------------------------------------------------
// Tag extraction helpers (pure, tested independently)
// ---------------------------------------------------------------------------

/// Extract the value after a `prefix:` tag.  Returns `None` if the tag does
/// not start with the given prefix followed by a colon.
#[must_use]
fn extract_tag_value<'a>(tag: &'a str, prefix: &str) -> Option<&'a str> {
    tag.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix(':'))
        .filter(|value| !value.is_empty())
}

/// Scan a slice of tags and collect all `repo:*` values into `repos` and all
/// `session:*` values into `sessions`.
fn collect_repo_session_tags(
    tags: &[String],
    repos: &mut BTreeSet<String>,
    sessions: &mut BTreeSet<String>,
) {
    for tag in tags {
        if let Some(repo) = extract_tag_value(tag, "repo") {
            repos.insert(repo.to_owned());
        }
        if let Some(session) = extract_tag_value(tag, "session") {
            sessions.insert(session.to_owned());
        }
    }
}

/// Extract `repo:*` and `session:*` values from presence metadata.
///
/// The presence struct stores an arbitrary JSON `metadata` field.  Agents
/// conventionally include `"repo": "<name>"` or `"repos": [...]` and
/// `"session": "<id>"` keys.  We also look for a `"tags"` array inside
/// metadata to support the same `repo:*` / `session:*` convention.
fn collect_presence_metadata(
    presence: &Presence,
    repos: &mut BTreeSet<String>,
    sessions: &mut BTreeSet<String>,
) {
    let meta = &presence.metadata;

    // Check direct keys: "repo" and "session"
    if let Some(repo) = meta
        .get("repo")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
    {
        repos.insert(repo.to_owned());
    }
    if let Some(session) = meta
        .get("session")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
    {
        sessions.insert(session.to_owned());
    }

    // Check "repos" array
    if let Some(arr) = meta.get("repos").and_then(serde_json::Value::as_array) {
        for name in arr.iter().filter_map(serde_json::Value::as_str) {
            if !name.is_empty() {
                repos.insert(name.to_owned());
            }
        }
    }

    // Check "tags" array inside metadata (same repo:*/session:* convention)
    if let Some(arr) = meta.get("tags").and_then(serde_json::Value::as_array) {
        let tags: Vec<String> = arr
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(String::from)
            .collect();
        collect_repo_session_tags(&tags, repos, sessions);
    }

    // The session_id field on Presence itself is always populated.
    if !presence.session_id.is_empty() {
        sessions.insert(presence.session_id.clone());
    }
}

// ---------------------------------------------------------------------------
// Public operations
// ---------------------------------------------------------------------------

/// Scan presence records and recent messages for unique `repo:*` and
/// `session:*` tags.
///
/// Returns a sorted, deduplicated [`InventoryResult`].
///
/// # Errors
///
/// Returns an error if the Redis connection or SCAN/XREVRANGE commands fail.
pub fn list_active_repos_and_sessions(
    conn: &mut redis::Connection,
    settings: &Settings,
) -> Result<InventoryResult> {
    let mut repos = BTreeSet::new();
    let mut sessions = BTreeSet::new();

    // 1. Scan presence records
    let presences = crate::redis_bus::bus_list_presence(conn, settings)?;
    for p in &presences {
        collect_presence_metadata(p, &mut repos, &mut sessions);
    }

    // 2. Scan recent messages (last 24 hours, up to 10 000 entries)
    let messages = crate::redis_bus::bus_list_messages_from_redis(
        conn, settings, /* agent */ None, /* from_agent */ None,
        /* since_minutes */ 1440, /* limit */ 10_000, /* include_broadcast */ true,
    )?;
    for msg in &messages {
        collect_repo_session_tags(&msg.tags, &mut repos, &mut sessions);
    }

    Ok(InventoryResult {
        active_repos: repos.into_iter().collect(),
        active_sessions: sessions.into_iter().collect(),
    })
}

/// List agents that have presence or recent messages tagged with
/// `repo:<repo_name>`.
///
/// # Errors
///
/// Returns an error if the Redis connection or query commands fail.
pub fn agents_by_repo(
    conn: &mut redis::Connection,
    settings: &Settings,
    repo_name: &str,
) -> Result<Vec<String>> {
    let mut agents = BTreeSet::new();
    let repo_tag = format!("repo:{repo_name}");

    // Presence records
    let presences = crate::redis_bus::bus_list_presence(conn, settings)?;
    for p in &presences {
        let mut p_repos = BTreeSet::new();
        let mut sessions_unused = BTreeSet::new();
        collect_presence_metadata(p, &mut p_repos, &mut sessions_unused);
        if p_repos.contains(repo_name) {
            agents.insert(p.agent.clone());
        }
    }

    // Recent messages with the matching repo tag
    let messages = crate::redis_bus::bus_list_messages_from_redis(
        conn, settings, None, None, 1440, 10_000, true,
    )?;
    for msg in &messages {
        if msg.tags.iter().any(|t| t == &repo_tag) {
            agents.insert(msg.from.clone());
        }
    }

    Ok(agents.into_iter().collect())
}

/// List open claims whose resource matches a repository name.
///
/// A claim matches if:
/// - its `resource` field starts with `<repo_name>/`, or
/// - its `repo_scopes` vector contains the repo name.
///
/// # Errors
///
/// Returns an error if the Redis connection or SCAN/HGETALL commands fail.
pub fn claims_by_repo(settings: &Settings, repo_name: &str) -> Result<Vec<OwnershipClaim>> {
    let all_claims = channels::list_claims(settings, None, None)?;
    let prefix = format!("{repo_name}/");

    let filtered: Vec<OwnershipClaim> = all_claims
        .into_iter()
        .filter(|claim| {
            claim.resource.starts_with(&prefix)
                || claim.resource == repo_name
                || claim.repo_scopes.iter().any(|s| s == repo_name)
        })
        .collect();

    Ok(filtered)
}

/// Build a full repo-scoped inventory view.
///
/// Combines [`agents_by_repo`] and [`claims_by_repo`] into a single response.
///
/// # Errors
///
/// Returns an error if any underlying Redis query fails.
pub fn repo_inventory(
    conn: &mut redis::Connection,
    settings: &Settings,
    repo_name: &str,
) -> Result<RepoInventory> {
    let agents = agents_by_repo(conn, settings, repo_name)?;
    let claims = claims_by_repo(settings, repo_name)?;
    Ok(RepoInventory {
        repo: repo_name.to_owned(),
        agents,
        claims,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- extract_tag_value ---------------------------------------------------

    #[test]
    fn extract_tag_value_repo() {
        assert_eq!(
            extract_tag_value("repo:agent-bus", "repo"),
            Some("agent-bus")
        );
    }

    #[test]
    fn extract_tag_value_session() {
        assert_eq!(
            extract_tag_value("session:sprint-42", "session"),
            Some("sprint-42")
        );
    }

    #[test]
    fn extract_tag_value_no_match() {
        assert_eq!(extract_tag_value("priority:high", "repo"), None);
    }

    #[test]
    fn extract_tag_value_empty_value_returns_none() {
        assert_eq!(extract_tag_value("repo:", "repo"), None);
    }

    #[test]
    fn extract_tag_value_prefix_only_no_colon() {
        assert_eq!(extract_tag_value("repo", "repo"), None);
    }

    // -- collect_repo_session_tags -------------------------------------------

    #[test]
    fn collect_tags_mixed() {
        let tags = vec![
            "repo:agent-bus".to_owned(),
            "session:s1".to_owned(),
            "repo:deepwiki-rs".to_owned(),
            "priority:high".to_owned(),
            "session:s2".to_owned(),
        ];
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_repo_session_tags(&tags, &mut repos, &mut sessions);
        assert_eq!(
            repos.into_iter().collect::<Vec<_>>(),
            vec!["agent-bus", "deepwiki-rs"]
        );
        assert_eq!(sessions.into_iter().collect::<Vec<_>>(), vec!["s1", "s2"]);
    }

    #[test]
    fn collect_tags_empty() {
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_repo_session_tags(&[], &mut repos, &mut sessions);
        assert!(repos.is_empty());
        assert!(sessions.is_empty());
    }

    #[test]
    fn collect_tags_duplicates_are_deduped() {
        let tags = vec!["repo:agent-bus".to_owned(), "repo:agent-bus".to_owned()];
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_repo_session_tags(&tags, &mut repos, &mut sessions);
        assert_eq!(repos.len(), 1);
    }

    // -- collect_presence_metadata -------------------------------------------

    fn make_presence(metadata: serde_json::Value) -> Presence {
        Presence {
            agent: "test-agent".to_owned(),
            status: "online".to_owned(),
            protocol_version: "1.0".to_owned(),
            timestamp_utc: "2026-04-04T00:00:00.000000Z".to_owned(),
            session_id: "test-session".to_owned(),
            capabilities: vec![],
            metadata,
            ttl_seconds: 180,
        }
    }

    #[test]
    fn presence_metadata_repo_key() {
        let p = make_presence(serde_json::json!({"repo": "agent-bus"}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        assert!(repos.contains("agent-bus"));
        // session_id is always collected
        assert!(sessions.contains("test-session"));
    }

    #[test]
    fn presence_metadata_repos_array() {
        let p = make_presence(serde_json::json!({"repos": ["alpha", "beta"]}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        assert!(repos.contains("alpha"));
        assert!(repos.contains("beta"));
    }

    #[test]
    fn presence_metadata_session_key() {
        let p = make_presence(serde_json::json!({"session": "sprint-42"}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        assert!(sessions.contains("sprint-42"));
        assert!(sessions.contains("test-session")); // from session_id field
    }

    #[test]
    fn presence_metadata_tags_array() {
        let p = make_presence(serde_json::json!({"tags": ["repo:litho-book", "session:s99"]}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        assert!(repos.contains("litho-book"));
        assert!(sessions.contains("s99"));
    }

    #[test]
    fn presence_metadata_empty_object() {
        let p = make_presence(serde_json::json!({}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        // Only session_id is collected
        assert!(repos.is_empty());
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains("test-session"));
    }

    #[test]
    fn presence_metadata_ignores_empty_repo() {
        let p = make_presence(serde_json::json!({"repo": ""}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        assert!(repos.is_empty());
    }

    #[test]
    fn presence_metadata_ignores_empty_session() {
        let p = make_presence(serde_json::json!({"session": ""}));
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        // Only session_id field is collected, not the empty metadata value
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn presence_metadata_null_handled() {
        let p = make_presence(serde_json::Value::Null);
        let mut repos = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        collect_presence_metadata(&p, &mut repos, &mut sessions);
        assert!(repos.is_empty());
        assert!(sessions.contains("test-session"));
    }
}
