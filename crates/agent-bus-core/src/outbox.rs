//! Durable client requests and ordered, explicitly classified replay.
//!
//! The journal contains request data, never authentication material. Its scope
//! is a configured destination identity, not proof of a per-agent identity.
//! Transports must validate their credential provider before opening a journal.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub(crate) fn digest_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 15)]));
    }
    encoded
}
const JOURNAL_LIMIT: u64 = 32 * 1024 * 1024;
const REQUEST_LIMIT: usize = 1_000;

const RECORD_LIMIT: usize = crate::validation::MAX_BODY_LEN + 16_384;

/// Preserve existing schema defaults for each client surface.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientSurface {
    Cli,
    Mcp,
}

impl ClientSurface {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mcp => "mcp",
        }
    }
}

/// Operations whose requests can be retained offline.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Send,
    Presence,
    Ack,
    /// Requests a lease from the authority; retention never grants one.
    ClaimRequest,
}

impl Operation {
    /// Existing MCP operation with the same argument contract.
    #[must_use]
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::Send => "post_message",
            Self::Presence => "set_presence",
            Self::Ack => "ack_message",
            Self::ClaimRequest => "claim_resource",
        }
    }

    /// Parse only supported operations; lease renew/release are never queued.
    #[must_use]
    pub fn from_tool(name: &str) -> Option<Self> {
        match name {
            "post_message" => Some(Self::Send),
            "set_presence" => Some(Self::Presence),
            "ack_message" => Some(Self::Ack),
            "claim_resource" => Some(Self::ClaimRequest),
            _ => None,
        }
    }
}

/// A request's stable, durable replay identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplayRequest {
    pub request_id: Uuid,
    /// Persisted random client origin, shared by concurrent local writers.
    pub origin_id: Uuid,
    /// Monotonic within the private journal; used for newer-presence guards.
    pub sequence: u64,
    pub created_ms: u64,
    /// Absolute intrinsic expiry; zero means Send/ACK does not expire by age.
    pub expires_ms: u64,
    pub operation: Operation,
    pub surface: ClientSurface,
    pub arguments: Map<String, Value>,
}

impl ReplayRequest {
    /// Reject unsupported shapes and semantics before any durable write.
    ///
    /// # Errors
    /// Invalid schema, missing identity or invalid message data.
    pub fn validate(&self) -> Result<(), OutboxError> {
        crate::mcp_dispatch::validate_tool_arguments(self.operation.tool_name(), &self.arguments)
            .map_err(|_sanitized| OutboxError::InvalidRequest)?;
        if self.sequence == 0
            || (matches!(self.operation, Operation::Send | Operation::Ack) && self.expires_ms != 0)
            || (matches!(
                self.operation,
                Operation::Presence | Operation::ClaimRequest
            ) && self.created_ms >= self.expires_ms)
        {
            return Err(OutboxError::InvalidRequest);
        }
        for key in match self.operation {
            Operation::Send => &["sender", "recipient", "topic", "body"][..],
            Operation::Presence => &["agent"][..],
            Operation::Ack => &["agent", "message_id"][..],
            Operation::ClaimRequest => &["agent", "resource"][..],
        } {
            let text = self
                .arguments
                .get(*key)
                .and_then(Value::as_str)
                .ok_or(OutboxError::InvalidRequest)?;
            crate::validation::non_empty(text, key)
                .map_err(|_sanitized| OutboxError::InvalidRequest)?;
        }
        if self.operation == Operation::ClaimRequest {
            crate::ops::claim::parse_lease_mode(
                self.arguments
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("exclusive"),
            )
            .map_err(|_sanitized| OutboxError::InvalidRequest)?;
            if let Some(scope) = self.arguments.get("scope").and_then(Value::as_str) {
                crate::ops::claim::parse_resource_scope(scope)
                    .map_err(|_sanitized| OutboxError::InvalidRequest)?;
            }
        }
        if self.operation == Operation::Presence {
            for field in ["status", "session_id"] {
                if let Some(value) = self.arguments.get(field).and_then(Value::as_str) {
                    crate::validation::reject_nul_bytes(value, field)
                        .map_err(|_sanitized| OutboxError::InvalidRequest)?;
                }
            }
            if let Some(items) = self.arguments.get("capabilities").and_then(Value::as_array) {
                for value in items {
                    crate::validation::reject_nul_bytes(
                        value.as_str().ok_or(OutboxError::InvalidRequest)?,
                        "capability",
                    )
                    .map_err(|_sanitized| OutboxError::InvalidRequest)?;
                }
            }
        }
        if self.operation == Operation::Send {
            let topic = self.arguments["topic"]
                .as_str()
                .ok_or(OutboxError::InvalidRequest)?;
            let body = self.arguments["body"]
                .as_str()
                .ok_or(OutboxError::InvalidRequest)?;
            let schema = self.arguments.get("schema").and_then(Value::as_str);
            let resolved =
                crate::validation::resolve_message_schema(self.surface.as_str(), schema, topic)
                    .map_err(|_sanitized| OutboxError::InvalidRequest)?;
            let fitted = crate::validation::auto_fit_schema(
                body,
                resolved.map(crate::validation::MessageSchema::as_str),
            );
            crate::validation::validate_message_schema(
                &fitted,
                resolved.map(crate::validation::MessageSchema::as_str),
            )
            .map_err(|_sanitized| OutboxError::InvalidRequest)?;
        }
        if matches!(
            self.operation,
            Operation::Presence | Operation::ClaimRequest
        ) && self.expires_ms - self.created_ms != self.lifetime_ms()?
        {
            return Err(OutboxError::InvalidRequest);
        }
        if serde_json::to_vec(self)?.len() > RECORD_LIMIT {
            return Err(OutboxError::Capacity);
        }
        Ok(())
    }

