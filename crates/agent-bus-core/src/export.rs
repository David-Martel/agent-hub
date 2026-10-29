//! Complete, paginated history export (`agent-bus export --all`).
//!
//! The windowed `export` is capped at seven days and reads one store. A full
//! archive has to read both, because the stores can hold different messages
//! (agent-hub#194), and has to decode the two bodies differently: `PostgreSQL`
//! bodies are plaintext, Redis bodies marked `_compressed: lz4` are base64
//! LZ4. This module streams `PostgreSQL` in keyset pages, then the Redis
//! stream in `XRANGE` pages, decodes both to plaintext [`Message`]s and emits
//! each message id once. A message present in both stores is taken from
//! `PostgreSQL`; Redis supplies only the ids `PostgreSQL` lacks.
//!
//! Memory use is the set of emitted ids, not the message bodies.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::error::{AgentBusError, Result};
use crate::models::Message;
use crate::postgres_store::{export_messages_page, parse_timestamp_utc};
use crate::redis_bus::{connect, decode_stream_entry};
use crate::settings::Settings;

/// Default rows or entries fetched per page.
pub const DEFAULT_PAGE_SIZE: usize = 2000;

/// What an export emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportSummary {
    /// Messages read from `PostgreSQL`.
    pub pg_messages: usize,
    /// Messages found only in the Redis stream.
    pub redis_only: usize,
    /// Redis entries skipped because `PostgreSQL` already had the id.
    pub redis_duplicates: usize,
}

/// Cursor for the `PostgreSQL` phase: the last `(timestamp_utc, id)` read.
type PgCursor = (chrono::DateTime<chrono::Utc>, Uuid);

/// Merge a `PostgreSQL` page source and a Redis page source into `sink`.
///
/// Both sources are closures so the paging, de-duplication and ordering rules
/// are testable without services. `pg_page` receives the previous cursor and
/// returns the next page (empty ends the phase); `redis_page` receives the last
/// stream id (`None` first) and returns decoded messages with their stream id.
///
/// # Errors
///
/// Returns the first error from either source or from `sink`.
pub fn export_union(
    mut pg_page: impl FnMut(Option<PgCursor>) -> Result<Vec<Message>>,
    mut redis_page: impl FnMut(Option<&str>) -> Result<Vec<(String, Message)>>,
    mut sink: impl FnMut(&Message) -> Result<()>,
) -> Result<ExportSummary> {
    let mut summary = ExportSummary::default();
    let mut seen: HashSet<String> = HashSet::new();

    let mut cursor: Option<PgCursor> = None;
    loop {
        let page = pg_page(cursor)?;
        let Some(last) = page.last() else { break };
        let last_ts = parse_timestamp_utc(&last.timestamp_utc)?;
        let last_id = Uuid::parse_str(&last.id).map_err(|e| {
            AgentBusError::Internal(format!("bad id {} in PostgreSQL: {e}", last.id))
        })?;
        for message in &page {
            sink(message)?;
            seen.insert(message.id.clone());
            summary.pg_messages += 1;
        }
        cursor = Some((last_ts, last_id));
    }

    let mut after: Option<String> = None;
    loop {
        let page = redis_page(after.as_deref())?;
        let Some((last_stream_id, _)) = page.last() else {
            break;
        };
        after = Some(last_stream_id.clone());
        for (_, message) in &page {
            if seen.insert(message.id.clone()) {
                sink(message)?;
                summary.redis_only += 1;
            } else {
                summary.redis_duplicates += 1;
            }
        }
    }
    Ok(summary)
}

