//! Inbox and context compaction operations.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::error::Result;
use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::models::Message;
use crate::redis_bus::{Notification, list_notifications_since_id, notification_cursor_key};
use crate::settings::Settings;

use super::MessageFilters;
use super::message::ReadMessagesRequest;
use super::message::{list_messages_history, list_messages_live};

// ---------------------------------------------------------------------------
// CheckInbox
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CheckInboxRequest<'a> {
    pub agent: &'a str,
    pub limit: usize,
    pub reset_cursor: bool,
    pub filters: MessageFilters<'a>,
}

#[derive(Debug)]
pub struct CheckInboxResult {
    pub messages: Vec<Message>,
    pub cursor_was: String,
    pub cursor_now: String,
    pub inbox_cursor_key: String,
}

/// Check an agent's inbox using cursor-based notification delivery.
///
/// Scans at most 4,096 entries in pages of 256, stopping at the requested match
/// count. The cursor advances over examined entries, including nonmatches,
/// but never over an unexamined page tail. An empty result with an advanced
/// cursor can mean the scan budget was exhausted; call again to continue.
///
/// Each effective filter combination has an independent versioned cursor.
/// Repo and session are conjunctive tags; tag order, duplicates and empty
/// tags do not distinguish cursors. Thread and topic remain exact matches.
/// Unfiltered reads retain the existing global key. Filtered reads start from
/// the beginning of retained history once after upgrading; unsafe legacy
/// scoped/global positions are neither inherited nor changed. Filtered
/// cursors expire seven days after their last advance or explicit reset.
///
/// `reset_cursor` resets only the selected lane before reading. A zero limit
/// does not scan or advance, but still honours an explicit reset.
///
/// # Errors
///
/// Returns an error if the Redis connection or stream commands fail.
pub fn check_inbox(
    conn: &mut redis::Connection,
    request: &CheckInboxRequest<'_>,
) -> Result<CheckInboxResult> {
    check_inbox_with_store(conn, request)
}

const INBOX_PAGE_SIZE: usize = 256;
const INBOX_SCAN_LIMIT: usize = 4_096;

// A small storage seam keeps cursor/pagination regressions independent of live
// agent state. Production still uses the same Redis notification decoder.
trait InboxStore {
    fn cursor(&mut self, key: &str) -> Result<String>;
    fn save_cursor(&mut self, key: &str, value: &str) -> Result<()>;
    fn page(&mut self, agent: &str, cursor: &str, limit: usize) -> Result<Vec<Notification>>;
}

impl<T: redis::ConnectionLike> InboxStore for T {
    fn cursor(&mut self, key: &str) -> Result<String> {
        let value: Option<String> = redis::cmd("GET").arg(key).query(self)?;
        Ok(value.unwrap_or_else(|| "0-0".to_owned()))
    }

    fn save_cursor(&mut self, key: &str, value: &str) -> Result<()> {
        let mut command = redis::cmd("SET");
        command.arg(key).arg(value);
        if key.starts_with("bus:notify_filter_cursor:") {
            command.arg("EX").arg(crate::redis_bus::CURSOR_TTL_SECS);
        }
        command.query::<()>(self)?;
        Ok(())
    }

    fn page(&mut self, agent: &str, cursor: &str, limit: usize) -> Result<Vec<Notification>> {
        list_notifications_since_id(self, agent, cursor, limit)
    }
}

fn inbox_cursor_key(request: &CheckInboxRequest<'_>, required_tags: &[String]) -> Result<String> {
    if required_tags.is_empty()
        && request.filters.thread_id.is_none()
        && request.filters.topic.is_none()
    {
        return Ok(notification_cursor_key(request.agent));
    }
    // JSON encodes component boundaries and None versus Some("") unambiguously.
    let identity = serde_json::to_vec(&(
        request.agent,
        required_tags,
        request.filters.thread_id,
        request.filters.topic,
    ))?;
    Ok(format!(
        "bus:notify_filter_cursor:{}:v1:{}",
        request.agent,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(identity))
    ))
}

fn check_inbox_with_store(
    store: &mut impl InboxStore,
    request: &CheckInboxRequest<'_>,
) -> Result<CheckInboxResult> {
    let mut required_tags = super::scoped_required_tags(&request.filters);
    required_tags.sort_unstable();
    required_tags.dedup();
    let key = inbox_cursor_key(request, &required_tags)?;
    let required_tag_refs: Vec<&str> = required_tags.iter().map(String::as_str).collect();
    if request.reset_cursor {
        store.save_cursor(&key, "0-0")?;
    }
    let cursor = store.cursor(&key)?;
    let mut cursor_now = cursor.clone();
    let mut messages = Vec::new();
    let mut scanned = 0;
    while messages.len() < request.limit && scanned < INBOX_SCAN_LIMIT {
        let page_size = INBOX_PAGE_SIZE.min(INBOX_SCAN_LIMIT - scanned);
        let page = store.page(request.agent, &cursor_now, page_size)?;
        let page_len = page.len();
        for notification in page {
            let stream_id = notification.notification_stream_id.ok_or_else(|| {
                crate::error::AgentBusError::Internal("Notification has no stream cursor".into())
            })?;
            cursor_now = stream_id;
            scanned += 1;
            if crate::redis_bus::message_matches_filters(
                &notification.message,
                Some(request.agent),
                None,
                true,
                request.filters.topic,
                request.filters.thread_id,
                &required_tag_refs,
            ) {
                messages.push(notification.message);
                if messages.len() == request.limit {
                    break;
                }
            }
        }
        if page_len < page_size {
            break;
        }
    }

    // Commit once: a failed later page must not consume unreturned messages.
    if cursor_now != cursor {
        store.save_cursor(&key, &cursor_now)?;
    }

    Ok(CheckInboxResult {
        messages,
        cursor_was: cursor,
        cursor_now,
        inbox_cursor_key: key,
    })
}

// ---------------------------------------------------------------------------
// CompactContext
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CompactContextRequest<'a> {
    pub agent: Option<&'a str>,
    pub filters: MessageFilters<'a>,
    pub since_minutes: u64,
    pub max_tokens: usize,
}

#[derive(Debug)]
pub struct CompactContextResult {
    pub messages: Vec<serde_json::Value>,
    /// Estimated token count of the compacted output.
    ///
    /// Available for callers that need budget tracking (e.g. the HTTP handler).
    pub token_count: usize,
    /// True when the message list was trimmed to meet the token budget.
    ///
    /// Available for callers that want to signal truncation to the consumer.
    pub truncated: bool,
}