    fn lifetime_ms(&self) -> Result<u64, OutboxError> {
        match self.operation {
            Operation::Presence | Operation::ClaimRequest => {
                let (field, default) = if self.operation == Operation::Presence {
                    ("ttl_seconds", 180)
                } else {
                    ("lease_ttl_seconds", 3_600)
                };
                let ttl = match self.arguments.get(field) {
                    Some(value) => value.as_u64().ok_or(OutboxError::InvalidRequest)?,
                    None => default,
                };
                if !(1..=86_400).contains(&ttl) {
                    return Err(OutboxError::InvalidRequest);
                }
                ttl.checked_mul(1_000).ok_or(OutboxError::InvalidRequest)
            }
            Operation::Send | Operation::Ack => Ok(0),
        }
    }

    /// Fingerprint complete immutable replay data for collision detection.
    ///
    /// # Errors
    /// JSON serialization failure.
    pub fn fingerprint(&self) -> Result<String, OutboxError> {
        Ok(digest_hex(&serde_json::to_vec(self)?))
    }
}

/// Transport outcomes distinguish retries from authentication/validation failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReplayResponse {
    Applied {
        result: Value,
    },
    /// Already expired or superseded requests have no new side effect.
    Superseded,
    /// A claim's prior grant cannot yet be established; do not repeat it.
    Pending,
}

/// A retryable transport failure is the only failure retained for replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayFailure {
    /// Connect/EOF/timeout, possibly after hub acceptance.
    Transport,
    /// Auth, validation, conflict, unsupported protocol or bad response shape.
    Permanent,
}

/// Authenticated destination transport; implementations do not log secrets.
pub trait ReplayTransport {
    /// Replay one immutable request.
    ///
    /// # Errors
    /// Transport ambiguity or permanent rejection; never an offline grant.
    fn replay(&self, request: &ReplayRequest) -> Result<ReplayResponse, ReplayFailure>;
}