/// Decode one raw `XRANGE` page into `(stream_id, plaintext Message)` pairs.
fn decode_redis_page(raw: Vec<redis::Value>) -> Vec<(String, Message)> {
    raw.into_iter()
        .filter_map(|entry| {
            let redis::Value::Array(parts) = entry else {
                return None;
            };
            let mut parts = parts.into_iter();
            let stream_id = match parts.next()? {
                redis::Value::BulkString(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                redis::Value::SimpleString(text) => text,
                _ => return None,
            };
            let redis::Value::Array(flat) = parts.next()? else {
                return None;
            };
            let mut fields: HashMap<String, redis::Value> = HashMap::new();
            let mut flat = flat.into_iter();
            while let (Some(key), Some(value)) = (flat.next(), flat.next()) {
                let redis::Value::BulkString(key) = key else {
                    continue;
                };
                fields.insert(String::from_utf8_lossy(&key).into_owned(), value);
            }
            let mut message = decode_stream_entry(&fields);
            message.stream_id = Some(stream_id.clone());
            Some((stream_id, message))
        })
        .collect()
}

/// Export the complete history of both stores to `sink`.
///
/// # Errors
///
/// Returns an error when `PostgreSQL` is not configured, when either store
/// cannot be read, or when `sink` fails.
pub fn export_all(
    settings: &Settings,
    page_size: usize,
    sink: impl FnMut(&Message) -> Result<()>,
) -> Result<ExportSummary> {
    if settings.database_url.is_none() {
        return Err(AgentBusError::Internal(
            "export --all needs PostgreSQL (database_url is not configured)".to_owned(),
        ));
    }
    let page_size = page_size.max(1);
    let mut conn = connect(settings)?;
    export_union(
        |cursor| export_messages_page(settings, cursor, page_size),
        |after| {
            let start = after.map_or_else(|| "-".to_owned(), |id| format!("({id}"));
            let raw: Vec<redis::Value> = redis::cmd("XRANGE")
                .arg(&settings.stream_key)
                .arg(start)
                .arg("+")
                .arg("COUNT")
                .arg(page_size)
                .query(&mut conn)
                .map_err(|e| AgentBusError::Internal(format!("XRANGE failed: {e}")))?;
            Ok(decode_redis_page(raw))
        },
        sink,
    )
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;

    fn message(id: &str, ts: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            timestamp_utc: ts.to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "t".to_owned(),
            to: "all".to_owned(),
            topic: "status".to_owned(),
            body: body.to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
            client_msg_id: None,
            origin_hub: None,
            origin_seq: None,
            hlc: None,
            sensitivity: None,
        }
    }

    const UUID_A: &str = "00000000-0000-7000-8000-00000000000a";
    const UUID_B: &str = "00000000-0000-7000-8000-00000000000b";
    const UUID_C: &str = "00000000-0000-7000-8000-00000000000c";
    const UUID_D: &str = "00000000-0000-7000-8000-00000000000d";

    /// Serve `rows` in pages of `size` honouring the keyset cursor, as PostgreSQL does.
    fn pg_source(
        rows: Vec<Message>,
        size: usize,
    ) -> impl FnMut(Option<PgCursor>) -> Result<Vec<Message>> {
        move |cursor| {
            Ok(rows
                .iter()
                .filter(|m| {
                    cursor.is_none_or(|(ts, id)| {
                        let key = (
                            parse_timestamp_utc(&m.timestamp_utc).expect("ts"),
                            Uuid::parse_str(&m.id).expect("uuid"),
                        );
                        key > (ts, id)
                    })
                })
                .take(size)
                .cloned()
                .collect())
        }
    }

    fn redis_source(
        entries: Vec<(String, Message)>,
        size: usize,
    ) -> impl FnMut(Option<&str>) -> Result<Vec<(String, Message)>> {
        move |after| {
            Ok(entries
                .iter()
                .filter(|(sid, _)| after.is_none_or(|a| sid.as_str() > a))
                .take(size)
                .cloned()
                .collect())
        }
    }

    /// Protects: every PG row is exported across page boundaries, including
    /// rows that share a timestamp (the keyset tiebreak is the id).
    /// Detects: an off-by-one or `>=` cursor that drops or repeats a page edge.
    #[test]
    fn pg_pages_cover_every_row_including_equal_timestamps() {
        let ts = "2026-10-29T00:00:00.000000Z";
        let rows = vec![
            message(UUID_A, ts, "a"),
            message(UUID_B, ts, "b"),
            message(UUID_C, ts, "c"),
            message(UUID_D, "2026-10-29T00:00:01.000000Z", "d"),
        ];
        let mut out = Vec::new();
        let summary = export_union(pg_source(rows, 2), redis_source(vec![], 2), |m| {
            out.push(m.id.clone());
            Ok(())
        })
        .expect("export");
        assert_eq!(out, [UUID_A, UUID_B, UUID_C, UUID_D]);
        assert_eq!(summary.pg_messages, 4);
        assert_eq!(summary.redis_only, 0);
    }

    /// Protects: Redis supplies only ids PostgreSQL lacks, and each id is emitted once.
    #[test]
    fn redis_adds_only_missing_ids() {
        let pg = vec![message(UUID_A, "2026-10-29T00:00:00.000000Z", "pg-a")];
        let redis = vec![
            (
                "1-0".to_owned(),
                message(UUID_A, "2026-10-29T00:00:00.000000Z", "redis-a"),
            ),
            (
                "2-0".to_owned(),
                message(UUID_B, "2026-10-29T00:00:02.000000Z", "redis-b"),
            ),
        ];
        let mut out = Vec::new();
        let summary = export_union(pg_source(pg, 10), redis_source(redis, 1), |m| {
            out.push((m.id.clone(), m.body.clone()));
            Ok(())
        })
        .expect("export");
        assert_eq!(
            out,
            [
                (UUID_A.to_owned(), "pg-a".to_owned()),
                (UUID_B.to_owned(), "redis-b".to_owned())
            ],
            "PostgreSQL wins for ids in both stores"
        );
        assert_eq!(summary.redis_only, 1);
        assert_eq!(summary.redis_duplicates, 1);
    }

    /// Protects: a Redis body marked `_compressed: lz4` is exported as plaintext.
    /// Detects: archiving the base64 LZ4 blob instead of the message text.
    #[test]
    fn compressed_redis_bodies_are_decoded_to_plaintext() {
        let plain = "decoded plaintext body ".repeat(40);
        let blob = base64::engine::general_purpose::STANDARD
            .encode(lz4_flex::compress_prepend_size(plain.as_bytes()));
        let bulk = |s: &str| redis::Value::BulkString(s.as_bytes().to_vec());
        let entry = redis::Value::Array(vec![
            bulk("7-0"),
            redis::Value::Array(vec![
                bulk("id"),
                bulk(UUID_A),
                bulk("timestamp_utc"),
                bulk("2026-10-29T00:00:00.000000Z"),
                bulk("body"),
                bulk(&blob),
                bulk("_compressed"),
                bulk("lz4"),
            ]),
        ]);
        let page = decode_redis_page(vec![entry]);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].0, "7-0");
        assert_eq!(page[0].1.body, plain);
        assert_eq!(page[0].1.stream_id.as_deref(), Some("7-0"));
    }

    /// Protects: a sink error stops the export instead of being swallowed.
    #[test]
    fn sink_errors_propagate() {
        let rows = vec![message(UUID_A, "2026-10-29T00:00:00.000000Z", "a")];
        let err = export_union(pg_source(rows, 5), redis_source(vec![], 5), |_| {
            Err(AgentBusError::Internal("disk full".to_owned()))
        })
        .expect_err("sink failure must surface");
        assert!(err.to_string().contains("disk full"));
    }

    /// Protects: `export_all` refuses to run without PostgreSQL rather than
    /// silently producing a Redis-only "complete" archive.
    #[test]
    fn export_all_without_postgres_is_an_error() {
        let mut settings = Settings::for_test();
        settings.database_url = None;
        assert!(export_all(&settings, 10, |_| Ok(())).is_err());
    }
}