/// Fetch recent messages from the live Redis stream and compact them to fit
/// within a token budget.
///
/// Messages are fetched directly from Redis (bypassing `PostgreSQL`) to keep
/// this latency-sensitive, token-oriented read path fast. The result is a
/// slice of minimised message JSON values ordered oldest-first, trimmed to
/// the token budget.
///
/// # Errors
///
/// Returns an error if the Redis connection or stream read fails.
pub fn compact_context(
    conn: &mut redis::Connection,
    settings: &Settings,
    request: &CompactContextRequest<'_>,
) -> Result<CompactContextResult> {
    const FETCH_LIMIT: usize = 500;

    let msgs = list_messages_live(
        conn,
        settings,
        &ReadMessagesRequest {
            agent: request.agent,
            from_agent: None,
            since_minutes: request.since_minutes,
            limit: FETCH_LIMIT,
            include_broadcast: true,
            filters: MessageFilters {
                repo: request.filters.repo,
                session: request.filters.session,
                tags: request.filters.tags,
                thread_id: request.filters.thread_id,
                topic: None,
            },
        },
    )?;

    let original_len = msgs.len();
    let compacted = crate::token::compact_context(&msgs, request.max_tokens);
    let token_count = compacted
        .iter()
        .map(|v| crate::token::estimate_tokens(&v.to_string()))
        .sum();
    let truncated = compacted.len() < original_len;

    Ok(CompactContextResult {
        messages: compacted,
        token_count,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// Session / Inbox Summarization
// ---------------------------------------------------------------------------

/// Compact rollup of a session's message history, optimised for LLM consumers.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    /// Number of distinct sender agents.
    pub agent_count: usize,
    /// Total messages in the summarised window.
    pub message_count: usize,
    /// Topics sorted by frequency (descending).
    pub topic_histogram: Vec<(String, usize)>,
    /// Sorted list of unique sender agent IDs.
    pub active_agents: Vec<String>,
    /// `(earliest, latest)` ISO-8601 timestamps, if any messages exist.
    pub time_range: Option<(String, String)>,
    /// Severity label -> count (extracted from `SEVERITY:` lines in bodies).
    pub severity_counts: HashMap<String, usize>,
    /// Unique `thread_id` values present in the message set.
    pub open_threads: Vec<String>,
}

/// Parameters for [`summarize_session`].
#[derive(Debug)]
pub struct SummarizeSessionRequest<'a> {
    /// Optional agent filter (recipient / direction).
    pub agent: Option<&'a str>,
    /// Optional `repo:<name>` tag filter.
    pub repo: Option<&'a str>,
    /// Optional `session:<id>` tag filter.
    pub session: Option<&'a str>,
    /// How far back to look, in minutes.
    pub since_minutes: u64,
    /// Maximum number of messages to consider.
    pub limit: usize,
}

/// Parameters for [`summarize_inbox`].
#[derive(Debug)]
pub struct SummarizeInboxRequest<'a> {
    /// Agent whose inbox to summarise (required).
    pub agent: &'a str,
    /// Optional `repo:<name>` tag filter.
    pub repo: Option<&'a str>,
    /// Optional `session:<id>` tag filter.
    pub session: Option<&'a str>,
    /// Maximum number of inbox items to consider.
    pub limit: usize,
    /// Whether to reset the inbox cursor before reading.
    pub reset_cursor: bool,
}

/// Result of [`summarize_inbox`].
#[derive(Debug, Clone, Serialize)]
pub struct SummarizeInboxResult {
    /// The aggregated summary.
    pub summary: SessionSummary,
    /// Inbox cursor value before the read.
    pub cursor_was: String,
    /// Inbox cursor value after the read.
    pub cursor_now: String,
}

/// Build a [`SessionSummary`] from an already-fetched slice of messages.
///
/// This is the pure aggregation kernel shared by both `summarize_session` and
/// `summarize_inbox`. It is deterministic: given the same input messages in the
/// same order, the output is identical.
#[must_use]
pub fn aggregate_messages(msgs: &[Message]) -> SessionSummary {
    let mut agents: BTreeSet<String> = BTreeSet::new();
    let mut topics: BTreeMap<String, usize> = BTreeMap::new();
    let mut severities: HashMap<String, usize> = HashMap::new();
    let mut threads: BTreeSet<String> = BTreeSet::new();
    let mut first_ts: Option<&str> = None;
    let mut last_ts: Option<&str> = None;

    for msg in msgs {
        agents.insert(msg.from.clone());
        *topics.entry(msg.topic.clone()).or_insert(0) += 1;

        if let Some(ref tid) = msg.thread_id {
            threads.insert(tid.clone());
        }

        // Extract severity from body: look for SEVERITY:<level> pattern.
        for line in msg.body.lines() {
            let line_upper = line.to_uppercase();
            if let Some(rest) = line_upper.strip_prefix("SEVERITY:") {
                let level = rest.trim();
                let key = if level.starts_with("CRITICAL") {
                    "CRITICAL"
                } else if level.starts_with("HIGH") {
                    "HIGH"
                } else if level.starts_with("MEDIUM") {
                    "MEDIUM"
                } else if level.starts_with("LOW") {
                    "LOW"
                } else {
                    continue;
                };
                *severities.entry(key.to_owned()).or_insert(0) += 1;
            }
        }

        // Track time range using ISO-8601 string ordering (lexicographic = chronological).
        let ts = msg.timestamp_utc.as_str();
        if first_ts.is_none_or(|f| ts < f) {
            first_ts = Some(ts);
        }
        if last_ts.is_none_or(|l| ts > l) {
            last_ts = Some(ts);
        }
    }

    // Sort topics by count descending, then name ascending for determinism.
    let mut topic_histogram: Vec<(String, usize)> = topics.into_iter().collect();
    topic_histogram.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let active_agents: Vec<String> = agents.into_iter().collect();

    let time_range = match (first_ts, last_ts) {
        (Some(f), Some(l)) => Some((f.to_owned(), l.to_owned())),
        _ => None,
    };

    SessionSummary {
        agent_count: active_agents.len(),
        message_count: msgs.len(),
        topic_histogram,
        active_agents,
        time_range,
        severity_counts: severities,
        open_threads: threads.into_iter().collect(),
    }
}