/// Journal errors never include the body or stored request arguments.
#[derive(Debug, Error)]
pub enum OutboxError {
    #[error("outbox storage failure")]
    Io(#[from] std::io::Error),
    #[error("outbox representation failure")]
    Json(#[from] serde_json::Error),
    #[error("outbox journal corrupt; preserve it for recovery")]
    Corrupt,
    #[error("outbox destination changed; existing requests require explicit disposition")]
    ScopeChanged,
    #[error("invalid durable request")]
    InvalidRequest,
    #[error("outbox capacity exceeded; request was not retained")]
    Capacity,
    #[error("outbox private custody validation failed")]
    Custody,
    #[error("another writer holds the outbox journal")]
    Busy,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Header {
        origin_id: Uuid,
        destination: String,
    },
    Queued {
        request: ReplayRequest,
    },
    Attempted {
        request_id: Uuid,
    },
    Settled {
        request_id: Uuid,
        disposition: Disposition,
    },
}

/// Retained terminal evidence; none of these means an offline grant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Applied,
    Expired,
    Superseded,
    Rejected,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    event: Event,
    sha256: String,
}

/// Result of a bounded ordered flush, including permanent rejection evidence.
#[derive(Debug, Default, Serialize)]
pub struct FlushReport {
    pub applied: usize,
    pub expired: usize,
    pub superseded: usize,
    pub rejected: usize,
    pub remaining: usize,
    pub blocked_request: Option<Uuid>,
    pub results: BTreeMap<Uuid, Value>,
    pub dispositions: BTreeMap<Uuid, Disposition>,
}

/// Exclusive journal guard held through persistence and bounded transport calls.
///
/// The supplied file must have been opened by the platform custody helper
/// (owner-only, regular, no reparse/symlink). A lock alone proves no privacy.
pub struct Journal {
    file: LockedFile,
    origin_id: Uuid,
    next_sequence: u64,
    pending: BTreeMap<u64, ReplayRequest>,
    poisoned: bool,
    attempted: HashSet<Uuid>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("pending", &self.pending.len())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

/// Unlock explicitly even if a cloned file/descriptor survives this owner.
struct LockedFile(File);
impl LockedFile {
    fn acquire(file: File) -> Result<Self, OutboxError> {
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => OutboxError::Busy,
            std::fs::TryLockError::Error(error) => OutboxError::Io(error),
        })?;
        Ok(Self(file))
    }
}
impl Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}
impl Drop for LockedFile {
    fn drop(&mut self) {
        // Closing one duplicate does not necessarily release an OFD lock.
        // Explicit unlock affects our owned file description before closing.
        let _ = self.0.unlock();
    }
}
impl Journal {
    /// Open a private journal without accepting inherited permissions.
    ///
    /// # Errors
    /// Custody failure, contention, corrupt journal or changed destination.
    pub fn open(path: &Path, destination: &str) -> Result<Self, OutboxError> {
        let file = crate::outbox_private::open(path).map_err(|_sanitized| OutboxError::Custody)?;
        Self::from_private_file(file, destination)
    }

