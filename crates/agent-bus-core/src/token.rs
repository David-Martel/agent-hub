//! Token estimation and context compaction utilities.
//!
//! Provides lightweight heuristics for estimating LLM token counts and
//! compacting conversation context to fit within a token budget.
//!
//! # Examples
//!
//! ```rust,ignore
//! use crate::token::{estimate_tokens, minimize_message, compact_context};
//!
//! let tokens = estimate_tokens("Hello, world!");
//! assert!(tokens > 0);
//! ```

use crate::models::Message;

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Estimate the number of LLM tokens in `text`.
///
/// Uses a simple heuristic: if JSON-like characters (`{`, `}`, `[`, `]`, `"`,
/// `:`) account for more than 15 % of the total character count the text is
/// treated as structured JSON and encoded at **2.5 chars / token**; otherwise
/// natural-language density of **4.0 chars / token** is assumed.  The result
/// is rounded up (ceiling division).
///
/// This is intentionally cheap — no tokeniser is linked.  Accuracy is
/// sufficient for context budget decisions where ±10 % error is acceptable.
///
/// # Errors
///
/// This function never returns an error.
///
/// # Examples
///
/// ```rust,ignore
/// assert!(estimate_tokens("hello world") > 0);
/// ```
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let total = text.chars().count();
    let json_chars = text
        .chars()
        .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '"' | ':'))
        .count();

    // If >15% are JSON structural characters use the denser ratio (2.5 chars/token).
    // Otherwise assume natural-language density (4.0 chars/token).
    //
    // Integer ceiling division avoids float-cast lints.  Both ratios are scaled
    // by 10 to represent them as exact integers: 4.0 → 40, 2.5 → 25.
    let denominator: usize = if json_chars * 100 > total * 15 {
        25
    } else {
        40
    };
    (total * 10).div_ceil(denominator)
}

// ---------------------------------------------------------------------------
// Message minimisation (for context compaction)
// ---------------------------------------------------------------------------

/// Produce a token-minimised [`serde_json::Value`] from a [`Message`].
///
/// Field name shortening and default-value stripping mirrors the logic in
/// [`crate::output::minimize_value`], specialised for the known `Message`
/// schema so it can operate without a round-trip through `serde_json::Value`.
///
/// Shortened field map:
/// - `timestamp_utc` → `ts`
/// - `from` → `f`
/// - `to` → `t`
/// - `topic` → `tp`
/// - `body` → `b`
/// - `priority` → `p` (only if not `"normal"`)
/// - `request_ack` → `ack` (only if `true`)
/// - `reply_to` → `rt` (only if `Some`)
/// - `thread_id` → `tid` (only if `Some`)
/// - `tags` → `tg` (only if non-empty)
/// - `metadata` → `m` (only if not `null` / empty object)
///
/// Fields `id`, `protocol_version`, and `stream_id` are omitted entirely as
/// they are not useful for LLM context.
///
/// # Examples
///
/// ```rust,ignore
/// let min = minimize_message(&msg);
/// assert!(min.get("f").is_some());  // "from" → "f"
/// ```
#[must_use]
pub fn minimize_message(msg: &Message) -> serde_json::Value {
    let mut map = serde_json::Map::new();

    map.insert(
        "ts".to_owned(),
        serde_json::Value::String(msg.timestamp_utc.clone()),
    );
    map.insert("f".to_owned(), serde_json::Value::String(msg.from.clone()));
    map.insert("t".to_owned(), serde_json::Value::String(msg.to.clone()));
    map.insert(
        "tp".to_owned(),
        serde_json::Value::String(msg.topic.clone()),
    );
    map.insert("b".to_owned(), serde_json::Value::String(msg.body.clone()));

    // Priority: only include when non-default.
    if msg.priority != "normal" {
        map.insert(
            "p".to_owned(),
            serde_json::Value::String(msg.priority.clone()),
        );
    }

    // request_ack: only include when true.
    if msg.request_ack {
        map.insert("ack".to_owned(), serde_json::Value::Bool(true));
    }

    // reply_to: only include when Some.
    if let Some(ref rt) = msg.reply_to {
        map.insert("rt".to_owned(), serde_json::Value::String(rt.clone()));
    }

    // thread_id: only include when Some.
    if let Some(ref tid) = msg.thread_id {
        map.insert("tid".to_owned(), serde_json::Value::String(tid.clone()));
    }

    // tags: only include when non-empty.
    if !msg.tags.is_empty() {
        let tags: Vec<serde_json::Value> = msg
            .tags
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect();
        map.insert("tg".to_owned(), serde_json::Value::Array(tags));
    }

    // metadata: include when it is not null and not an empty object.
    let include_meta = match &msg.metadata {
        serde_json::Value::Null => false,
        serde_json::Value::Object(o) => !o.is_empty(),
        _ => true,
    };
    if include_meta {
        map.insert("m".to_owned(), msg.metadata.clone());
    }

    serde_json::Value::Object(map)
}