/// Produce a compact session summary by fetching messages from durable history.
///
/// Uses [`list_messages_history`] (Redis + `PostgreSQL` fallback) to retrieve
/// messages matching the given filters, then aggregates them into a
/// [`SessionSummary`].
///
/// # Errors
///
/// Returns an error if the underlying message fetch fails.
pub fn summarize_session(
    settings: &Settings,
    request: &SummarizeSessionRequest<'_>,
) -> Result<SessionSummary> {
    let empty_tags: Vec<String> = Vec::new();
    let msgs = list_messages_history(
        settings,
        &ReadMessagesRequest {
            agent: request.agent,
            from_agent: None,
            since_minutes: request.since_minutes,
            limit: request.limit,
            include_broadcast: true,
            filters: MessageFilters {
                repo: request.repo,
                session: request.session,
                tags: &empty_tags,
                thread_id: None,
                topic: None,
            },
        },
    )?;

    Ok(aggregate_messages(&msgs))
}

/// Produce a compact summary of a single agent's inbox.
///
/// Reads the agent's notification stream via [`check_inbox`] and aggregates the
/// resulting messages into a [`SessionSummary`]. The cursor is advanced as
/// usual (unless `reset_cursor` was set), so the caller sees only new messages
/// since the last check.
///
/// # Errors
///
/// Returns an error if the Redis connection or notification read fails.
pub fn summarize_inbox(
    conn: &mut redis::Connection,
    request: &SummarizeInboxRequest<'_>,
) -> Result<SummarizeInboxResult> {
    let empty_tags: Vec<String> = Vec::new();
    let inbox = check_inbox(
        conn,
        &CheckInboxRequest {
            agent: request.agent,
            limit: request.limit,
            reset_cursor: request.reset_cursor,
            filters: MessageFilters {
                repo: request.repo,
                session: request.session,
                tags: &empty_tags,
                thread_id: None,
                topic: None,
            },
        },
    )?;

    let summary = aggregate_messages(&inbox.messages);
    Ok(SummarizeInboxResult {
        summary,
        cursor_was: inbox.cursor_was,
        cursor_now: inbox.cursor_now,
    })
}

// ---------------------------------------------------------------------------
// Thread summaries / compaction
// ---------------------------------------------------------------------------

/// Parameters for [`summarize_thread`].
#[derive(Debug)]
pub struct SummarizeThreadRequest<'a> {
    /// Thread ID to summarize (required).
    pub thread_id: &'a str,
    /// How far back to look, in minutes.
    pub since_minutes: u64,
    /// Maximum number of messages to consider.
    pub limit: usize,
}

/// Produce a compact summary for a single `thread_id` by fetching messages
/// from durable history.
///
/// This is the thread-scoped analogue of [`summarize_session`]: it retrieves
/// messages that carry the specified `thread_id` tag and aggregates them via
/// [`aggregate_messages`].
///
/// # Errors
///
/// Returns an error if the underlying message fetch fails.
pub fn summarize_thread(
    settings: &Settings,
    request: &SummarizeThreadRequest<'_>,
) -> Result<SessionSummary> {
    let empty_tags: Vec<String> = Vec::new();
    let msgs = list_messages_history(
        settings,
        &ReadMessagesRequest {
            agent: None,
            from_agent: None,
            since_minutes: request.since_minutes,
            limit: request.limit,
            include_broadcast: true,
            filters: MessageFilters {
                repo: None,
                session: None,
                tags: &empty_tags,
                thread_id: Some(request.thread_id),
                topic: None,
            },
        },
    )?;

    Ok(aggregate_messages(&msgs))
}

/// Parameters for [`compact_thread`].
#[derive(Debug)]
pub struct CompactThreadRequest<'a> {
    /// Thread ID to compact (required).
    pub thread_id: &'a str,
    /// Maximum token budget for the compacted output.
    pub token_budget: usize,
    /// How far back to look, in minutes.
    pub since_minutes: u64,
    /// Maximum number of messages to fetch before compaction.
    pub limit: usize,
}