    /// Read or initialize an exclusively locked private append-only journal.
    ///
    /// # Errors
    /// Corrupt/truncated journal, foreign destination, lock contention or I/O.
    // One bounded parser validates the complete sequence before accepting state.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn from_private_file(file: File, destination: &str) -> Result<Self, OutboxError> {
        if destination.is_empty()
            || destination.len() > 256
            || destination.chars().any(char::is_control)
        {
            return Err(OutboxError::ScopeChanged);
        }
        let mut file = LockedFile::acquire(file)?;
        if file.metadata()?.len() > JOURNAL_LIMIT {
            return Err(OutboxError::Capacity);
        }
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut bytes)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(OutboxError::Corrupt);
        }
        let mut journal = Self {
            file,
            origin_id: Uuid::new_v4(),
            next_sequence: 1,
            pending: BTreeMap::new(),
            poisoned: false,
            attempted: HashSet::new(),
        };
        let mut header_seen = false;
        let mut all_ids = HashSet::new();
        for line in bytes
            .strip_suffix(b"\n")
            .unwrap_or(&bytes)
            .split(|byte| *byte == b'\n')
            .filter(|_| !bytes.is_empty())
        {
            if line.is_empty() {
                return Err(OutboxError::Corrupt);
            }
            if line.len() > RECORD_LIMIT {
                return Err(OutboxError::Corrupt);
            }
            let record: Record =
                serde_json::from_slice(line).map_err(|_sanitized| OutboxError::Corrupt)?;
            if record.sha256 != digest_hex(&serde_json::to_vec(&record.event)?) {
                return Err(OutboxError::Corrupt);
            }
            match record.event {
                Event::Header {
                    origin_id,
                    destination: stored,
                } => {
                    if header_seen || !all_ids.is_empty() {
                        return Err(OutboxError::Corrupt);
                    }
                    if stored != destination {
                        return Err(OutboxError::ScopeChanged);
                    }
                    journal.origin_id = origin_id;
                    header_seen = true;
                }
                Event::Queued { request } => {
                    request
                        .validate()
                        .map_err(|_sanitized| OutboxError::Corrupt)?;
                    if !header_seen
                        || request.origin_id != journal.origin_id
                        || request.sequence != journal.next_sequence
                        || !all_ids.insert(request.request_id)
                    {
                        return Err(OutboxError::Corrupt);
                    }
                    journal.next_sequence = request
                        .sequence
                        .checked_add(1)
                        .ok_or(OutboxError::Corrupt)?;
                    journal.pending.insert(request.sequence, request);
                }
                Event::Attempted { request_id } => {
                    if !journal
                        .pending
                        .values()
                        .any(|request| request.request_id == request_id)
                        || !journal.attempted.insert(request_id)
                    {
                        return Err(OutboxError::Corrupt);
                    }
                }
                Event::Settled {
                    request_id,
                    disposition: _,
                } => {
                    let sequence = journal
                        .pending
                        .iter()
                        .find(|(_, request)| request.request_id == request_id)
                        .map(|(sequence, _)| *sequence)
                        .ok_or(OutboxError::Corrupt)?;
                    journal.pending.remove(&sequence);
                    journal.attempted.remove(&request_id);
                }
            }
        }
        if !header_seen {
            if !bytes.is_empty() {
                return Err(OutboxError::Corrupt);
            }
            journal.append(&Event::Header {
                origin_id: journal.origin_id,
                destination: destination.to_owned(),
            })?;
        }
        if journal.pending.len() > REQUEST_LIMIT {
            return Err(OutboxError::Capacity);
        }
        Ok(journal)
    }

    fn append(&mut self, event: &Event) -> Result<(), OutboxError> {
        if self.poisoned {
            return Err(OutboxError::Corrupt);
        }
        let sha256 = digest_hex(&serde_json::to_vec(event)?);
        let mut encoded =
            serde_json::to_vec(&serde_json::json!({"event": event, "sha256": sha256}))?;
        encoded.push(b'\n');
        if encoded.len() > RECORD_LIMIT
            || self
                .file
                .metadata()?
                .len()
                .saturating_add(encoded.len() as u64)
                > JOURNAL_LIMIT
        {
            return Err(OutboxError::Capacity);
        }
        // Any failed append may have left an incomplete record. Preserve it
        // and refuse reuse of this guard; never append behind unknown bytes.
        self.poisoned = true;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&encoded)?;
        self.file.sync_all()?;
        self.poisoned = false;
        Ok(())
    }

    /// Persist before any outbound request; errors never imply retention.
    ///
    /// # Errors
    /// Invalid request, capacity or storage failure.
    pub fn enqueue(
        &mut self,
        operation: Operation,
        surface: ClientSurface,
        arguments: Map<String, Value>,
        now_ms: u64,
    ) -> Result<ReplayRequest, OutboxError> {
        if self.pending.len() >= REQUEST_LIMIT {
            return Err(OutboxError::Capacity);
        }
        let ttl = match operation {
            Operation::Presence => arguments
                .get("ttl_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(180)
                .checked_mul(1_000),
            Operation::ClaimRequest => arguments
                .get("lease_ttl_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(3_600)
                .checked_mul(1_000),
            Operation::Send | Operation::Ack => Some(0),
        }
        .ok_or(OutboxError::InvalidRequest)?;
        let request = ReplayRequest {
            request_id: Uuid::now_v7(),
            origin_id: self.origin_id,
            sequence: self.next_sequence,
            created_ms: now_ms,
            expires_ms: if ttl == 0 {
                0
            } else {
                now_ms.checked_add(ttl).ok_or(OutboxError::InvalidRequest)?
            },
            operation,
            surface,
            arguments,
        };
        request.validate()?;
        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(OutboxError::Capacity)?;
        self.append(&Event::Queued {
            request: request.clone(),
        })?;
        self.next_sequence = next_sequence;
        self.pending.insert(request.sequence, request.clone());
        Ok(request)
    }

    fn settle(
        &mut self,
        request: &ReplayRequest,
        disposition: Disposition,
    ) -> Result<(), OutboxError> {
        self.append(&Event::Settled {
            request_id: request.request_id,
            disposition,
        })?;
        self.pending.remove(&request.sequence);
        self.attempted.remove(&request.request_id);
        Ok(())
    }

    /// Flush in sequence; a retryable or indeterminate claim stops the queue.
    ///
    /// # Errors
    /// Durable settlement failure; retain the same ID for subsequent replay.
    pub fn flush(
        &mut self,
        transport: &impl ReplayTransport,
        now_ms: u64,
        limit: usize,
    ) -> Result<FlushReport, OutboxError> {
        let mut report = FlushReport::default();
        let requests: Vec<_> = self
            .pending
            .values()
            .take(limit.min(REQUEST_LIMIT))
            .cloned()
            .collect();
        for request in requests {
            if !self.attempted.contains(&request.request_id)
                && request.expires_ms != 0
                && now_ms >= request.expires_ms
            {
                self.settle(&request, Disposition::Expired)?;
                report
                    .dispositions
                    .insert(request.request_id, Disposition::Expired);
                report.expired += 1;
                continue;
            }
            if !self.attempted.contains(&request.request_id)
                && request.operation == Operation::Presence
                && self.pending.values().any(|newer| {
                    newer.sequence > request.sequence
                        && newer.operation == Operation::Presence
                        && newer.arguments.get("agent") == request.arguments.get("agent")
                })
            {
                self.settle(&request, Disposition::Superseded)?;
                report
                    .dispositions
                    .insert(request.request_id, Disposition::Superseded);
                report.superseded += 1;
                continue;
            }
            if !self.attempted.contains(&request.request_id) {
                self.append(&Event::Attempted {
                    request_id: request.request_id,
                })?;
                self.attempted.insert(request.request_id);
            }
            match transport.replay(&request) {
                Ok(ReplayResponse::Applied { result }) => {
                    self.settle(&request, Disposition::Applied)?;
                    report
                        .dispositions
                        .insert(request.request_id, Disposition::Applied);
                    report.results.insert(request.request_id, result);
                    report.applied += 1;
                }
                Ok(ReplayResponse::Superseded) => {
                    self.settle(&request, Disposition::Superseded)?;
                    report
                        .dispositions
                        .insert(request.request_id, Disposition::Superseded);
                    report.superseded += 1;
                }
                Ok(ReplayResponse::Pending) | Err(ReplayFailure::Transport) => {
                    report.blocked_request = Some(request.request_id);
                    break;
                }
                Err(ReplayFailure::Permanent) => {
                    self.settle(&request, Disposition::Rejected)?;
                    report
                        .dispositions
                        .insert(request.request_id, Disposition::Rejected);
                    report.rejected += 1;
                    report.blocked_request = Some(request.request_id);
                    break;
                }
            }
        }
        report.remaining = self.pending.len();
        Ok(report)
    }

    /// Pending requests in durable sequence order.
    pub fn pending(&self) -> impl Iterator<Item = &ReplayRequest> {
        self.pending.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs::OpenOptions;
    use tempfile::NamedTempFile;

    fn open(temp: &NamedTempFile) -> Journal {
        Journal::from_private_file(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path())
                .unwrap(),
            "onsite-hub-scope",
        )
        .unwrap()
    }

    fn send(body: &str) -> Map<String, Value> {
        serde_json::json!({"sender":"test", "recipient":"receiver", "topic":"status", "body":body})
            .as_object()
            .unwrap()
            .clone()
    }

    struct Transport {
        calls: RefCell<Vec<Uuid>>,
        fail_after: usize,
    }
    impl ReplayTransport for Transport {
        fn replay(&self, request: &ReplayRequest) -> Result<ReplayResponse, ReplayFailure> {
            let mut calls = self.calls.borrow_mut();
            calls.push(request.request_id);
            if calls.len() > self.fail_after {
                return Err(ReplayFailure::Transport);
            }
            Ok(ReplayResponse::Applied {
                result: serde_json::json!({"id":request.request_id}),
            })
        }
    }

    #[test]
    fn sends_and_acknowledgements_older_than_seven_days_replay_without_age_loss() {
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open(&temp);
        let send = journal
            .enqueue(Operation::Send, ClientSurface::Cli, send("weeks old"), 100)
            .unwrap();
        let ack = journal
            .enqueue(
                Operation::Ack,
                ClientSurface::Cli,
                serde_json::json!({"agent":"fixture","message_id":"message"})
                    .as_object()
                    .unwrap()
                    .clone(),
                101,
            )
            .unwrap();
        assert_eq!((send.expires_ms, ack.expires_ms), (0, 0));
        drop(journal);
        let mut recovered = open(&temp);
        let transport = Transport {
            calls: RefCell::new(vec![]),
            fail_after: 2,
        };
        let report = recovered
            .flush(&transport, 100 + 30 * 24 * 60 * 60 * 1000, 16)
            .unwrap();
        assert_eq!(
            (report.applied, report.expired, report.remaining),
            (2, 0, 0)
        );
        assert_eq!(
            *transport.calls.borrow(),
            vec![send.request_id, ack.request_id]
        );
    }

    #[test]
    fn attempted_presence_survives_reopen_expiry_and_newer_entry_until_cached_settlement() {
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open(&temp);
        let old = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                serde_json::json!({"agent":"fixture","ttl_seconds":1})
                    .as_object()
                    .unwrap()
                    .clone(),
                100,
            )
            .unwrap();
        let offline = Transport {
            calls: RefCell::new(vec![]),
            fail_after: 0,
        };
        assert_eq!(journal.flush(&offline, 101, 16).unwrap().remaining, 1);
        drop(journal);
        let mut recovered = open(&temp);
        let newer = recovered
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                serde_json::json!({"agent":"fixture","ttl_seconds":1})
                    .as_object()
                    .unwrap()
                    .clone(),
                1200,
            )
            .unwrap();
        let transport = Transport {
            calls: RefCell::new(vec![]),
            fail_after: 2,
        };
        let report = recovered.flush(&transport, 1201, 16).unwrap();
        assert_eq!((report.applied, report.remaining), (2, 0));
        assert_eq!(
            *transport.calls.borrow(),
            vec![old.request_id, newer.request_id]
        );
    }

    #[test]
    fn stable_request_survives_real_file_reopen_and_partial_ordered_flush() {
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open(&temp);
        let first = journal
            .enqueue(Operation::Send, ClientSurface::Cli, send("first"), 10_000)
            .unwrap();
        let second = journal
            .enqueue(Operation::Send, ClientSurface::Cli, send("second"), 10_001)
            .unwrap();
        let third = journal
            .enqueue(Operation::Send, ClientSurface::Cli, send("third"), 10_002)
            .unwrap();
        drop(journal);
        let mut journal = open(&temp);
        assert_eq!(
            journal
                .pending()
                .map(|request| request.request_id)
                .collect::<Vec<_>>(),
            vec![first.request_id, second.request_id, third.request_id]
        );
        let transport = Transport {
            calls: RefCell::new(vec![]),
            fail_after: 1,
        };
        let report = journal.flush(&transport, 10_003, 1_000).unwrap();
        assert_eq!(
            (report.applied, report.remaining, report.blocked_request),
            (1, 2, Some(second.request_id))
        );
        assert_eq!(
            *transport.calls.borrow(),
            vec![first.request_id, second.request_id]
        );
        assert_eq!(
            report.results[&first.request_id]["id"],
            first.request_id.to_string()
        );
        drop(journal);
        let mut journal = open(&temp);
        let transport = Transport {
            calls: RefCell::new(vec![]),
            fail_after: usize::MAX,
        };
        assert_eq!(journal.flush(&transport, 10_004, 1_000).unwrap().applied, 2);
        assert_eq!(
            *transport.calls.borrow(),
            vec![second.request_id, third.request_id]
        );
        drop(journal);
        assert_eq!(open(&temp).pending().count(), 0);
    }

    #[test]
    fn exclusive_file_lock_refuses_actual_second_writer_then_releases() {
        let temp = NamedTempFile::new().unwrap();
        let first = open(&temp);
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path())
            .unwrap();
        assert!(matches!(
            Journal::from_private_file(second, "onsite-hub-scope"),
            Err(OutboxError::Busy)
        ));
        drop(first);
        assert_eq!(open(&temp).pending().count(), 0);
    }

    #[test]
    fn explicit_unlock_releases_lock_while_original_duplicate_remains_open() {
        let temp = NamedTempFile::new().unwrap();
        let first = open(&temp);
        let surviving_duplicate = first.file.try_clone().unwrap();
        assert!(matches!(
            Journal::from_private_file(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(temp.path())
                    .unwrap(),
                "onsite-hub-scope"
            ),
            Err(OutboxError::Busy)
        ));
        drop(first);
        let second = open(&temp);
        assert_eq!(second.pending().count(), 0);
        assert!(surviving_duplicate.metadata().unwrap().is_file());
        drop(second);
        drop(surviving_duplicate);
    }
    #[test]
    fn destination_change_refuses_without_mutating_original_bytes() {
        let temp = NamedTempFile::new().unwrap();
        let journal = open(&temp);
        drop(journal);
        let original = std::fs::read(temp.path()).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path())
            .unwrap();
        assert!(matches!(
            Journal::from_private_file(file, "cloud-scope"),
            Err(OutboxError::ScopeChanged)
        ));
        assert_eq!(std::fs::read(temp.path()).unwrap(), original);
    }

    #[test]
    fn truncated_or_corrupted_journal_is_preserved_and_never_repaired_silently() {
        for suffix in [b"unfinished".as_slice(), b"\n".as_slice()] {
            let temp = NamedTempFile::new().unwrap();
            drop(open(&temp));
            let mut file = OpenOptions::new().append(true).open(temp.path()).unwrap();
            file.write_all(suffix).unwrap();
            file.sync_all().unwrap();
            drop(file);
            let before = std::fs::read(temp.path()).unwrap();
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path())
                .unwrap();
            assert!(matches!(
                Journal::from_private_file(file, "onsite-hub-scope"),
                Err(OutboxError::Corrupt)
            ));
            assert_eq!(std::fs::read(temp.path()).unwrap(), before);
        }
    }

    #[test]
    fn expired_and_newer_presence_have_no_transport_side_effect() {
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open(&temp);
        let presence = |status: &str| {
            serde_json::json!({"agent":"test", "status":status, "ttl_seconds":1})
                .as_object()
                .unwrap()
                .clone()
        };
        journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Mcp,
                presence("old"),
                1_000,
            )
            .unwrap();
        let latest = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Mcp,
                presence("latest"),
                1_001,
            )
            .unwrap();
        let transport = Transport {
            calls: RefCell::new(vec![]),
            fail_after: usize::MAX,
        };
        let report = journal.flush(&transport, 1_002, 10).unwrap();
        assert_eq!((report.superseded, report.applied), (1, 1));
        assert_eq!(*transport.calls.borrow(), vec![latest.request_id]);
        journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Mcp,
                presence("expired"),
                2_000,
            )
            .unwrap();
        assert_eq!(journal.flush(&transport, 3_000, 10).unwrap().expired, 1);
        assert_eq!(transport.calls.borrow().len(), 1);
    }

    #[test]
    fn invalid_arguments_and_unsupported_operations_are_never_retained() {
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open(&temp);
        let before = std::fs::read(temp.path()).unwrap();
        let mut invalid = send("bad");
        invalid.insert("priority".to_owned(), serde_json::json!("unknown"));
        assert!(matches!(
            journal.enqueue(Operation::Send, ClientSurface::Mcp, invalid, 100),
            Err(OutboxError::InvalidRequest)
        ));
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
        for tool in ["renew_claim", "release_claim", "pull_task", "list_messages"] {
            assert_eq!(Operation::from_tool(tool), None);
        }
    }

    #[test]
    fn pending_claim_never_settles_or_grants_and_prevents_later_overtaking() {
        struct ExpiredAtAuthority {
            calls: RefCell<Vec<Uuid>>,
        }
        impl ReplayTransport for ExpiredAtAuthority {
            fn replay(&self, request: &ReplayRequest) -> Result<ReplayResponse, ReplayFailure> {
                self.calls.borrow_mut().push(request.request_id);
                if request.operation == Operation::ClaimRequest {
                    Ok(ReplayResponse::Superseded)
                } else {
                    Ok(ReplayResponse::Applied {
                        result: serde_json::json!({"id":request.request_id}),
                    })
                }
            }
        }
        struct Pending;
        impl ReplayTransport for Pending {
            fn replay(&self, _request: &ReplayRequest) -> Result<ReplayResponse, ReplayFailure> {
                Ok(ReplayResponse::Pending)
            }
        }
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open(&temp);
        let claim = journal.enqueue(Operation::ClaimRequest, ClientSurface::Mcp,
            serde_json::json!({"agent":"test", "resource":"owned-resource", "lease_ttl_seconds":1}).as_object().unwrap().clone(), 100).unwrap();
        journal
            .enqueue(
                Operation::Send,
                ClientSurface::Mcp,
                send("after claim"),
                101,
            )
            .unwrap();
        let report = journal.flush(&Pending, 102, 10).unwrap();
        assert_eq!(
            (report.applied, report.remaining, report.blocked_request),
            (0, 2, Some(claim.request_id))
        );
        let transport = ExpiredAtAuthority {
            calls: RefCell::new(vec![]),
        };
        let report = journal.flush(&transport, 1_100, 10).unwrap();
        assert_eq!(
            (report.superseded, report.applied, report.remaining),
            (1, 1, 0)
        );
        assert_eq!(transport.calls.borrow().len(), 2);
        assert_eq!(transport.calls.borrow()[0], claim.request_id);
    }
}
