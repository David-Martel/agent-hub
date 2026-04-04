//! Shared bus operations reused across CLI, HTTP, and MCP transports.

pub mod admin;
pub mod channel;
pub mod claim;
pub mod inbox;
pub mod inventory;
pub mod message;
pub mod subscription;
pub mod task;

use crate::models::{Message, XREVRANGE_MIN_FETCH, XREVRANGE_OVERFETCH_FACTOR};
use crate::postgres_store::query_scope_tags;

// Channel and claim items are imported directly from their submodules by
// callers that need them.  Re-export only what multiple transport modules
// consume from the `crate::ops` namespace to avoid dead-code noise during
// the incremental migration (Task 2 → Task 4).
pub use message::{
    AckMessageRequest, PostMessageRequest, PresenceRequest, ReadMessagesRequest,
    ValidatedBatchItem, ValidatedSendRequest, knock_metadata, list_messages_history,
    list_messages_live, post_ack, post_message, set_presence, validated_batch_send,
    validated_post_message,
};

#[derive(Debug)]
pub struct MessageFilters<'a> {
    pub repo: Option<&'a str>,
    pub session: Option<&'a str>,
    pub tags: &'a [String],
    pub thread_id: Option<&'a str>,
}

impl MessageFilters<'_> {
    /// Returns `true` if no filter fields are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repo.is_none()
            && self.session.is_none()
            && self.tags.is_empty()
            && self.thread_id.is_none()
    }
}

/// Return `true` if `msg` passes all active filter criteria.
#[must_use]
pub fn message_matches_filters(msg: &Message, filters: &MessageFilters<'_>) -> bool {
    if let Some(repo) = filters.repo {
        let expected = format!("repo:{repo}");
        if !msg.tags.iter().any(|tag| tag == &expected) {
            return false;
        }
    }

    if let Some(session) = filters.session {
        let expected = format!("session:{session}");
        if !msg.tags.iter().any(|tag| tag == &expected) {
            return false;
        }
    }

    if !filters
        .tags
        .iter()
        .all(|tag| msg.tags.iter().any(|msg_tag| msg_tag == tag))
    {
        return false;
    }

    if filters
        .thread_id
        .is_some_and(|thread_id| msg.thread_id.as_deref() != Some(thread_id))
    {
        return false;
    }

    true
}

/// Compute the overfetch limit to use when tag or thread filters are active.
#[must_use]
pub fn extra_filter_fetch_limit(limit: usize, filters: &MessageFilters<'_>) -> usize {
    if filters.is_empty() {
        limit
    } else {
        (limit * XREVRANGE_OVERFETCH_FACTOR).max(XREVRANGE_MIN_FETCH)
    }
}

pub fn scoped_required_tags(filters: &MessageFilters<'_>) -> Vec<String> {
    let tags: Vec<&str> = filters.tags.iter().map(String::as_str).collect();
    query_scope_tags(filters.repo, filters.session, &tags)
}
