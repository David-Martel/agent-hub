//! Inbox and context compaction operations.

use anyhow::Result;

use crate::models::Message;
use crate::redis_bus::{
    get_notification_cursor, list_notifications_since_id, notification_cursor_key,
    set_notification_cursor,
};
use crate::settings::Settings;

use super::MessageFilters;
use super::message::ReadMessagesRequest;
use super::message::list_messages_live;

// ---------------------------------------------------------------------------
// CheckInbox
// ---------------------------------------------------------------------------

pub(crate) struct CheckInboxRequest<'a> {
    pub(crate) agent: &'a str,
    pub(crate) limit: usize,
    pub(crate) reset_cursor: bool,
    pub(crate) filters: MessageFilters<'a>,
}

pub(crate) struct CheckInboxResult {
    pub(crate) messages: Vec<Message>,
    pub(crate) cursor_was: String,
    pub(crate) cursor_now: String,
    pub(crate) inbox_cursor_key: String,
}

/// Check an agent's inbox using cursor-based notification delivery.
///
/// Reads notifications from the per-agent attention stream since the last
/// stored cursor, applies tag/thread filters, advances the cursor to the last
/// delivered entry, and returns the matched messages with cursor metadata.
///
/// If `reset_cursor` is set the cursor is zeroed before reading, which
/// re-delivers all messages from the beginning of the stream.
///
/// # Errors
///
/// Returns an error if the Redis connection or stream commands fail.
pub(crate) fn check_inbox(
    conn: &mut redis::Connection,
    request: &CheckInboxRequest<'_>,
) -> Result<CheckInboxResult> {
    let required_tags = super::scoped_required_tags(&request.filters);
    let required_tag_refs: Vec<&str> = required_tags.iter().map(String::as_str).collect();

    // Honour reset_cursor before reading: write "0-0" so the subsequent read
    // starts from the beginning of the stream.
    if request.reset_cursor {
        set_notification_cursor(conn, request.agent, "0-0")?;
    }

    let cursor = get_notification_cursor(conn, request.agent)?;
    let notifications =
        list_notifications_since_id(conn, request.agent, &cursor, request.limit)?;

    let thread_id = request.filters.thread_id;
    let agent = request.agent;
    let filtered_notifications: Vec<_> = notifications
        .into_iter()
        .filter(|notification| {
            crate::redis_bus::message_matches_filters(
                &notification.message,
                Some(agent),
                None,
                true,
                thread_id,
                &required_tag_refs,
            )
        })
        .take(request.limit)
        .collect();

    let messages: Vec<Message> = filtered_notifications
        .iter()
        .map(|notification| notification.message.clone())
        .collect();

    let cursor_now = filtered_notifications
        .last()
        .and_then(|notification| notification.notification_stream_id.as_deref())
        .unwrap_or(&cursor)
        .to_owned();

    // Advance cursor to the last delivered notification entry so replay
    // remains aligned with the per-agent attention stream.
    if cursor_now != cursor {
        set_notification_cursor(conn, request.agent, &cursor_now)?;
    }

    Ok(CheckInboxResult {
        messages,
        cursor_was: cursor,
        cursor_now,
        inbox_cursor_key: notification_cursor_key(request.agent),
    })
}

// ---------------------------------------------------------------------------
// CompactContext
// ---------------------------------------------------------------------------

pub(crate) struct CompactContextRequest<'a> {
    pub(crate) agent: Option<&'a str>,
    pub(crate) filters: MessageFilters<'a>,
    pub(crate) since_minutes: u64,
    pub(crate) max_tokens: usize,
}

pub(crate) struct CompactContextResult {
    pub(crate) messages: Vec<serde_json::Value>,
    /// Estimated token count of the compacted output.
    ///
    /// Not yet consumed by `cmd_compact_context` but available for callers
    /// that need budget tracking (e.g. the HTTP handler).
    #[expect(dead_code, reason = "reserved for future callers that expose token metadata")]
    pub(crate) token_count: usize,
    /// True when the message list was trimmed to meet the token budget.
    ///
    /// Not yet surfaced by `cmd_compact_context` but available for callers
    /// that want to signal truncation to the consumer.
    #[expect(dead_code, reason = "reserved for future callers that expose truncation metadata")]
    pub(crate) truncated: bool,
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
pub(crate) fn compact_context(
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
