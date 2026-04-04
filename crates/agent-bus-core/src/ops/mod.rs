//! Shared bus operations reused across CLI, HTTP, and MCP transports.

pub mod ack_deadline;
pub mod admin;
pub mod channel;
pub mod claim;
pub mod inbox;
pub mod inventory;
pub mod message;
pub mod subscription;
pub mod task;
pub mod thread;

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

    /// Returns `true` when the filters contain at least one tag-based or
    /// thread-based constraint that would benefit from a `PostgreSQL` GIN
    /// index scan rather than client-side filtering over a Redis stream dump.
    ///
    /// This is the gate used by `bus_list_messages_with_filters` to decide
    /// whether the optimised PG tag-query path is worthwhile.
    #[must_use]
    pub fn has_meaningful_tag_filters(&self) -> bool {
        self.repo.is_some()
            || self.session.is_some()
            || !self.tags.is_empty()
            || self.thread_id.is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(tags: &[&str], thread_id: Option<&str>) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_utc: "2026-04-04T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "status".to_owned(),
            body: "test".to_owned(),
            thread_id: thread_id.map(str::to_owned),
            tags: tags
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>()
                .into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        }
    }

    // -------------------------------------------------------------------
    // MessageFilters::is_empty
    // -------------------------------------------------------------------

    #[test]
    fn message_filters_is_empty_when_all_none() {
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: None,
        };
        assert!(filters.is_empty());
        assert!(!filters.has_meaningful_tag_filters());
    }

    // -------------------------------------------------------------------
    // MessageFilters::has_meaningful_tag_filters
    // -------------------------------------------------------------------

    #[test]
    fn has_meaningful_tag_filters_true_with_repo() {
        let filters = MessageFilters {
            repo: Some("agent-bus"),
            session: None,
            tags: &[],
            thread_id: None,
        };
        assert!(!filters.is_empty());
        assert!(filters.has_meaningful_tag_filters());
    }

    #[test]
    fn has_meaningful_tag_filters_true_with_session() {
        let filters = MessageFilters {
            repo: None,
            session: Some("s1"),
            tags: &[],
            thread_id: None,
        };
        assert!(filters.has_meaningful_tag_filters());
    }

    #[test]
    fn has_meaningful_tag_filters_true_with_tags() {
        let tags = vec!["wave:1".to_owned()];
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &tags,
            thread_id: None,
        };
        assert!(filters.has_meaningful_tag_filters());
    }

    #[test]
    fn has_meaningful_tag_filters_true_with_thread_id() {
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: Some("t-42"),
        };
        assert!(filters.has_meaningful_tag_filters());
    }

    // -------------------------------------------------------------------
    // message_matches_filters
    // -------------------------------------------------------------------

    #[test]
    fn message_matches_filters_passes_with_matching_repo() {
        let msg = make_message(&["repo:agent-bus", "session:s1"], Some("t-1"));
        let filters = MessageFilters {
            repo: Some("agent-bus"),
            session: None,
            tags: &[],
            thread_id: None,
        };
        assert!(message_matches_filters(&msg, &filters));
    }

    #[test]
    fn message_matches_filters_rejects_mismatched_repo() {
        let msg = make_message(&["repo:other"], None);
        let filters = MessageFilters {
            repo: Some("agent-bus"),
            session: None,
            tags: &[],
            thread_id: None,
        };
        assert!(!message_matches_filters(&msg, &filters));
    }

    #[test]
    fn message_matches_filters_passes_with_matching_session() {
        let msg = make_message(&["session:s1"], None);
        let filters = MessageFilters {
            repo: None,
            session: Some("s1"),
            tags: &[],
            thread_id: None,
        };
        assert!(message_matches_filters(&msg, &filters));
    }

    #[test]
    fn message_matches_filters_passes_with_matching_thread_id() {
        let msg = make_message(&[], Some("t-42"));
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: Some("t-42"),
        };
        assert!(message_matches_filters(&msg, &filters));
    }

    #[test]
    fn message_matches_filters_rejects_wrong_thread_id() {
        let msg = make_message(&[], Some("t-42"));
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: Some("t-99"),
        };
        assert!(!message_matches_filters(&msg, &filters));
    }

    #[test]
    fn message_matches_filters_passes_when_empty() {
        let msg = make_message(&[], None);
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: None,
        };
        assert!(message_matches_filters(&msg, &filters));
    }

    // -------------------------------------------------------------------
    // scoped_required_tags
    // -------------------------------------------------------------------

    #[test]
    fn scoped_required_tags_builds_from_repo_and_session() {
        let tags = vec!["wave:1".to_owned()];
        let filters = MessageFilters {
            repo: Some("agent-bus"),
            session: Some("s1"),
            tags: &tags,
            thread_id: None,
        };
        let scoped = scoped_required_tags(&filters);
        assert_eq!(scoped, vec!["repo:agent-bus", "session:s1", "wave:1"]);
    }

    #[test]
    fn scoped_required_tags_empty_when_no_filters() {
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: None,
        };
        let scoped = scoped_required_tags(&filters);
        assert!(scoped.is_empty());
    }

    // -------------------------------------------------------------------
    // extra_filter_fetch_limit
    // -------------------------------------------------------------------

    #[test]
    fn extra_filter_fetch_limit_no_multiplier_when_empty() {
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &[],
            thread_id: None,
        };
        assert_eq!(extra_filter_fetch_limit(10, &filters), 10);
    }

    #[test]
    fn extra_filter_fetch_limit_multiplies_when_tags_present() {
        let tags = vec!["repo:agent-bus".to_owned()];
        let filters = MessageFilters {
            repo: None,
            session: None,
            tags: &tags,
            thread_id: None,
        };
        let result = extra_filter_fetch_limit(10, &filters);
        assert!(
            result > 10,
            "fetch limit must be multiplied when tags are present"
        );
    }
}