/// Fetch recent messages for a single `thread_id` from the live Redis stream
/// and compact them to fit within a token budget.
///
/// This is the thread-scoped analogue of [`compact_context`]: it fetches
/// messages filtered to the given `thread_id`, then applies token-budget
/// trimming via [`crate::token::compact_context`].
///
/// # Errors
///
/// Returns an error if the Redis connection or stream read fails.
pub fn compact_thread(
    conn: &mut redis::Connection,
    settings: &Settings,
    request: &CompactThreadRequest<'_>,
) -> Result<CompactContextResult> {
    let empty_tags: Vec<String> = Vec::new();
    let msgs = list_messages_live(
        conn,
        settings,
        &ReadMessagesRequest {
            agent: None,
            from_agent: None,
            since_minutes: request.since_minutes,
            limit: request.limit,
            include_broadcast: true,
            filters: MessageFilters {
                repo: None,
                session: None,
                tags: &empty_tags,
                thread_id: Some(request.thread_id),
                topic: None,
            },
        },
    )?;

    let original_len = msgs.len();
    let compacted = crate::token::compact_context(&msgs, request.token_budget);
    let token_count = compacted
        .iter()
        .map(|v| crate::token::estimate_tokens(&v.to_string()))
        .sum();
    let truncated = compacted.len() < original_len;

    Ok(CompactContextResult {
        messages: compacted,
        token_count,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// OrchestratorSummary
// ---------------------------------------------------------------------------

/// High-level diff of what changed since an agent last polled, optimised for
/// token-efficient LLM consumption.
///
/// Instead of replaying full message bodies the summary provides counts and
/// changed entity names so that an orchestrator can decide whether to fetch
/// the full history.
#[derive(Debug, Clone, Serialize)]
pub struct OrchestratorSummary {
    /// Total new messages since the cursor.
    pub new_messages: usize,
    /// Messages with topic matching `"findings"` or body containing `"FINDING:"`.
    pub new_findings: usize,
    /// Messages whose topic is `"ack"` or body contains `"ACK"`.
    pub new_acks: usize,
    /// Agent names that posted a `"presence"` or `"status"` topic message.
    pub presence_changes: Vec<String>,
    /// Resource names mentioned in `"ownership"` / `"claim"` / `"arbitration"` topics.
    pub claim_changes: Vec<String>,
    /// Messages with topic `"knock"`.
    pub new_knocks: usize,
    /// Cursor value before this read (`None` if first poll).
    pub since_cursor: Option<String>,
    /// Cursor value after this read — pass it as `since_cursor` next time.
    pub next_cursor: String,
}

/// Produce a token-efficient orchestrator summary of what changed since last
/// poll for a given agent.
///
/// Reads the agent's notification stream (same source as [`check_inbox`]),
/// categorises messages by topic, and returns counts plus changed entity names
/// rather than full message bodies.  The cursor is advanced so the next call
/// sees only new activity.
///
/// # Errors
///
/// Returns an error if the Redis connection or notification read fails.
pub fn orchestrator_summary(
    conn: &mut redis::Connection,
    agent: &str,
    cursor: Option<&str>,
) -> Result<OrchestratorSummary> {
    // If an explicit cursor was provided, use it; otherwise read the stored
    // cursor for this agent.
    let effective_cursor = match cursor {
        Some(c) if !c.is_empty() => c.to_owned(),
        _ => crate::redis_bus::get_notification_cursor(conn, agent)?,
    };

    let notifications = crate::redis_bus::list_notifications_since_id(
        conn,
        agent,
        &effective_cursor,
        10_000, // generous upper bound — we only count, not return bodies
    )?;

    let summary = categorise_notifications(&notifications, &effective_cursor);

    // Advance the stored cursor so the next poll starts from where we left off.
    if summary.next_cursor != effective_cursor {
        crate::redis_bus::set_notification_cursor(conn, agent, &summary.next_cursor)?;
    }

    Ok(summary)
}

/// Pure categorisation kernel — no I/O, fully testable.
#[must_use]
fn categorise_notifications(
    notifications: &[crate::redis_bus::Notification],
    since_cursor: &str,
) -> OrchestratorSummary {
    let mut new_findings = 0usize;
    let mut new_acks = 0usize;
    let mut new_knocks = 0usize;
    let mut presence_agents: BTreeSet<String> = BTreeSet::new();
    let mut claim_resources: BTreeSet<String> = BTreeSet::new();
    let mut last_stream_id: Option<&str> = None;

    for notification in notifications {
        let msg = &notification.message;
        let topic_lower = msg.topic.to_lowercase();

        // Classify by topic.
        if topic_lower == "findings"
            || topic_lower.contains("finding")
            || msg.body.contains("FINDING:")
        {
            new_findings += 1;
        }

        if topic_lower == "ack" || msg.body.contains("ACK") {
            new_acks += 1;
        }

        if topic_lower == "presence" || topic_lower == "status" {
            presence_agents.insert(msg.from.clone());
        }

        if topic_lower == "ownership"
            || topic_lower == "claim"
            || topic_lower == "arbitration"
            || topic_lower.starts_with("arbitration-")
        {
            // Try to extract a resource name from the body or use the topic.
            // Convention: ownership messages often mention the resource in the
            // first line or as a tag.
            let resource_name = extract_resource_from_message(msg);
            claim_resources.insert(resource_name);
        }

        if topic_lower == "knock" {
            new_knocks += 1;
        }

        if let Some(sid) = notification.notification_stream_id.as_deref() {
            last_stream_id = Some(sid);
        }
    }

    let next_cursor = last_stream_id.unwrap_or(since_cursor).to_owned();

    OrchestratorSummary {
        new_messages: notifications.len(),
        new_findings,
        new_acks,
        presence_changes: presence_agents.into_iter().collect(),
        claim_changes: claim_resources.into_iter().collect(),
        new_knocks,
        since_cursor: Some(since_cursor.to_owned()),
        next_cursor,
    }
}

/// Best-effort extraction of a resource name from a claim/ownership message.
///
/// Looks for `arbitration:<resource>` tags first, then falls back to the
/// first non-empty line of the body.
fn extract_resource_from_message(msg: &Message) -> String {
    // Check tags for arbitration:* pattern.
    for tag in &msg.tags {
        if let Some(resource) = tag.strip_prefix("arbitration:")
            && !resource.is_empty()
        {
            return resource.to_owned();
        }
    }
    // Fallback: first non-empty body line (often "CLAIM CONTESTED: <resource>").
    for line in msg.body.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            // Strip common prefixes.
            if let Some(rest) = trimmed.strip_prefix("CLAIM CONTESTED:") {
                return rest.trim().to_owned();
            }
            return trimmed.to_owned();
        }
    }
    msg.topic.clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    struct ScriptedRedis(std::collections::VecDeque<(Vec<u8>, redis::Value)>);

    impl redis::ConnectionLike for ScriptedRedis {
        fn req_packed_command(&mut self, command: &[u8]) -> redis::RedisResult<redis::Value> {
            let (expected, response) = self.0.pop_front().expect("unexpected Redis command");
            assert_eq!(command, expected);
            Ok(response)
        }

        fn req_packed_commands(
            &mut self,
            _: &[u8],
            _: usize,
            _: usize,
        ) -> redis::RedisResult<Vec<redis::Value>> {
            panic!("inbox must not require a pipeline");
        }

        fn get_db(&self) -> i64 {
            0
        }
        fn check_connection(&mut self) -> bool {
            true
        }
        fn is_open(&self) -> bool {
            true
        }
    }

    #[test]
    fn inbox_redis_adapter_preserves_cursor_ttl_contract_and_decodes_entries() {
        for filtered in [false, true] {
            let request = inbox_request(filtered.then_some("a"), 1);
            let key = super::inbox_cursor_key(&request, &[]).unwrap();
            let message = inbox_entries(["a"]).entries.remove(0).message;
            let entry = |id: &str, payload: Vec<u8>| {
                redis::Value::Array(vec![
                    redis::Value::BulkString(id.as_bytes().to_vec()),
                    redis::Value::Array(vec![
                        redis::Value::BulkString(b"message".to_vec()),
                        redis::Value::BulkString(payload),
                    ]),
                ])
            };
            let mut set = redis::cmd("SET");
            set.arg(&key).arg("2-0");
            if filtered {
                set.arg("EX").arg(604_800);
            }
            let mut backend = ScriptedRedis(std::collections::VecDeque::from([
                (
                    redis::cmd("GET").arg(&key).get_packed_command(),
                    redis::Value::Nil,
                ),
                (
                    redis::cmd("XRANGE")
                        .arg("agent_bus:notify:codex")
                        .arg("(0-0")
                        .arg("+")
                        .arg("COUNT")
                        .arg(super::INBOX_PAGE_SIZE)
                        .get_packed_command(),
                    redis::Value::Array(vec![
                        entry("1-0", b"invalid JSON".to_vec()),
                        entry("2-0", serde_json::to_vec(&message).unwrap()),
                    ]),
                ),
                (set.get_packed_command(), redis::Value::Okay),
            ]));
            let result = super::check_inbox_with_store(&mut backend, &request).unwrap();
            assert_eq!(result.messages.len(), 1);
            assert_eq!(result.messages[0].id, message.id);
            assert_eq!(result.cursor_now, "2-0");
            assert!(backend.0.is_empty());
        }
    }

    #[test]
    fn inbox_redis_reset_is_atomic_with_expiry_and_zero_limit_never_reads_stream() {
        let mut request = inbox_request(Some("a"), 0);
        request.reset_cursor = true;
        let key = super::inbox_cursor_key(&request, &[]).unwrap();
        let mut backend = ScriptedRedis(std::collections::VecDeque::from([
            (
                redis::cmd("SET")
                    .arg(&key)
                    .arg("0-0")
                    .arg("EX")
                    .arg(604_800)
                    .get_packed_command(),
                redis::Value::Okay,
            ),
            (
                redis::cmd("GET").arg(&key).get_packed_command(),
                redis::Value::BulkString(b"0-0".to_vec()),
            ),
        ]));
        assert_eq!(
            super::check_inbox_with_store(&mut backend, &request)
                .unwrap()
                .cursor_now,
            "0-0"
        );
        assert!(backend.0.is_empty());
    }

    #[derive(Default)]
    struct MemoryInbox {
        cursors: std::collections::HashMap<String, String>,
        entries: Vec<crate::redis_bus::Notification>,
        reads: usize,
        fail_on_read: Option<usize>,
    }

    impl super::InboxStore for MemoryInbox {
        fn cursor(&mut self, key: &str) -> crate::error::Result<String> {
            Ok(self
                .cursors
                .get(key)
                .cloned()
                .unwrap_or_else(|| "0-0".into()))
        }

        fn save_cursor(&mut self, key: &str, value: &str) -> crate::error::Result<()> {
            self.cursors.insert(key.into(), value.into());
            Ok(())
        }

        fn page(
            &mut self,
            _: &str,
            cursor: &str,
            limit: usize,
        ) -> crate::error::Result<Vec<crate::redis_bus::Notification>> {
            self.reads += 1;
            if self.fail_on_read == Some(self.reads) {
                return Err(crate::error::AgentBusError::Internal(
                    "injected page failure".into(),
                ));
            }
            let position = cursor.split('-').next().unwrap().parse::<usize>().unwrap();
            Ok(self
                .entries
                .iter()
                .skip(position)
                .take(limit)
                .cloned()
                .collect())
        }
    }

    fn inbox_request(thread: Option<&str>, limit: usize) -> super::CheckInboxRequest<'_> {
        super::CheckInboxRequest {
            agent: "codex",
            limit,
            reset_cursor: false,
            filters: super::MessageFilters {
                repo: None,
                session: None,
                tags: &[],
                thread_id: thread,
                topic: None,
            },
        }
    }

    fn inbox_entries(threads: impl IntoIterator<Item = &'static str>) -> MemoryInbox {
        MemoryInbox {
            entries: threads
                .into_iter()
                .enumerate()
                .map(|(index, thread)| {
                    let mut message = make_message(
                        "claude",
                        "status",
                        "fixture",
                        "2026-09-27T00:00:00Z",
                        Some(thread),
                    );
                    message.to = "codex".into();
                    message.tags =
                        smallvec::smallvec!["repo:study".into(), format!("session:{thread}")];
                    make_notification(message, Some(&format!("{}-0", index + 1)))
                })
                .collect(),
            ..MemoryInbox::default()
        }
    }

    #[test]
    fn inbox_matches_beyond_unmatched_pages_without_repeating_or_skipping() {
        let mut store =
            inbox_entries(std::iter::repeat_n("old", 600).chain(["current", "current"]));
        let request = inbox_request(Some("current"), 1);
        let first = super::check_inbox_with_store(&mut store, &request).unwrap();
        assert_eq!(first.messages.len(), 1);
        assert_eq!(first.cursor_now, "601-0");
        assert_eq!(store.reads, 3);
        let second = super::check_inbox_with_store(&mut store, &request).unwrap();
        assert_eq!(second.cursor_now, "602-0");
        assert_eq!(second.messages.len(), 1);
        assert!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn inbox_bounded_empty_scan_makes_progress() {
        let mut store =
            inbox_entries(std::iter::repeat_n("old", super::INBOX_SCAN_LIMIT).chain(["current"]));
        let request = inbox_request(Some("current"), 1);
        let first = super::check_inbox_with_store(&mut store, &request).unwrap();
        assert!(first.messages.is_empty());
        assert_eq!(first.cursor_now, format!("{}-0", super::INBOX_SCAN_LIMIT));
        assert_eq!(
            store.reads,
            super::INBOX_SCAN_LIMIT / super::INBOX_PAGE_SIZE
        );
        assert_eq!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn inbox_filtered_global_and_reset_positions_are_independent() {
        let mut store = inbox_entries(["a", "b", "a"]);
        let mut a = inbox_request(Some("a"), 1);
        let first = super::check_inbox_with_store(&mut store, &a).unwrap();
        assert_eq!(first.cursor_now, "1-0");
        let b = super::check_inbox_with_store(&mut store, &inbox_request(Some("b"), 1)).unwrap();
        assert_eq!(b.cursor_now, "2-0");
        let global = super::check_inbox_with_store(&mut store, &inbox_request(None, 1)).unwrap();
        assert_eq!(global.inbox_cursor_key, "bus:notify_cursor:codex");
        assert_eq!(global.cursor_now, "1-0");
        assert_eq!(
            super::check_inbox_with_store(&mut store, &a)
                .unwrap()
                .cursor_now,
            "3-0"
        );
        a.reset_cursor = true;
        assert_eq!(
            super::check_inbox_with_store(&mut store, &a)
                .unwrap()
                .cursor_now,
            "1-0"
        );
        assert_eq!(store.cursors[&b.inbox_cursor_key], "2-0");
        assert_eq!(store.cursors[&global.inbox_cursor_key], "1-0");
    }

    #[test]
    fn inbox_normalized_tags_and_scope_conjunction_use_same_lane() {
        let mut store = inbox_entries(["a", "b"]);
        let mut scoped = inbox_request(None, 1);
        scoped.filters.repo = Some("study");
        scoped.filters.session = Some("b");
        let first = super::check_inbox_with_store(&mut store, &scoped).unwrap();
        assert_eq!(first.cursor_now, "2-0");
        assert_eq!(first.messages[0].thread_id.as_deref(), Some("b"));
        let tags = vec![
            "session:b".into(),
            String::new(),
            "repo:study".into(),
            "session:b".into(),
        ];
        let mut equivalent = inbox_request(None, 1);
        equivalent.filters.tags = &tags;
        let second = super::check_inbox_with_store(&mut store, &equivalent).unwrap();
        assert_eq!(first.inbox_cursor_key, second.inbox_cursor_key);
        assert!(second.messages.is_empty());
        scoped.filters.session = Some("a");
        let other = super::check_inbox_with_store(&mut store, &scoped).unwrap();
        assert_ne!(first.inbox_cursor_key, other.inbox_cursor_key);
        assert_eq!(other.cursor_now, "1-0");
        scoped.filters.session = None;
        assert_eq!(
            super::check_inbox_with_store(&mut store, &scoped)
                .unwrap()
                .cursor_now,
            "1-0"
        );
    }

    #[test]
    fn inbox_topic_is_filtered_and_isolated() {
        let mut store = inbox_entries(["a", "a"]);
        store.entries[1].message.topic = "findings".into();
        let mut request = inbox_request(None, 1);
        request.filters.topic = Some("findings");
        assert_eq!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .cursor_now,
            "2-0"
        );
        request.filters.topic = Some("status");
        assert_eq!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .cursor_now,
            "1-0"
        );
    }

    #[test]
    fn inbox_zero_limit_does_not_scan_and_reset_affects_only_selected_lane() {
        let mut store = inbox_entries(["a"]);
        let mut request = inbox_request(Some("a"), 1);
        let first = super::check_inbox_with_store(&mut store, &request).unwrap();
        request.limit = 0;
        let reads = store.reads;
        assert_eq!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .cursor_now,
            "1-0"
        );
        request.reset_cursor = true;
        assert_eq!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .cursor_now,
            "0-0"
        );
        assert_eq!(store.cursors[&first.inbox_cursor_key], "0-0");
        assert_eq!(store.reads, reads);
    }

    #[test]
    fn inbox_page_failure_does_not_consume_unreturned_messages() {
        let mut store = inbox_entries(std::iter::repeat_n("a", 300));
        store.fail_on_read = Some(2);
        assert!(super::check_inbox_with_store(&mut store, &inbox_request(Some("a"), 300)).is_err());
        assert!(store.cursors.is_empty());
    }

    #[test]
    fn inbox_legacy_filtered_positions_are_not_inherited_or_mutated() {
        let mut store = inbox_entries(["a", "b"]);
        store
            .cursors
            .insert("bus:notify_cursor:codex".into(), "2-0".into());
        store
            .cursors
            .insert("bus:notify_cursor:codex:repo:study".into(), "2-0".into());
        let mut request = inbox_request(None, 1);
        request.filters.repo = Some("study");
        let result = super::check_inbox_with_store(&mut store, &request).unwrap();
        assert_eq!(result.cursor_was, "0-0");
        assert_eq!(result.cursor_now, "1-0");
        assert_eq!(store.cursors["bus:notify_cursor:codex"], "2-0");
        assert_eq!(store.cursors["bus:notify_cursor:codex:repo:study"], "2-0");
    }

    #[test]
    fn inbox_skips_malformed_snapshot_and_advances() {
        let mut store = inbox_entries(["a", "a"]);
        // The Redis decoder represents invalid JSON with an empty message.
        store.entries[0].message.to.clear();
        store.entries[0].message.id.clear();
        let result = super::check_inbox_with_store(&mut store, &inbox_request(None, 1)).unwrap();
        assert_eq!(result.cursor_now, "2-0");
        assert_eq!(result.messages.len(), 1);
        assert!(!result.messages[0].id.is_empty());
    }

    #[test]
    fn inbox_empty_and_delimited_filter_components_remain_unambiguous() {
        let mut store = inbox_entries([]);
        let mut request = inbox_request(None, 1);
        let global = super::check_inbox_with_store(&mut store, &request).unwrap();
        request.filters.repo = Some("");
        request.filters.session = Some("");
        assert_eq!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .inbox_cursor_key,
            global.inbox_cursor_key
        );
        request.filters.thread_id = Some("");
        assert_ne!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .inbox_cursor_key,
            global.inbox_cursor_key
        );
        request.filters.thread_id = Some("a:b");
        request.filters.topic = Some("c");
        let first = super::check_inbox_with_store(&mut store, &request).unwrap();
        request.filters.thread_id = Some("a");
        request.filters.topic = Some("b:c");
        assert_ne!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .inbox_cursor_key,
            first.inbox_cursor_key
        );
        request.filters.thread_id = None;
        request.filters.topic = None;
        request.filters.repo = Some(" ");
        assert_ne!(
            super::check_inbox_with_store(&mut store, &request)
                .unwrap()
                .inbox_cursor_key,
            global.inbox_cursor_key
        );
    }
    use smallvec::smallvec;

    use super::*;

    fn make_message(
        from: &str,
        topic: &str,
        body: &str,
        timestamp: &str,
        thread_id: Option<&str>,
    ) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_utc: timestamp.to_owned(),
            protocol_version: "0.5".to_owned(),
            from: from.to_owned(),
            to: "broadcast".to_owned(),
            topic: topic.to_owned(),
            body: body.to_owned(),
            thread_id: thread_id.map(ToOwned::to_owned),
            tags: smallvec![],
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
            client_msg_id: None,
            origin_hub: None,
            origin_seq: None,
            hlc: None,
            sensitivity: None,
        }
    }

    #[test]
    fn aggregate_empty_messages() {
        let summary = aggregate_messages(&[]);
        assert_eq!(summary.agent_count, 0);
        assert_eq!(summary.message_count, 0);
        assert!(summary.topic_histogram.is_empty());
        assert!(summary.active_agents.is_empty());
        assert!(summary.time_range.is_none());
        assert!(summary.severity_counts.is_empty());
        assert!(summary.open_threads.is_empty());
    }

    #[test]
    fn aggregate_counts_agents_and_topics() {
        let msgs = vec![
            make_message("alice", "status", "all good", "2026-04-01T10:00:00Z", None),
            make_message("bob", "status", "done", "2026-04-01T10:01:00Z", None),
            make_message(
                "alice",
                "findings",
                "found bug",
                "2026-04-01T10:02:00Z",
                None,
            ),
        ];
        let summary = aggregate_messages(&msgs);

        assert_eq!(summary.agent_count, 2);
        assert_eq!(summary.message_count, 3);
        assert_eq!(summary.active_agents, vec!["alice", "bob"]);

        // status=2, findings=1 -> status first
        assert_eq!(summary.topic_histogram[0], ("status".to_owned(), 2));
        assert_eq!(summary.topic_histogram[1], ("findings".to_owned(), 1));
    }

    #[test]
    fn aggregate_time_range() {
        let msgs = vec![
            make_message("a", "t", "", "2026-04-01T12:00:00Z", None),
            make_message("a", "t", "", "2026-04-01T08:00:00Z", None),
            make_message("a", "t", "", "2026-04-01T15:30:00Z", None),
        ];
        let summary = aggregate_messages(&msgs);

        let (earliest, latest) = summary.time_range.expect("should have time_range");
        assert_eq!(earliest, "2026-04-01T08:00:00Z");
        assert_eq!(latest, "2026-04-01T15:30:00Z");
    }

    #[test]
    fn aggregate_severity_counts() {
        let msgs = vec![
            make_message(
                "a",
                "findings",
                "FINDING: something\nSEVERITY: HIGH",
                "2026-04-01T10:00:00Z",
                None,
            ),
            make_message(
                "b",
                "findings",
                "FINDING: else\nSEVERITY: CRITICAL",
                "2026-04-01T10:01:00Z",
                None,
            ),
            make_message(
                "a",
                "findings",
                "FINDING: minor\nSEVERITY: high",
                "2026-04-01T10:02:00Z",
                None,
            ),
            make_message(
                "a",
                "findings",
                "no severity here",
                "2026-04-01T10:03:00Z",
                None,
            ),
        ];
        let summary = aggregate_messages(&msgs);

        assert_eq!(summary.severity_counts.get("HIGH"), Some(&2));
        assert_eq!(summary.severity_counts.get("CRITICAL"), Some(&1));
        assert_eq!(summary.severity_counts.get("LOW"), None);
        assert_eq!(summary.severity_counts.get("MEDIUM"), None);
    }

    #[test]
    fn aggregate_threads() {
        let msgs = vec![
            make_message("a", "t", "", "2026-04-01T10:00:00Z", Some("thread-1")),
            make_message("b", "t", "", "2026-04-01T10:01:00Z", Some("thread-2")),
            make_message("a", "t", "", "2026-04-01T10:02:00Z", Some("thread-1")),
            make_message("c", "t", "", "2026-04-01T10:03:00Z", None),
        ];
        let summary = aggregate_messages(&msgs);

        assert_eq!(summary.open_threads, vec!["thread-1", "thread-2"]);
    }

    #[test]
    fn aggregate_deterministic_topic_sort() {
        // When counts are equal, topics sort alphabetically.
        let msgs = vec![
            make_message("a", "beta", "", "2026-04-01T10:00:00Z", None),
            make_message("a", "alpha", "", "2026-04-01T10:01:00Z", None),
            make_message("a", "gamma", "", "2026-04-01T10:02:00Z", None),
        ];
        let summary = aggregate_messages(&msgs);

        let topic_names: Vec<&str> = summary
            .topic_histogram
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(topic_names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn summary_serializes_to_json() {
        let msgs = vec![make_message(
            "agent-1",
            "status",
            "SEVERITY: LOW",
            "2026-04-01T10:00:00Z",
            Some("t-1"),
        )];
        let summary = aggregate_messages(&msgs);

        let json = serde_json::to_value(&summary).expect("must serialise");
        assert_eq!(json["agent_count"], 1);
        assert_eq!(json["message_count"], 1);
        assert!(json["topic_histogram"].is_array());
        assert!(json["time_range"].is_array());
        assert_eq!(json["severity_counts"]["LOW"], 1);
        assert_eq!(json["open_threads"][0], "t-1");
    }

    // -----------------------------------------------------------------------
    // Thread-filtered aggregation
    // -----------------------------------------------------------------------

    #[test]
    fn aggregate_filters_by_thread() {
        let msgs = vec![
            make_message("a", "status", "ok", "2026-04-01T10:00:00Z", Some("t-1")),
            make_message("b", "status", "ok", "2026-04-01T10:01:00Z", Some("t-2")),
            make_message("c", "review", "done", "2026-04-01T10:02:00Z", Some("t-1")),
            make_message("d", "status", "ok", "2026-04-01T10:03:00Z", None),
        ];

        // Filter to only thread "t-1" messages before aggregating.
        let filtered: Vec<_> = msgs
            .into_iter()
            .filter(|m| m.thread_id.as_deref() == Some("t-1"))
            .collect();
        let summary = aggregate_messages(&filtered);

        assert_eq!(summary.message_count, 2);
        assert_eq!(summary.agent_count, 2);
        assert_eq!(summary.active_agents, vec!["a", "c"]);
        assert_eq!(summary.open_threads, vec!["t-1"]);
    }

    // -----------------------------------------------------------------------
    // Scoped cursor key tests
    // -----------------------------------------------------------------------

    #[test]
    fn scoped_cursor_key_repo() {
        let key = crate::redis_bus::scoped_notification_cursor_key("claude", Some("myrepo"), None);
        assert_eq!(key, "bus:notify_cursor:claude:repo:myrepo");
    }

    #[test]
    fn scoped_cursor_key_session() {
        let key =
            crate::redis_bus::scoped_notification_cursor_key("claude", None, Some("sprint-42"));
        assert_eq!(key, "bus:notify_cursor:claude:session:sprint-42");
    }

    #[test]
    fn scoped_cursor_key_repo_wins_over_session() {
        // When both repo and session are provided, repo takes precedence.
        let key = crate::redis_bus::scoped_notification_cursor_key(
            "claude",
            Some("myrepo"),
            Some("sprint-42"),
        );
        assert_eq!(key, "bus:notify_cursor:claude:repo:myrepo");
    }

    #[test]
    fn scoped_cursor_key_none_falls_back_to_global() {
        let key = crate::redis_bus::scoped_notification_cursor_key("claude", None, None);
        assert_eq!(key, "bus:notify_cursor:claude");
    }

    // -----------------------------------------------------------------------
    // OrchestratorSummary / categorise_notifications tests
    // -----------------------------------------------------------------------

    fn make_notification(msg: Message, stream_id: Option<&str>) -> crate::redis_bus::Notification {
        crate::redis_bus::Notification {
            id: msg.id.clone(),
            agent: msg.to.clone(),
            created_at: msg.timestamp_utc.clone(),
            reason: "direct_message".to_owned(),
            requires_ack: false,
            message: msg,
            notification_stream_id: stream_id.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn categorise_empty_notifications() {
        let summary = super::categorise_notifications(&[], "0-0");
        assert_eq!(summary.new_messages, 0);
        assert_eq!(summary.new_findings, 0);
        assert_eq!(summary.new_acks, 0);
        assert_eq!(summary.new_knocks, 0);
        assert!(summary.presence_changes.is_empty());
        assert!(summary.claim_changes.is_empty());
        assert_eq!(summary.since_cursor, Some("0-0".to_owned()));
        assert_eq!(summary.next_cursor, "0-0");
    }

    #[test]
    fn categorise_findings_by_topic() {
        let msg = make_message(
            "scanner",
            "findings",
            "FINDING: bug\nSEVERITY: HIGH",
            "2026-04-01T10:00:00Z",
            None,
        );
        let notifications = vec![make_notification(msg, Some("100-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.new_messages, 1);
        assert_eq!(summary.new_findings, 1);
        assert_eq!(summary.next_cursor, "100-0");
    }

    #[test]
    fn categorise_findings_by_body_marker() {
        let msg = make_message(
            "scanner",
            "report",
            "FINDING: something bad",
            "2026-04-01T10:00:00Z",
            None,
        );
        let notifications = vec![make_notification(msg, Some("101-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.new_findings, 1);
    }

    #[test]
    fn categorise_ack_messages() {
        let msg = make_message(
            "agent-a",
            "ack",
            "ACK for msg-123",
            "2026-04-01T10:00:00Z",
            None,
        );
        let notifications = vec![make_notification(msg, Some("102-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.new_acks, 1);
    }

    #[test]
    fn categorise_presence_changes() {
        let m1 = make_message("alice", "presence", "online", "2026-04-01T10:00:00Z", None);
        let m2 = make_message("bob", "status", "busy", "2026-04-01T10:01:00Z", None);
        let m3 = make_message("alice", "presence", "idle", "2026-04-01T10:02:00Z", None);
        let notifications = vec![
            make_notification(m1, Some("200-0")),
            make_notification(m2, Some("201-0")),
            make_notification(m3, Some("202-0")),
        ];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.presence_changes, vec!["alice", "bob"]);
    }

    #[test]
    fn categorise_claim_changes_from_tags() {
        let mut msg = make_message(
            "orchestrator",
            "ownership",
            "claim info",
            "2026-04-01T10:00:00Z",
            None,
        );
        msg.tags = smallvec!["arbitration:src/http.rs".to_owned()];
        let notifications = vec![make_notification(msg, Some("300-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.claim_changes, vec!["src/http.rs"]);
    }

    #[test]
    fn categorise_claim_changes_contested_body() {
        let msg = make_message(
            "system",
            "ownership",
            "CLAIM CONTESTED: src/lib.rs",
            "2026-04-01T10:00:00Z",
            None,
        );
        let notifications = vec![make_notification(msg, Some("301-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.claim_changes, vec!["src/lib.rs"]);
    }

    #[test]
    fn categorise_knocks() {
        let msg = make_message(
            "peer",
            "knock",
            "are you there?",
            "2026-04-01T10:00:00Z",
            None,
        );
        let notifications = vec![make_notification(msg, Some("400-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        assert_eq!(summary.new_knocks, 1);
    }

    #[test]
    fn categorise_mixed_messages() {
        let m1 = make_message(
            "scanner",
            "findings",
            "FINDING: x\nSEVERITY: LOW",
            "2026-04-01T10:00:00Z",
            None,
        );
        let m2 = make_message("agent-a", "ack", "ACK", "2026-04-01T10:01:00Z", None);
        let m3 = make_message("bob", "status", "idle", "2026-04-01T10:02:00Z", None);
        let m4 = make_message("peer", "knock", "ping", "2026-04-01T10:03:00Z", None);
        let m5 = make_message(
            "scanner",
            "findings",
            "FINDING: y\nSEVERITY: HIGH",
            "2026-04-01T10:04:00Z",
            None,
        );
        let notifications = vec![
            make_notification(m1, Some("500-0")),
            make_notification(m2, Some("501-0")),
            make_notification(m3, Some("502-0")),
            make_notification(m4, Some("503-0")),
            make_notification(m5, Some("504-0")),
        ];
        let summary = super::categorise_notifications(&notifications, "100-0");
        assert_eq!(summary.new_messages, 5);
        assert_eq!(summary.new_findings, 2);
        assert_eq!(summary.new_acks, 1);
        assert_eq!(summary.new_knocks, 1);
        assert_eq!(summary.presence_changes, vec!["bob"]);
        assert_eq!(summary.since_cursor, Some("100-0".to_owned()));
        assert_eq!(summary.next_cursor, "504-0");
    }

    #[test]
    fn categorise_cursor_stays_when_no_notifications() {
        let summary = super::categorise_notifications(&[], "99-0");
        assert_eq!(summary.since_cursor, Some("99-0".to_owned()));
        assert_eq!(summary.next_cursor, "99-0");
    }

    #[test]
    fn orchestrator_summary_serializes_to_json() {
        let msg = make_message(
            "scanner",
            "findings",
            "FINDING: a\nSEVERITY: HIGH",
            "2026-04-01T10:00:00Z",
            None,
        );
        let notifications = vec![make_notification(msg, Some("600-0"))];
        let summary = super::categorise_notifications(&notifications, "0-0");
        let json = serde_json::to_value(&summary).expect("must serialise");
        assert_eq!(json["new_messages"], 1);
        assert_eq!(json["new_findings"], 1);
        assert_eq!(json["new_acks"], 0);
        assert_eq!(json["new_knocks"], 0);
        assert!(json["presence_changes"].is_array());
        assert!(json["claim_changes"].is_array());
        assert_eq!(json["next_cursor"], "600-0");
    }
}
