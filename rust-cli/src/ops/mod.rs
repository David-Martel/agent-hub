//! Shared bus operations reused across CLI, HTTP, and MCP transports.

pub(crate) mod admin;
pub(crate) mod channel;
pub(crate) mod claim;
pub(crate) mod inbox;
pub(crate) mod message;
pub(crate) mod task;

#[cfg(test)]
use crate::models::{Message, XREVRANGE_MIN_FETCH, XREVRANGE_OVERFETCH_FACTOR};
use crate::postgres_store::query_scope_tags;

// Channel and claim items are imported directly from their submodules by
// callers that need them.  Re-export only what multiple transport modules
// consume from the `crate::ops` namespace to avoid dead-code noise during
// the incremental migration (Task 2 → Task 4).
pub(crate) use message::{
    AckMessageRequest, PostMessageRequest, PresenceRequest, ReadMessagesRequest, knock_metadata,
    list_messages_history, list_messages_live, post_ack, post_message, set_presence,
};

pub(crate) struct MessageFilters<'a> {
    pub(crate) repo: Option<&'a str>,
    pub(crate) session: Option<&'a str>,
    pub(crate) tags: &'a [String],
    pub(crate) thread_id: Option<&'a str>,
}

impl MessageFilters<'_> {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.repo.is_none()
            && self.session.is_none()
            && self.tags.is_empty()
            && self.thread_id.is_none()
    }
}

#[cfg(test)]
pub(crate) fn message_matches_filters(msg: &Message, filters: &MessageFilters<'_>) -> bool {
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

#[cfg(test)]
pub(crate) fn extra_filter_fetch_limit(limit: usize, filters: &MessageFilters<'_>) -> usize {
    if filters.is_empty() {
        limit
    } else {
        (limit * XREVRANGE_OVERFETCH_FACTOR).max(XREVRANGE_MIN_FETCH)
    }
}

pub(crate) fn scoped_required_tags(filters: &MessageFilters<'_>) -> Vec<String> {
    let tags: Vec<&str> = filters.tags.iter().map(String::as_str).collect();
    query_scope_tags(filters.repo, filters.session, &tags)
}