// ---------------------------------------------------------------------------
// Context compaction
// ---------------------------------------------------------------------------

/// Compact a slice of messages into a token-budget-limited list.
///
/// Iterates `messages` from **newest to oldest**, minimises each via
/// [`minimize_message`], estimates the token cost of its JSON representation,
/// and accumulates messages until `max_tokens` would be exceeded.  The
/// returned `Vec` is in **oldest-first** order (matching the original slice
/// order) so callers can pass it directly as an ordered conversation context.
///
/// If a single minimised message would itself exceed `max_tokens` it is still
/// included (to avoid returning an empty result for very tight budgets).
///
/// # Examples
///
/// ```rust,ignore
/// let compacted = compact_context(&messages, 2000);
/// assert!(compacted.len() <= messages.len());
/// ```
#[must_use]
pub fn compact_context(messages: &[Message], max_tokens: usize) -> Vec<serde_json::Value> {
    let mut budget = max_tokens;
    // Collect indices (newest-first) that fit within the budget.
    let mut indices: Vec<usize> = Vec::new();

    for (i, msg) in messages.iter().enumerate().rev() {
        let minimised = minimize_message(msg);
        let json = serde_json::to_string(&minimised).unwrap_or_default();
        let cost = estimate_tokens(&json);

        if indices.is_empty() || cost <= budget {
            // Always include at least one message; subsequent ones only when they fit.
            indices.push(i);
            budget = budget.saturating_sub(cost);
        } else {
            break;
        }
    }

    // Re-order to oldest-first by reversing the collected indices.
    indices.reverse();
    indices
        .into_iter()
        .map(|i| minimize_message(&messages[i]))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use super::*;

    fn make_message(from: &str, to: &str, topic: &str, body: &str) -> Message {
        Message {
            id: "test-id".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            topic: topic.to_owned(),
            body: body.to_owned(),
            thread_id: None,
            tags: smallvec![],
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        }
    }

    // -----------------------------------------------------------------------
    // estimate_tokens
    // -----------------------------------------------------------------------

    #[test]
    fn estimate_tokens_empty_string_returns_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_natural_language_uses_4_chars_per_token() {
        // 8 chars of plain text — no JSON structural chars → ceil(8 / 4.0) = 2
        let text = "helloXXX";
        assert_eq!(estimate_tokens(text), 2);
    }

    #[test]
    fn estimate_tokens_json_dense_text_uses_2_5_chars_per_token() {
        // `{"a":"b"}` — 9 chars, structural chars: { " " : " } = 5 → 5/9 ≈ 55% > 15%
        // ceil(9 / 2.5) = ceil(3.6) = 4
        let text = r#"{"a":"b"}"#;
        assert_eq!(estimate_tokens(text), 4);
    }

    #[test]
    fn estimate_tokens_exactly_15_percent_uses_natural_ratio() {
        // 20 chars total, 3 structural (15.0%) → NOT > 15%, so 4.0 ratio
        // ceil(20 / 4.0) = 5
        let structural_count = 3_usize;
        let total = 20_usize;
        let text = format!(
            "{}{}",
            "a".repeat(total - structural_count),
            "{\":" // 3 structural chars
        );
        assert_eq!(text.len(), total);
        let json_count = text
            .chars()
            .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '"' | ':'))
            .count();
        assert_eq!(json_count, structural_count);
        // 3/20 = 15.0%, NOT > 15%, uses 4.0 ratio
        assert_eq!(estimate_tokens(&text), 5);
    }

    #[test]
    fn estimate_tokens_above_15_percent_uses_json_ratio() {
        // 16 chars, 3 structural → 3/16 = 18.75% > 15% → json ratio (2.5)
        // ceil(16 / 2.5) = ceil(6.4) = 7
        let text = format!("{}{}", "a".repeat(13), "{\":");
        let json_count = text
            .chars()
            .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '"' | ':'))
            .count();
        assert_eq!(json_count, 3);
        assert_eq!(text.len(), 16);
        assert_eq!(estimate_tokens(&text), 7);
    }

    #[test]
    fn estimate_tokens_positive_for_nonempty_text() {
        assert!(estimate_tokens("hello") > 0);
        assert!(estimate_tokens("a") > 0);
    }

    #[test]
    fn estimate_tokens_real_json_message_is_reasonable() {
        // A typical minimised JSON message should be 5-100 tokens.
        let json =
            r#"{"ts":"2026-01-01T00:00:00Z","f":"claude","t":"codex","tp":"status","b":"ready"}"#;
        let tokens = estimate_tokens(json);
        assert!(tokens >= 5, "expected >= 5, got {tokens}");
        assert!(tokens <= 100, "expected <= 100, got {tokens}");
    }

    // -----------------------------------------------------------------------
    // minimize_message
    // -----------------------------------------------------------------------

    #[test]
    fn minimize_message_shortens_required_fields() {
        let msg = make_message("claude", "codex", "status", "ready");
        let min = minimize_message(&msg);
        let obj = min.as_object().expect("should be an object");

        assert_eq!(obj["ts"], "2026-01-01T00:00:00Z");
        assert_eq!(obj["f"], "claude");
        assert_eq!(obj["t"], "codex");
        assert_eq!(obj["tp"], "status");
        assert_eq!(obj["b"], "ready");
    }

    #[test]
    fn minimize_message_strips_id_protocol_version_stream_id() {
        let msg = make_message("a", "b", "t", "body");
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("protocol_version"));
        assert!(!obj.contains_key("stream_id"));
    }

    #[test]
    fn minimize_message_strips_default_priority() {
        let msg = make_message("a", "b", "t", "body"); // priority = "normal"
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("p"), "normal priority should be stripped");
    }

    #[test]
    fn minimize_message_includes_non_default_priority() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.priority = "high".to_owned();
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert_eq!(obj["p"], "high");
    }

    #[test]
    fn minimize_message_strips_false_request_ack() {
        let msg = make_message("a", "b", "t", "body"); // request_ack = false
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(
            !obj.contains_key("ack"),
            "false request_ack should be stripped"
        );
    }

    #[test]
    fn minimize_message_includes_true_request_ack() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.request_ack = true;
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert_eq!(obj["ack"], true);
    }

    #[test]
    fn minimize_message_strips_empty_tags() {
        let msg = make_message("a", "b", "t", "body"); // tags = []
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("tg"), "empty tags should be stripped");
    }

    #[test]
    fn minimize_message_includes_non_empty_tags() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.tags = smallvec!["repo:agent-bus".to_owned(), "sprint".to_owned()];
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert_eq!(obj["tg"], serde_json::json!(["repo:agent-bus", "sprint"]));
    }

    #[test]
    fn minimize_message_strips_null_thread_id() {
        let msg = make_message("a", "b", "t", "body"); // thread_id = None
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("tid"));
    }

    #[test]
    fn minimize_message_includes_some_thread_id() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.thread_id = Some("thread-abc".to_owned());
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert_eq!(obj["tid"], "thread-abc");
    }

    #[test]
    fn minimize_message_strips_empty_metadata() {
        let msg = make_message("a", "b", "t", "body"); // metadata = {}
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("m"), "empty metadata should be stripped");
    }

    #[test]
    fn minimize_message_strips_null_metadata() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.metadata = serde_json::Value::Null;
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("m"), "null metadata should be stripped");
    }

    #[test]
    fn minimize_message_includes_non_empty_metadata() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.metadata = serde_json::json!({"key": "value"});
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert_eq!(obj["m"], serde_json::json!({"key": "value"}));
    }

    #[test]
    fn minimize_message_strips_none_reply_to() {
        let msg = make_message("a", "b", "t", "body");
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("rt"));
    }

    #[test]
    fn minimize_message_includes_some_reply_to() {
        let mut msg = make_message("a", "b", "t", "body");
        msg.reply_to = Some("msg-123".to_owned());
        let min = minimize_message(&msg);
        let obj = min.as_object().unwrap();
        assert_eq!(obj["rt"], "msg-123");
    }

    // -----------------------------------------------------------------------
    // compact_context
    // -----------------------------------------------------------------------

    #[test]
    fn compact_context_empty_messages_returns_empty() {
        let result = compact_context(&[], 1000);
        assert!(result.is_empty());
    }

    #[test]
    fn compact_context_returns_all_when_within_budget() {
        let msgs: Vec<Message> = (0..5)
            .map(|i| make_message("claude", "codex", "status", &format!("message {i}")))
            .collect();
        let result = compact_context(&msgs, 10_000);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn compact_context_limits_to_budget() {
        let msgs: Vec<Message> = (0..20)
            .map(|i| make_message("claude", "codex", "status", &format!("message {i}")))
            .collect();
        // With a very tight budget only a few messages should fit.
        let result = compact_context(&msgs, 30);
        assert!(
            result.len() < 20,
            "should have fewer than 20 with tight budget, got {}",
            result.len()
        );
    }

    #[test]
    fn compact_context_always_includes_at_least_one_message() {
        // Even a max_tokens=0 budget should return the newest message.
        let msgs = vec![make_message("a", "b", "t", "only one")];
        let result = compact_context(&msgs, 0);
        assert_eq!(result.len(), 1, "must include at least one message");
    }

    #[test]
    fn compact_context_result_is_oldest_first() {
        let msgs = vec![
            make_message("a", "b", "t", "first"),
            make_message("a", "b", "t", "second"),
            make_message("a", "b", "t", "third"),
        ];
        let result = compact_context(&msgs, 10_000);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["b"], "first");
        assert_eq!(result[1]["b"], "second");
        assert_eq!(result[2]["b"], "third");
    }

    #[test]
    fn compact_context_newest_messages_prioritised_when_budget_tight() {
        // If budget only allows 1 message, we should get the NEWEST one.
        let msgs = vec![
            make_message("a", "b", "t", "old message here"),
            make_message("a", "b", "t", "new message here"),
        ];
        // Tight budget: only 1 message fits.
        let result = compact_context(&msgs, 15);
        assert_eq!(result.len(), 1);
        // The returned message should be the newest (last in original slice).
        assert_eq!(result[0]["b"], "new message here");
    }

    #[test]
    fn compact_context_minimises_each_message() {
        let mut msg = make_message("claude", "codex", "status", "hi");
        msg.priority = "normal".to_owned(); // should be stripped
        msg.tags = smallvec!["sprint".to_owned()];
        let msgs = vec![msg];
        let result = compact_context(&msgs, 10_000);
        assert_eq!(result.len(), 1);
        let obj = result[0].as_object().unwrap();
        // Minimised: no "id", "p" stripped (normal priority), tags present.
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("p"));
        assert!(obj.contains_key("tg"));
    }
}
