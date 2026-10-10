//! Hub-side cloud sync task (agent-hub#79, steps 0 and 1).
//!
//! Replicates this hub's message stream and presence to the cloud tier
//! (`cloud/agentbus`) and ingests what other origins wrote there, entirely in
//! the background. The contract is `cloud/agentbus/SYNC-CONTRACT.md`; the
//! rules this module adds on top of it:
//!
//! - **Off by default.** The task runs only when a cloud URL, a 0600 token file
//!   and a `hub_identity` are all configured ([`resolve`]). With none of them
//!   set nothing here runs and `/health` is unchanged.
//! - **Local writes never wait on the cloud.** Nothing on the request path
//!   touches this module. The task reads Redis through the same pool as every
//!   other handler and holds no lock across a cloud call.
//! - **No echo.** Only messages whose `origin_hub` is absent or this hub are
//!   pushed. Messages pulled from the cloud are stored with their own
//!   `origin_hub`, so the push tailer skips them.
//! - **`sensitivity=no-offsite` is never pushed.**
//! - **Claims are never synced or proxied.** Claims are not in the message
//!   stream and this module has no claim code at all; the cloud is never the
//!   claim authority for an on-site resource.
//! - **Bounded memory.** The outbox holds at most `outbox_cap` messages. When
//!   it would overflow it is dropped and the task re-reads from the last
//!   acknowledged stream id (catch-up), so nothing is lost and nothing grows.
//! - **Backoff.** Failures retry after 1 to 60 s with full jitter.
//!
//! The engine is generic over two small traits ([`LocalStore`], [`CloudApi`])
//! so the push, pull, overflow and backoff logic is unit tested without Redis
//! or a network; [`RedisLocal`] and [`HttpCloud`] are the real implementations.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent_bus_core::hub_candidates::{BearerToken, CredentialEnv, read_private_token_file};
use agent_bus_core::models::{CloudHealth, Message, Presence, Sensitivity};
use agent_bus_core::redis_bus::RedisPool;
use agent_bus_core::settings::Settings;
use agent_bus_core::sync_store::{self, IngestOutcome};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// Largest batch the Worker accepts on `/sync/push` and `/sync/push-presence`.
pub(crate) const MAX_BATCH: usize = 500;
/// Default outbox bound, in messages.
const DEFAULT_OUTBOX_CAP: usize = 5_000;
/// Page size requested from `/sync/pull`.
const PULL_PAGE: usize = 200;
/// Idle poll interval between ticks.
const DEFAULT_POLL: Duration = Duration::from_secs(2);
/// Interval between presence pulls.
const DEFAULT_PRESENCE_POLL: Duration = Duration::from_secs(15);
/// Per-request timeout so a hung cloud cannot wedge the task.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Chunks read per tick before yielding back to the poll interval.
const MAX_CHUNKS_PER_TICK: usize = 20;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Resolved, validated sync configuration. `Debug` never prints the token.
#[derive(Debug, Clone)]
pub(crate) struct CloudSyncConfig {
    /// Cloud base URL without a trailing slash.
    pub(crate) base_url: String,
    token: BearerToken,
    /// This hub's identity: the `origin_hub` of everything it pushes.
    pub(crate) hub: String,
    pub(crate) state_path: PathBuf,
    pub(crate) outbox_cap: usize,
    pub(crate) batch_size: usize,
    pub(crate) poll_interval: Duration,
    pub(crate) presence_interval: Duration,
    /// Push the whole retained history on first run instead of only new writes.
    pub(crate) backfill: bool,
}

/// What [`resolve`] decided.
#[derive(Debug)]
pub(crate) enum Resolution {
    /// Neither a URL nor a token file is set: behave exactly as before.
    Disabled,
    /// Something was set but cannot be used; sync stays off and `/health`
    /// reports why.
    Invalid(String),
    /// Run the task.
    Enabled(Box<CloudSyncConfig>),
}

/// Decide whether cloud sync runs.
///
/// Enabled only when `cloud_url`, `cloud_token_file` and `hub_identity` are all
/// present and valid. A URL must be `https`, except for a loopback host (used
/// by tests and an SSH-forwarded dev cloud), and must not carry credentials.
/// The token file must be owner-only on Unix.
pub(crate) fn resolve(settings: &Settings, env: &dyn CredentialEnv) -> Resolution {
    if settings.cloud_url.is_none() && settings.cloud_token_file.is_none() {
        return Resolution::Disabled;
    }
    match build_config(settings, env) {
        Ok(config) => Resolution::Enabled(Box::new(config)),
        Err(reason) => Resolution::Invalid(reason),
    }
}

fn build_config(
    settings: &Settings,
    env: &dyn CredentialEnv,
) -> std::result::Result<CloudSyncConfig, String> {
    let url = settings
        .cloud_url
        .as_deref()
        .ok_or("cloud_url is required when cloud_token_file is set")?;
    let token_file = settings
        .cloud_token_file
        .as_deref()
        .ok_or("cloud_token_file is required when cloud_url is set")?;
    let hub = settings.hub_identity.clone().ok_or(
        "hub_identity is required for cloud sync: it is this hub's origin_hub and the \
         exclude_origin of every pull",
    )?;
    let base_url = validate_cloud_url(url)?;
    let token = read_private_token_file(token_file, env).map_err(|reason| reason.to_string())?;
    let state_path = match settings.cloud_sync_state_file.as_deref() {
        Some(path) => PathBuf::from(path),
        None => env
            .home_dir()
            .ok_or("cannot choose a cloud sync state file without a home directory")?
            .join(".config")
            .join("agent-bus")
            .join("cloud-sync-state.json"),
    };
    Ok(CloudSyncConfig {
        base_url,
        token,
        hub,
        state_path,
        outbox_cap: DEFAULT_OUTBOX_CAP,
        batch_size: MAX_BATCH,
        poll_interval: DEFAULT_POLL,
        presence_interval: DEFAULT_PRESENCE_POLL,
        backfill: env
            .var("AGENT_BUS_CLOUD_SYNC_BACKFILL")
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes")),
    })
}

/// Accept `https://…`, or `http://` only for a loopback host.
fn validate_cloud_url(raw: &str) -> std::result::Result<String, String> {
    let parsed =
        reqwest::Url::parse(raw.trim()).map_err(|_error| "cloud_url is not a valid URL")?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("cloud_url must not contain credentials".to_owned());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("cloud_url must not contain a query or fragment".to_owned());
    }
    let loopback = match parsed.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => return Err("cloud_url has no host".to_owned()),
    };
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("cloud_url must be https (http is allowed only for loopback)".to_owned()),
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

// ---------------------------------------------------------------------------
// Status (shared with /health)
// ---------------------------------------------------------------------------

/// Live sync status read by `/health`. Created even when the configuration is
/// invalid so the operator can see why sync is off.
#[derive(Debug, Default)]
pub(crate) struct CloudSyncStatus {
    reachable: AtomicBool,
    queue_depth: AtomicU64,
    /// Unix milliseconds of the last successful push; `0` means never.
    last_push_ms: AtomicI64,
    last_pull_ms: AtomicI64,
    dropped_batches: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl CloudSyncStatus {
    /// A status already carrying an error, for an invalid configuration.
    pub(crate) fn invalid(reason: &str) -> Self {
        let status = Self::default();
        status.record_error(reason);
        status
    }

    fn record_push_ok(&self, now_ms: i64) {
        self.last_push_ms.store(now_ms, Ordering::Relaxed);
        self.reachable.store(true, Ordering::Relaxed);
        self.clear_error();
    }

    fn record_pull_ok(&self, now_ms: i64) {
        self.last_pull_ms.store(now_ms, Ordering::Relaxed);
        self.reachable.store(true, Ordering::Relaxed);
        self.clear_error();
    }

    fn record_tick_ok(&self) {
        self.reachable.store(true, Ordering::Relaxed);
        self.clear_error();
    }

    fn record_error(&self, reason: &str) {
        self.reachable.store(false, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(truncate(reason, 300));
    }

    fn clear_error(&self) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The `cloud_*` block for `/health`.
    pub(crate) fn snapshot(&self, now_ms: i64) -> CloudHealth {
        let at = |ms: i64| {
            (ms > 0)
                .then(|| chrono::DateTime::from_timestamp_millis(ms))
                .flatten()
                .map(|time| time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        };
        let age = |ms: i64| (ms > 0).then(|| u64::try_from((now_ms - ms) / 1000).unwrap_or(0));
        let push = self.last_push_ms.load(Ordering::Relaxed);
        let pull = self.last_pull_ms.load(Ordering::Relaxed);
        CloudHealth {
            cloud_configured: true,
            cloud_reachable: self.reachable.load(Ordering::Relaxed),
            cloud_queue_depth: self.queue_depth.load(Ordering::Relaxed),
            cloud_last_push_at_utc: at(push),
            cloud_last_push_age_seconds: age(push),
            cloud_last_pull_at_utc: at(pull),
            cloud_last_pull_age_seconds: age(pull),
            cloud_dropped_batches_total: self.dropped_batches.load(Ordering::Relaxed),
            cloud_last_error: self
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Delay after `failures` consecutive failures: full jitter over an
/// exponentially growing ceiling (1, 2, 4, ... s), clamped to 1..=60 s.
/// `jitter_unit` is a uniform sample in `[0, 1)`.
pub(crate) fn backoff_delay(failures: u32, jitter_unit: f64) -> Duration {
    let ceiling = f64::from(1_u32 << failures.min(6)).min(60.0);
    Duration::from_secs_f64((ceiling * jitter_unit.clamp(0.0, 1.0)).clamp(1.0, 60.0))
}

fn jitter_unit() -> f64 {
    let bits = uuid::Uuid::new_v4().as_u128() >> 75;
    // 53 random bits to a uniform f64 in [0, 1).
    f64::from(u32::try_from(bits & 0xFFFF_FFFF).unwrap_or(0)) / 4_294_967_296.0
}

// ---------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------

/// Progress persisted across restarts. All three are acknowledged positions:
/// everything at or before them has been delivered (or deliberately skipped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncCursors {
    /// Last local stream id fully handled by the push side.
    pub(crate) push_stream_id: String,
    /// `next_cursor` of the last pulled page.
    pub(crate) pull_cursor: u64,
    /// Last local `presence_events.id` pushed.
    pub(crate) presence_origin_id: i64,
}

impl Default for SyncCursors {
    fn default() -> Self {
        Self {
            push_stream_id: "0-0".to_owned(),
            pull_cursor: 0,
            presence_origin_id: 0,
        }
    }
}

/// Load cursors; `None` when the file does not exist yet.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or parsed. A corrupt
/// state file is surfaced rather than silently restarting from zero, which
/// would re-push the entire history.
pub(crate) fn load_cursors(path: &Path) -> Result<Option<SyncCursors>> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("cloud sync state {} is corrupt", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Persist cursors atomically (temp file, then rename).
///
/// # Errors
/// Returns an error if the directory or file cannot be written.
pub(crate) fn save_cursors(path: &Path, cursors: &SyncCursors) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cursors)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One `/sync/push` item. The Worker's push body names the parties `sender` and
/// `recipient`, unlike the `from`/`to` of a stored or pulled message.
#[derive(Debug, Serialize)]
struct PushMessage<'a> {
    id: &'a str,
    timestamp_utc: &'a str,
    protocol_version: &'a str,
    sender: &'a str,
    recipient: &'a str,
    topic: &'a str,
    body: &'a str,
    tags: &'a [String],
    priority: &'a str,
    request_ack: bool,
    metadata: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_msg_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hlc: Option<&'a str>,
}

impl<'a> PushMessage<'a> {
    fn from_message(msg: &'a Message) -> Self {
        Self {
            id: &msg.id,
            timestamp_utc: &msg.timestamp_utc,
            protocol_version: &msg.protocol_version,
            sender: &msg.from,
            recipient: &msg.to,
            topic: &msg.topic,
            body: &msg.body,
            tags: msg.tags.as_slice(),
            priority: &msg.priority,
            request_ack: msg.request_ack,
            metadata: &msg.metadata,
            thread_id: msg.thread_id.as_deref(),
            // The local default fills `reply_to` with the sender; that is not
            // a real reply, so it is not replicated.
            reply_to: msg
                .reply_to
                .as_deref()
                .filter(|reply| !reply.is_empty() && *reply != msg.from),
            client_msg_id: msg.client_msg_id.as_deref(),
            origin_seq: msg.origin_seq,
            hlc: msg.hlc.as_deref(),
        }
    }
}

/// One `/sync/push-presence` event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PresenceEvent {
    origin_id: i64,
    timestamp_utc: String,
    protocol_version: String,
    agent: String,
    status: String,
    session_id: String,
    capabilities: Vec<String>,
    metadata: serde_json::Value,
    ttl_seconds: u64,
}

impl PresenceEvent {
    fn new(origin_id: i64, presence: Presence) -> Self {
        Self {
            origin_id,
            timestamp_utc: presence.timestamp_utc,
            protocol_version: presence.protocol_version,
            agent: presence.agent,
            status: presence.status,
            session_id: presence.session_id,
            capabilities: presence.capabilities,
            metadata: presence.metadata,
            ttl_seconds: presence.ttl_seconds,
        }
    }
}

/// What the Worker said about a push. Only counts matter to the engine;
/// per-item rejections are logged and never retried, because a rejection is a
/// verdict on the content, not a transient failure.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PushOutcome {
    pub(crate) accepted: usize,
    pub(crate) duplicates: usize,
    pub(crate) rejected: Vec<String>,
}

/// One page of `/sync/pull`.
#[derive(Debug)]
pub(crate) struct PullPage {
    pub(crate) messages: Vec<Message>,
    /// Items the page held that could not be parsed (skipped, not fatal).
    pub(crate) unparseable: usize,
    pub(crate) next_cursor: u64,
    pub(crate) has_more: bool,
}

/// A presence row from `GET /presence`: the stored shape plus `origin_hub`.
#[derive(Debug, Deserialize)]
pub(crate) struct WirePresence {
    #[serde(flatten)]
    pub(crate) presence: Presence,
    #[serde(default)]
    pub(crate) origin_hub: Option<String>,
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// The local store the engine reads from and ingests into.
pub(crate) trait LocalStore: Send + Sync + 'static {
    /// Newest stream id, or `"0-0"` for an empty stream.
    fn stream_tail(&self) -> impl Future<Output = Result<String>> + Send;
    /// Up to `max` messages strictly after `after`, oldest first.
    fn messages_after(
        &self,
        after: &str,
        max: usize,
    ) -> impl Future<Output = Result<Vec<Message>>> + Send;
    /// Highest local presence event id.
    fn presence_tail(&self) -> impl Future<Output = Result<i64>> + Send;
    /// Up to `max` presence events with id greater than `after`.
    fn presence_after(
        &self,
        after: i64,
        max: usize,
    ) -> impl Future<Output = Result<Vec<(i64, Presence)>>> + Send;
    /// Store a pulled message, idempotently.
    fn ingest_message(&self, msg: Message) -> impl Future<Output = Result<IngestOutcome>> + Send;
    /// Merge a pulled presence row; `true` if it was written.
    fn apply_presence(&self, presence: Presence) -> impl Future<Output = Result<bool>> + Send;
}

/// The cloud tier.
pub(crate) trait CloudApi: Send + Sync + 'static {
    fn push_messages(&self, batch: &[Message]) -> impl Future<Output = Result<PushOutcome>> + Send;
    fn push_presence(
        &self,
        events: &[PresenceEvent],
    ) -> impl Future<Output = Result<PushOutcome>> + Send;
    fn pull(
        &self,
        since: u64,
        exclude_origin: &str,
        limit: usize,
    ) -> impl Future<Output = Result<PullPage>> + Send;
    fn pull_presence(&self) -> impl Future<Output = Result<Vec<WirePresence>>> + Send;
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Whether `msg` may leave this hub.
///
/// Only locally originated messages are pushed (no `origin_hub`, or this
/// hub's), which is what stops a pulled message echoing back. Anything not
/// plainly `internal` stays on-site.
pub(crate) fn pushable(msg: &Message, hub: &str) -> bool {
    msg.origin_hub.as_deref().is_none_or(|origin| origin == hub)
        && msg.effective_sensitivity() == Sensitivity::Internal
}

/// The sync engine: cursors, outbox and the two seams.
pub(crate) struct Engine<L, C> {
    local: L,
    cloud: C,
    hub: String,
    state_path: Option<PathBuf>,
    batch_size: usize,
    outbox_cap: usize,
    presence_interval: Duration,
    status: Arc<CloudSyncStatus>,
    cursors: SyncCursors,
    /// Highest stream id loaded into the outbox or skipped; `>= push_stream_id`.
    read_to: String,
    /// Eligible messages loaded but not yet acknowledged, with their stream ids.
    outbox: VecDeque<(String, Message)>,
    last_presence_pull: Option<Instant>,
}

impl<L: LocalStore, C: CloudApi> Engine<L, C> {
    pub(crate) fn new(
        local: L,
        cloud: C,
        config: &CloudSyncConfig,
        status: Arc<CloudSyncStatus>,
        cursors: SyncCursors,
    ) -> Self {
        let batch_size = config.batch_size.clamp(1, MAX_BATCH);
        Self {
            local,
            cloud,
            hub: config.hub.clone(),
            state_path: Some(config.state_path.clone()),
            batch_size,
            // A bound below one batch could never make progress.
            outbox_cap: config.outbox_cap.max(batch_size),
            presence_interval: config.presence_interval,
            status,
            read_to: cursors.push_stream_id.clone(),
            cursors,
            outbox: VecDeque::new(),
            last_presence_pull: None,
        }
    }

    /// One pass: push messages, push presence, pull messages, pull presence.
    /// The first failure ends the pass; nothing is acknowledged past it.
    pub(crate) async fn tick(&mut self) -> Result<()> {
        self.push_messages().await?;
        self.push_presence().await?;
        self.pull_messages().await?;
        self.pull_presence().await?;
        self.status.record_tick_ok();
        Ok(())
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        if let Err(error) = save_cursors(path, &self.cursors) {
            // Not fatal: a lost cursor only means an idempotent re-push.
            tracing::warn!("cloud sync: could not persist cursors: {error:#}");
        }
    }

    fn set_depth(&self) {
        self.status
            .queue_depth
            .store(self.outbox.len() as u64, Ordering::Relaxed);
    }

    async fn push_messages(&mut self) -> Result<()> {
        for _ in 0..MAX_CHUNKS_PER_TICK {
            let chunk = self
                .local
                .messages_after(&self.read_to, self.batch_size)
                .await
                .context("reading the local stream")?;
            let full_chunk = chunk.len() >= self.batch_size;
            if !chunk.is_empty() {
                self.enqueue(chunk);
            }
            self.flush_outbox().await?;
            if !full_chunk {
                break;
            }
        }
        Ok(())
    }

    /// Load a chunk into the outbox, or drop the outbox and fall back to
    /// cursor catch-up if it would overflow.
    fn enqueue(&mut self, chunk: Vec<Message>) {
        let last_seen = chunk.last().and_then(|msg| msg.stream_id.clone());
        let eligible: Vec<(String, Message)> = chunk
            .into_iter()
            .filter(|msg| pushable(msg, &self.hub))
            .filter_map(|msg| msg.stream_id.clone().map(|id| (id, msg)))
            .collect();
        if self.outbox.len() + eligible.len() > self.outbox_cap {
            self.outbox.clear();
            self.read_to.clone_from(&self.cursors.push_stream_id);
            self.status.dropped_batches.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "cloud sync: outbox full ({} messages); dropping it and re-reading from {}",
                self.outbox_cap,
                self.cursors.push_stream_id
            );
        } else {
            self.outbox.extend(eligible);
            if let Some(last) = last_seen {
                self.read_to = last;
            }
        }
        self.set_depth();
    }

    async fn flush_outbox(&mut self) -> Result<()> {
        while !self.outbox.is_empty() {
            let take = self.outbox.len().min(self.batch_size);
            let batch: Vec<Message> = self
                .outbox
                .iter()
                .take(take)
                .map(|(_, msg)| msg.clone())
                .collect();
            let outcome = self
                .cloud
                .push_messages(&batch)
                .await
                .context("pushing messages")?;
            log_rejections("message", &outcome);
            let acked = self.outbox.drain(..take).next_back().map(|(id, _)| id);
            self.cursors.push_stream_id = if self.outbox.is_empty() {
                // Everything loaded is delivered or deliberately skipped.
                self.read_to.clone()
            } else {
                acked.unwrap_or_else(|| self.cursors.push_stream_id.clone())
            };
            self.persist();
            self.status.record_push_ok(now_ms());
            self.set_depth();
        }
        if self.cursors.push_stream_id != self.read_to {
            // Only ineligible entries since the last ack: nothing to send, but
            // the cursor still moves so they are not re-scanned forever.
            self.cursors.push_stream_id.clone_from(&self.read_to);
            self.persist();
        }
        Ok(())
    }

    async fn push_presence(&mut self) -> Result<()> {
        for _ in 0..MAX_CHUNKS_PER_TICK {
            let rows = self
                .local
                .presence_after(self.cursors.presence_origin_id, self.batch_size)
                .await
                .context("reading local presence events")?;
            let Some(max_id) = rows.iter().map(|(id, _)| *id).max() else {
                break;
            };
            let full = rows.len() >= self.batch_size;
            let events: Vec<PresenceEvent> = rows
                .into_iter()
                .map(|(id, presence)| PresenceEvent::new(id, presence))
                .collect();
            let outcome = self
                .cloud
                .push_presence(&events)
                .await
                .context("pushing presence")?;
            log_rejections("presence", &outcome);
            self.cursors.presence_origin_id = max_id;
            self.persist();
            self.status.record_push_ok(now_ms());
            if !full {
                break;
            }
        }
        Ok(())
    }

    async fn pull_messages(&mut self) -> Result<()> {
        loop {
            let page = self
                .cloud
                .pull(self.cursors.pull_cursor, &self.hub, PULL_PAGE)
                .await
                .context("pulling messages")?;
            if page.unparseable > 0 {
                tracing::warn!(
                    "cloud sync: skipped {} unparseable pulled message(s)",
                    page.unparseable
                );
            }
            for msg in page.messages {
                // Belt and braces: the Worker already excludes this hub's own
                // origin, and a message with no origin cannot be told apart
                // from a local one, so neither is ingested.
                match msg.origin_hub.as_deref() {
                    Some(origin) if origin != self.hub => {}
                    _ => continue,
                }
                self.local
                    .ingest_message(msg)
                    .await
                    .context("ingesting a pulled message")?;
            }
            self.cursors.pull_cursor = page.next_cursor.max(self.cursors.pull_cursor);
            self.persist();
            self.status.record_pull_ok(now_ms());
            if !page.has_more {
                return Ok(());
            }
        }
    }

    async fn pull_presence(&mut self) -> Result<()> {
        if self
            .last_presence_pull
            .is_some_and(|at| at.elapsed() < self.presence_interval)
        {
            return Ok(());
        }
        let rows = self
            .cloud
            .pull_presence()
            .await
            .context("pulling presence")?;
        for row in rows {
            let mut presence = row.presence;
            // Remote rows carry their origin in metadata so the local merge can
            // recognise (and drop) this hub's own announcements.
            if let Some(origin) = row.origin_hub {
                if !presence.metadata.is_object() {
                    presence.metadata = serde_json::json!({});
                }
                if let Some(map) = presence.metadata.as_object_mut() {
                    map.insert("origin_hub".to_owned(), origin.into());
                }
            }
            self.local
                .apply_presence(presence)
                .await
                .context("applying pulled presence")?;
        }
        self.last_presence_pull = Some(Instant::now());
        Ok(())
    }
}

fn log_rejections(kind: &str, outcome: &PushOutcome) {
    for reason in &outcome.rejected {
        tracing::warn!("cloud sync: cloud rejected a {kind}: {reason}");
    }
}

/// Run the engine until `shutdown` fires, backing off after failures.
pub(crate) async fn run<L: LocalStore, C: CloudApi>(
    mut engine: Engine<L, C>,
    poll_interval: Duration,
    shutdown: Arc<Notify>,
) {
    let mut failures: u32 = 0;
    let stopped = shutdown.notified();
    tokio::pin!(stopped);
    // Keep the broadcast waiter registered while a tick is doing I/O.
    stopped.as_mut().enable();
    loop {
        let outcome = tokio::select! {
            () = &mut stopped => return,
            outcome = engine.tick() => outcome,
        };
        let delay = match outcome {
            Ok(()) => {
                failures = 0;
                poll_interval
            }
            Err(error) => {
                engine.status.record_error(&format!("{error:#}"));
                tracing::warn!("cloud sync: {error:#}");
                let delay = backoff_delay(failures, jitter_unit());
                failures = failures.saturating_add(1);
                delay
            }
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = &mut stopped => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Real implementations
// ---------------------------------------------------------------------------

/// [`LocalStore`] over the hub's Redis pool, `PostgreSQL` and `PgWriter`.
#[derive(Clone)]
pub(crate) struct RedisLocal {
    settings: Arc<Settings>,
    pool: RedisPool,
    hub: String,
}

impl RedisLocal {
    pub(crate) fn new(settings: Arc<Settings>, pool: RedisPool, hub: String) -> Self {
        Self {
            settings,
            pool,
            hub,
        }
    }

    async fn blocking<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Settings, &RedisPool, &str) -> Result<T> + Send + 'static,
    {
        let settings = Arc::clone(&self.settings);
        let pool = self.pool.clone();
        let hub = self.hub.clone();
        tokio::task::spawn_blocking(move || work(&settings, &pool, &hub))
            .await
            .map_err(|error| anyhow!("blocking task failed: {error}"))?
    }
}

impl LocalStore for RedisLocal {
    async fn stream_tail(&self) -> Result<String> {
        self.blocking(|settings, pool, _| {
            let mut conn = pool.get_connection()?;
            let raw: Vec<redis::Value> = redis::cmd("XREVRANGE")
                .arg(&settings.stream_key)
                .arg("+")
                .arg("-")
                .arg("COUNT")
                .arg(1)
                .query(&mut *conn)?;
            Ok(agent_bus_core::redis_bus::parse_xrange_result(&raw)
                .into_iter()
                .next()
                .map_or_else(|| "0-0".to_owned(), |(id, _)| id))
        })
        .await
    }

    async fn messages_after(&self, after: &str, max: usize) -> Result<Vec<Message>> {
        let after = after.to_owned();
        self.blocking(move |settings, pool, _| {
            let mut conn = pool.get_connection()?;
            Ok(sync_store::read_messages_after(
                &mut conn, settings, &after, max,
            )?)
        })
        .await
    }

    async fn presence_tail(&self) -> Result<i64> {
        self.blocking(|settings, _, _| {
            Ok(agent_bus_core::postgres_store::presence_event_max_id(
                settings,
            )?)
        })
        .await
    }

    async fn presence_after(&self, after: i64, max: usize) -> Result<Vec<(i64, Presence)>> {
        self.blocking(move |settings, _, _| {
            Ok(agent_bus_core::postgres_store::list_presence_events_after(
                settings, after, max,
            )?)
        })
        .await
    }

    async fn ingest_message(&self, msg: Message) -> Result<IngestOutcome> {
        self.blocking(move |settings, pool, _| {
            let mut conn = pool.get_connection()?;
            Ok(sync_store::ingest_synced_message(
                &mut conn,
                settings,
                &msg,
                agent_bus_core::pg_writer(),
            )?)
        })
        .await
    }

    async fn apply_presence(&self, presence: Presence) -> Result<bool> {
        self.blocking(move |settings, pool, hub| {
            let mut conn = pool.get_connection()?;
            Ok(sync_store::apply_synced_presence(
                &mut conn,
                settings,
                &presence,
                hub,
                chrono::Utc::now(),
            )?)
        })
        .await
    }
}

/// [`CloudApi`] over HTTPS with a bearer token.
pub(crate) struct HttpCloud {
    client: reqwest::Client,
    base_url: String,
    token: BearerToken,
}

impl std::fmt::Debug for HttpCloud {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpCloud")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl HttpCloud {
    pub(crate) fn new(config: &CloudSyncConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(5))
                .build()
                .context("building the cloud HTTP client")?,
            base_url: config.base_url.clone(),
            token: config.token.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<serde_json::Value> {
        let response = request
            .bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(|error| anyhow!("request failed: {}", error.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("cloud answered HTTP {}", status.as_u16()));
        }
        response
            .json()
            .await
            .map_err(|error| anyhow!("cloud response was not JSON: {}", error.without_url()))
    }
}

fn outcome_from(body: &serde_json::Value) -> PushOutcome {
    let count = |key: &str| match body.get(key) {
        Some(serde_json::Value::Array(items)) => items.len(),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
        _ => 0,
    };
    let rejected = body
        .get("rejected")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("rejected")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    PushOutcome {
        accepted: count("accepted"),
        duplicates: count("duplicates"),
        rejected,
    }
}

impl CloudApi for HttpCloud {
    async fn push_messages(&self, batch: &[Message]) -> Result<PushOutcome> {
        let messages: Vec<PushMessage<'_>> = batch.iter().map(PushMessage::from_message).collect();
        let body = self
            .send(
                self.client
                    .post(self.url("/sync/push"))
                    .json(&serde_json::json!({ "messages": messages })),
            )
            .await?;
        Ok(outcome_from(&body))
    }

    async fn push_presence(&self, events: &[PresenceEvent]) -> Result<PushOutcome> {
        let body = self
            .send(
                self.client
                    .post(self.url("/sync/push-presence"))
                    .json(&serde_json::json!({ "events": events })),
            )
            .await?;
        Ok(outcome_from(&body))
    }

    async fn pull(&self, since: u64, exclude_origin: &str, limit: usize) -> Result<PullPage> {
        let mut url = reqwest::Url::parse(&self.url("/sync/pull"))
            .map_err(|error| anyhow!("bad pull URL: {error}"))?;
        url.query_pairs_mut()
            .append_pair("since", &since.to_string())
            .append_pair("exclude_origin", exclude_origin)
            .append_pair("limit", &limit.to_string());
        let body = self.send(self.client.get(url)).await?;
        let items = body
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut messages = Vec::with_capacity(items.len());
        let mut unparseable = 0;
        for item in items {
            match serde_json::from_value::<Message>(item) {
                Ok(msg) => messages.push(msg),
                Err(_) => unparseable += 1,
            }
        }
        Ok(PullPage {
            messages,
            unparseable,
            next_cursor: body
                .get("next_cursor")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(since),
            has_more: body
                .get("has_more")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    async fn pull_presence(&self) -> Result<Vec<WirePresence>> {
        let body = self.send(self.client.get(self.url("/presence"))).await?;
        Ok(body
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| serde_json::from_value(row.clone()).ok())
                    .collect()
            })
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// What [`start`] hands back to the server.
pub(crate) type SharedStatus = Option<Arc<CloudSyncStatus>>;

/// Start the sync task if configured. Returns the status handle `/health`
/// reads, or `None` when sync is disabled (the exact pre-sync behaviour).
///
/// Must run inside a Tokio runtime.
pub(crate) async fn start(
    settings: &Arc<Settings>,
    pool: &RedisPool,
    shutdown: &Arc<Notify>,
    env: &dyn CredentialEnv,
) -> SharedStatus {
    let config = match resolve(settings, env) {
        Resolution::Disabled => return None,
        Resolution::Invalid(reason) => {
            tracing::error!("cloud sync is configured but disabled: {reason}");
            return Some(Arc::new(CloudSyncStatus::invalid(&reason)));
        }
        Resolution::Enabled(config) => *config,
    };
    let status = Arc::new(CloudSyncStatus::default());
    let local = RedisLocal::new(Arc::clone(settings), pool.clone(), config.hub.clone());
    let cloud = match HttpCloud::new(&config) {
        Ok(cloud) => cloud,
        Err(error) => {
            let reason = format!("{error:#}");
            tracing::error!("cloud sync is configured but disabled: {reason}");
            return Some(Arc::new(CloudSyncStatus::invalid(&reason)));
        }
    };
    let cursors = match initial_cursors(&config, &local).await {
        Ok(cursors) => cursors,
        Err(error) => {
            let reason = format!("{error:#}");
            tracing::error!("cloud sync is configured but disabled: {reason}");
            return Some(Arc::new(CloudSyncStatus::invalid(&reason)));
        }
    };
    tracing::info!(
        "cloud sync enabled: hub {} -> {} (outbox cap {}, batch {})",
        config.hub,
        config.base_url,
        config.outbox_cap,
        config.batch_size
    );
    let poll_every = config.poll_interval;
    let engine = Engine::new(local, cloud, &config, Arc::clone(&status), cursors);
    tokio::spawn(run(engine, poll_every, Arc::clone(shutdown)));
    Some(status)
}

/// Persisted cursors, or on first run the current tail (unless backfill was
/// requested) so enabling sync does not push the whole retained history.
async fn initial_cursors<L: LocalStore>(
    config: &CloudSyncConfig,
    local: &L,
) -> Result<SyncCursors> {
    if let Some(existing) = load_cursors(&config.state_path)? {
        return Ok(existing);
    }
    let cursors = if config.backfill {
        SyncCursors::default()
    } else {
        SyncCursors {
            push_stream_id: local.stream_tail().await?,
            pull_cursor: 0,
            presence_origin_id: local.presence_tail().await?,
        }
    };
    save_cursors(&config.state_path, &cursors)?;
    Ok(cursors)
}

/// Current wall-clock time in Unix milliseconds, for [`CloudSyncStatus::snapshot`].
pub(crate) fn snapshot_now() -> i64 {
    now_ms()
}

/// Disposable-backend addressing shared with the core crate's backend tests:
/// URLs come only from `AGENT_BUS_TEST_*`, and the live bus ports are refused.
#[cfg(test)]
#[path = "../../agent-bus-core/tests/support/backend_env.rs"]
mod backend_env;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};
    use std::sync::atomic::AtomicUsize;

    use axum::extract::{Query, State};
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    // ----- test doubles ----------------------------------------------------

    fn msg(n: u64, from: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": format!("id-{n}"),
            "timestamp_utc": "2026-01-01T00:00:00.000000Z",
            "protocol_version": "1.0",
            "from": from, "to": "all", "topic": "status", "body": format!("body {n}"),
            "stream_id": format!("{n}-0"),
        }))
        .unwrap()
    }

    #[derive(Default)]
    struct LocalInner {
        stream: Vec<Message>,
        presence: Vec<(i64, Presence)>,
        ingested: Vec<Message>,
        seen: HashSet<(String, String)>,
        applied_presence: Vec<Presence>,
    }

    /// In-memory store. A plain `std` mutex, never held across an `.await`.
    #[derive(Clone, Default)]
    struct MemLocal(Arc<Mutex<LocalInner>>);

    impl MemLocal {
        fn append(&self, m: Message) {
            self.0.lock().unwrap().stream.push(m);
        }
        fn stream_len(&self) -> usize {
            self.0.lock().unwrap().stream.len()
        }
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "an in-memory double needs no await"
    )]
    impl LocalStore for MemLocal {
        async fn stream_tail(&self) -> Result<String> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .stream
                .last()
                .and_then(|m| m.stream_id.clone())
                .unwrap_or_else(|| "0-0".to_owned()))
        }
        async fn messages_after(&self, after: &str, max: usize) -> Result<Vec<Message>> {
            let key = |id: &str| id.split('-').next().unwrap().parse::<u64>().unwrap();
            let after = key(after);
            Ok(self
                .0
                .lock()
                .unwrap()
                .stream
                .iter()
                .filter(|m| key(m.stream_id.as_deref().unwrap()) > after)
                .take(max)
                .cloned()
                .collect())
        }
        async fn presence_tail(&self) -> Result<i64> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .presence
                .iter()
                .map(|(id, _)| *id)
                .max()
                .unwrap_or(0))
        }
        async fn presence_after(&self, after: i64, max: usize) -> Result<Vec<(i64, Presence)>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .presence
                .iter()
                .filter(|(id, _)| *id > after)
                .take(max)
                .cloned()
                .collect())
        }
        async fn ingest_message(&self, mut m: Message) -> Result<IngestOutcome> {
            let mut inner = self.0.lock().unwrap();
            let key = (m.origin_hub.clone().unwrap(), m.id.clone());
            if !inner.seen.insert(key) {
                return Ok(IngestOutcome::Duplicate);
            }
            // Like the real store: the entry lands in the stream, with its
            // origin, under a fresh local stream id.
            m.stream_id = Some(format!("{}-0", 1_000 + inner.stream.len() as u64));
            inner.stream.push(m.clone());
            inner.ingested.push(m);
            Ok(IngestOutcome::Ingested)
        }
        async fn apply_presence(&self, p: Presence) -> Result<bool> {
            self.0.lock().unwrap().applied_presence.push(p);
            Ok(true)
        }
    }

    #[derive(Default)]
    struct CloudInner {
        pushed: Vec<Vec<String>>,
        pushed_presence: Vec<Vec<i64>>,
        pages: VecDeque<PullPage>,
        pulls: Vec<(u64, String)>,
        presence_rows: Vec<(Presence, Option<String>)>,
    }

    #[derive(Clone, Default)]
    struct MockCloud {
        inner: Arc<Mutex<CloudInner>>,
        failing: Arc<AtomicBool>,
        hang: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        hung_call_started: Arc<Notify>,
    }

    impl MockCloud {
        fn pushed_ids(&self) -> Vec<String> {
            self.inner
                .lock()
                .unwrap()
                .pushed
                .iter()
                .flatten()
                .cloned()
                .collect()
        }
        fn gate(&self) -> impl std::future::Future<Output = Result<()>> + Send + use<> {
            let (failing, hang) = (
                self.failing.load(Ordering::SeqCst),
                self.hang.load(Ordering::SeqCst),
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            let hung_call_started = Arc::clone(&self.hung_call_started);
            async move {
                if hang {
                    hung_call_started.notify_one();
                    std::future::pending::<()>().await;
                }
                if failing {
                    return Err(anyhow!("cloud answered HTTP 503"));
                }
                Ok(())
            }
        }
    }

    impl CloudApi for MockCloud {
        async fn push_messages(&self, batch: &[Message]) -> Result<PushOutcome> {
            self.gate().await?;
            self.inner
                .lock()
                .unwrap()
                .pushed
                .push(batch.iter().map(|m| m.id.clone()).collect());
            Ok(PushOutcome::default())
        }
        async fn push_presence(&self, events: &[PresenceEvent]) -> Result<PushOutcome> {
            self.gate().await?;
            self.inner
                .lock()
                .unwrap()
                .pushed_presence
                .push(events.iter().map(|e| e.origin_id).collect());
            Ok(PushOutcome::default())
        }
        async fn pull(&self, since: u64, exclude: &str, _limit: usize) -> Result<PullPage> {
            self.gate().await?;
            let mut inner = self.inner.lock().unwrap();
            inner.pulls.push((since, exclude.to_owned()));
            Ok(inner.pages.pop_front().unwrap_or(PullPage {
                messages: vec![],
                unparseable: 0,
                next_cursor: since,
                has_more: false,
            }))
        }
        async fn pull_presence(&self) -> Result<Vec<WirePresence>> {
            self.gate().await?;
            Ok(self
                .inner
                .lock()
                .unwrap()
                .presence_rows
                .iter()
                .map(|(presence, origin)| WirePresence {
                    presence: presence.clone(),
                    origin_hub: origin.clone(),
                })
                .collect())
        }
    }

    fn config(dir: &Path, batch: usize, cap: usize) -> CloudSyncConfig {
        CloudSyncConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            token: BearerToken::new("test-token"),
            hub: "hub-a".to_owned(),
            state_path: dir.join("state.json"),
            outbox_cap: cap,
            batch_size: batch,
            poll_interval: Duration::from_millis(10),
            presence_interval: Duration::ZERO,
            backfill: false,
        }
    }

    fn engine(
        dir: &Path,
        local: &MemLocal,
        cloud: &MockCloud,
        batch: usize,
        cap: usize,
    ) -> (Engine<MemLocal, MockCloud>, Arc<CloudSyncStatus>) {
        let status = Arc::new(CloudSyncStatus::default());
        let engine = Engine::new(
            local.clone(),
            cloud.clone(),
            &config(dir, batch, cap),
            Arc::clone(&status),
            SyncCursors::default(),
        );
        (engine, status)
    }

    fn presence(agent: &str) -> Presence {
        Presence {
            agent: agent.to_owned(),
            status: "online".to_owned(),
            protocol_version: "1.0".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            session_id: "s".to_owned(),
            capabilities: vec![],
            metadata: serde_json::json!({}),
            ttl_seconds: 300,
        }
    }

    // ----- configuration ---------------------------------------------------

    struct FakeEnv {
        home: PathBuf,
        vars: Vec<(String, String)>,
    }
    impl CredentialEnv for FakeEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
        fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
            std::fs::read_to_string(path)
        }
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
    }

    fn settings(url: Option<&str>, token: Option<&Path>, hub: Option<&str>) -> Settings {
        let mut s = Settings::from_env();
        s.cloud_url = url.map(str::to_owned);
        s.cloud_token_file = token.map(|p| p.display().to_string());
        s.hub_identity = hub.map(str::to_owned);
        s
    }

    #[cfg(unix)]
    fn restrict(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(not(unix))]
    fn restrict(_path: &Path, _mode: u32) {}

    fn token_file(dir: &Path, mode: u32) -> PathBuf {
        let path = dir.join("cloud.token");
        std::fs::write(&path, "tok-ABC123\n").unwrap();
        restrict(&path, mode);
        path
    }

    fn fake_env(dir: &Path) -> FakeEnv {
        FakeEnv {
            home: dir.to_path_buf(),
            vars: vec![],
        }
    }

    #[test]
    fn unset_configuration_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(None, None, Some("hub-a"));
        assert!(matches!(
            resolve(&s, &fake_env(dir.path())),
            Resolution::Disabled
        ));
    }

    #[test]
    fn a_half_configured_pair_is_invalid_not_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let tok = token_file(dir.path(), 0o600);
        for s in [
            settings(Some("https://cloud.example"), None, Some("hub-a")),
            settings(None, Some(&tok), Some("hub-a")),
        ] {
            assert!(matches!(
                resolve(&s, &fake_env(dir.path())),
                Resolution::Invalid(_)
            ));
        }
    }

    #[test]
    fn hub_identity_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let tok = token_file(dir.path(), 0o600);
        let s = settings(Some("https://cloud.example"), Some(&tok), None);
        let Resolution::Invalid(reason) = resolve(&s, &fake_env(dir.path())) else {
            panic!("expected invalid");
        };
        assert!(reason.contains("hub_identity"), "{reason}");
    }

    #[test]
    fn https_is_required_except_for_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let tok = token_file(dir.path(), 0o600);
        let enabled = |url: &str| {
            matches!(
                resolve(
                    &settings(Some(url), Some(&tok), Some("hub-a")),
                    &fake_env(dir.path())
                ),
                Resolution::Enabled(_)
            )
        };
        assert!(enabled("https://cloud.example"));
        assert!(enabled("http://127.0.0.1:8787"));
        assert!(enabled("http://localhost:8787"));
        assert!(!enabled("http://cloud.example"));
        assert!(!enabled("http://192.0.2.5:8787"));
        assert!(!enabled("https://user:pw@cloud.example"));
        assert!(!enabled("https://cloud.example/?x=1"));
        assert!(!enabled("not a url"));
    }

    #[cfg(unix)]
    #[test]
    fn a_token_file_readable_by_others_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let tok = token_file(dir.path(), 0o644);
        let s = settings(Some("https://cloud.example"), Some(&tok), Some("hub-a"));
        let Resolution::Invalid(reason) = resolve(&s, &fake_env(dir.path())) else {
            panic!("expected invalid");
        };
        assert!(reason.contains("chmod 600"), "{reason}");
        assert!(
            !reason.contains("tok-ABC123"),
            "the token must never be echoed"
        );
    }

    #[test]
    fn the_token_never_appears_in_debug_output() {
        let dir = tempfile::tempdir().unwrap();
        let tok = token_file(dir.path(), 0o600);
        let s = settings(Some("https://cloud.example"), Some(&tok), Some("hub-a"));
        let Resolution::Enabled(cfg) = resolve(&s, &fake_env(dir.path())) else {
            panic!("expected enabled");
        };
        assert!(!format!("{cfg:?}").contains("tok-ABC123"));
        assert!(!format!("{:?}", HttpCloud::new(&cfg).unwrap()).contains("tok-ABC123"));
        assert_eq!(cfg.base_url, "https://cloud.example");
        assert!(cfg.state_path.ends_with("cloud-sync-state.json"));
        assert!(!cfg.backfill);
    }

    // ----- backoff and cursors --------------------------------------------

    #[test]
    fn backoff_is_one_to_sixty_seconds_for_every_attempt_and_jitter() {
        for failures in 0..=40 {
            for j in [0.0, 0.01, 0.5, 0.999, 1.0] {
                let d = backoff_delay(failures, j);
                assert!(
                    d >= Duration::from_secs(1) && d <= Duration::from_secs(60),
                    "failures={failures} jitter={j} gave {d:?}"
                );
            }
        }
        assert_eq!(
            backoff_delay(0, 0.7),
            Duration::from_secs(1),
            "first retry waits 1s"
        );
        assert_eq!(
            backoff_delay(30, 1.0),
            Duration::from_secs(60),
            "ceiling is 60s"
        );
        assert!(
            backoff_delay(4, 1.0) > backoff_delay(1, 1.0),
            "the ceiling grows"
        );
    }

    #[test]
    fn cursors_round_trip_and_a_corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.json");
        assert_eq!(load_cursors(&path).unwrap(), None);
        let cursors = SyncCursors {
            push_stream_id: "9-1".into(),
            pull_cursor: 4,
            presence_origin_id: 7,
        };
        save_cursors(&path, &cursors).unwrap();
        assert_eq!(load_cursors(&path).unwrap(), Some(cursors));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(
            load_cursors(&path).is_err(),
            "must not silently restart from zero"
        );
    }

    #[tokio::test]
    async fn first_run_starts_at_the_tail_unless_backfill_is_requested() {
        let dir = tempfile::tempdir().unwrap();
        let local = MemLocal::default();
        for n in 1..=3 {
            local.append(msg(n, "alice"));
        }
        local
            .0
            .lock()
            .unwrap()
            .presence
            .push((11, presence("alice")));
        let mut cfg = config(dir.path(), 500, 5000);
        let tail = initial_cursors(&cfg, &local).await.unwrap();
        assert_eq!(tail.push_stream_id, "3-0");
        assert_eq!(tail.presence_origin_id, 11);
        // A second start resumes from the persisted file, not from the tail.
        local.append(msg(4, "alice"));
        assert_eq!(initial_cursors(&cfg, &local).await.unwrap(), tail);
        std::fs::remove_file(&cfg.state_path).unwrap();
        cfg.backfill = true;
        assert_eq!(
            initial_cursors(&cfg, &local).await.unwrap(),
            SyncCursors::default()
        );
    }

    // ----- push ------------------------------------------------------------

    #[tokio::test]
    async fn push_sends_local_messages_in_order_and_persists_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        for n in 1..=5 {
            local.append(msg(n, "alice"));
        }
        let (mut e, status) = engine(dir.path(), &local, &cloud, 2, 100);
        e.tick().await.unwrap();
        assert_eq!(cloud.pushed_ids(), ["id-1", "id-2", "id-3", "id-4", "id-5"]);
        assert!(
            cloud
                .inner
                .lock()
                .unwrap()
                .pushed
                .iter()
                .all(|b| b.len() <= 2)
        );
        assert_eq!(
            load_cursors(&dir.path().join("state.json"))
                .unwrap()
                .unwrap()
                .push_stream_id,
            "5-0"
        );
        let health = status.snapshot(now_ms());
        assert!(health.cloud_reachable && health.cloud_last_push_at_utc.is_some());
        assert_eq!(health.cloud_queue_depth, 0);
        // Nothing new: nothing is sent again.
        e.tick().await.unwrap();
        assert_eq!(cloud.pushed_ids().len(), 5);
    }

    #[tokio::test]
    async fn restart_resumes_from_the_persisted_cursor_without_resending() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        for n in 1..=3 {
            local.append(msg(n, "alice"));
        }
        let (mut first, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        first.tick().await.unwrap();
        local.append(msg(4, "alice"));
        let cursors = load_cursors(&dir.path().join("state.json"))
            .unwrap()
            .unwrap();
        let status = Arc::new(CloudSyncStatus::default());
        let mut second = Engine::new(
            local.clone(),
            cloud.clone(),
            &config(dir.path(), 500, 5000),
            status,
            cursors,
        );
        second.tick().await.unwrap();
        assert_eq!(cloud.pushed_ids(), ["id-1", "id-2", "id-3", "id-4"]);
    }

    #[tokio::test]
    async fn no_offsite_messages_are_never_pushed_but_the_cursor_moves_past_them() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        let mut by_field = msg(1, "alice");
        by_field.sensitivity = Some(Sensitivity::NoOffsite);
        let mut by_meta = msg(2, "alice");
        by_meta.metadata = serde_json::json!({"sensitivity": "no-offsite"});
        let mut by_tag = msg(3, "alice");
        by_tag.tags.push("sensitivity:no-offsite".to_owned());
        let mut unknown = msg(4, "alice");
        unknown.metadata = serde_json::json!({"sensitivity": "top-secret"});
        for m in [by_field, by_meta, by_tag, unknown, msg(5, "alice")] {
            local.append(m);
        }
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        assert_eq!(cloud.pushed_ids(), ["id-5"]);
        assert_eq!(
            e.cursors.push_stream_id, "5-0",
            "skipped entries are not rescanned"
        );
        e.tick().await.unwrap();
        assert_eq!(cloud.pushed_ids(), ["id-5"]);
    }

    #[tokio::test]
    async fn a_trailing_run_of_no_offsite_messages_still_advances_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        local.append(msg(1, "alice"));
        let mut hidden = msg(2, "alice");
        hidden.sensitivity = Some(Sensitivity::NoOffsite);
        local.append(hidden);
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        assert_eq!(e.cursors.push_stream_id, "2-0");
    }

    #[tokio::test]
    async fn pulled_messages_keep_their_origin_and_are_never_pushed_back() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        local.append(msg(1, "alice"));
        let mut remote = msg(900, "roamer");
        remote.origin_hub = Some("cloud".to_owned());
        remote.stream_id = None;
        cloud.inner.lock().unwrap().pages.push_back(PullPage {
            messages: vec![remote],
            unparseable: 0,
            next_cursor: 42,
            has_more: false,
        });
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        // The pull asked the cloud to exclude this hub's own origin.
        assert_eq!(
            cloud.inner.lock().unwrap().pulls[0],
            (0, "hub-a".to_owned())
        );
        let ingested = local.0.lock().unwrap().ingested.clone();
        assert_eq!(ingested.len(), 1);
        assert_eq!(ingested[0].origin_hub.as_deref(), Some("cloud"));
        // A second pass must not push the ingested message: no echo.
        e.tick().await.unwrap();
        e.tick().await.unwrap();
        assert_eq!(cloud.pushed_ids(), ["id-1"]);
        assert_eq!(e.cursors.pull_cursor, 42);
        assert_eq!(
            load_cursors(&dir.path().join("state.json"))
                .unwrap()
                .unwrap()
                .pull_cursor,
            42
        );
    }

    #[tokio::test]
    async fn pull_replays_are_idempotent_and_never_ingest_this_hubs_own_messages() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        let mut remote = msg(900, "roamer");
        remote.origin_hub = Some("cloud".to_owned());
        let mut own = msg(901, "alice");
        own.origin_hub = Some("hub-a".to_owned());
        let mut unattributed = msg(902, "alice");
        unattributed.origin_hub = None;
        let page = || PullPage {
            messages: vec![remote.clone(), own.clone(), unattributed.clone()],
            unparseable: 0,
            next_cursor: 7,
            has_more: false,
        };
        cloud.inner.lock().unwrap().pages.extend([page(), page()]);
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        e.cursors.pull_cursor = 0; // simulate a lost cursor: the same page again
        e.tick().await.unwrap();
        let inner = local.0.lock().unwrap();
        assert_eq!(
            inner.ingested.len(),
            1,
            "ingested once despite two deliveries"
        );
        assert_eq!(inner.ingested[0].id, "id-900");
    }

    #[tokio::test]
    async fn pull_follows_has_more_across_pages() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        let page = |n: u64, cursor: u64, more: bool| {
            let mut m = msg(n, "roamer");
            m.origin_hub = Some("cloud".to_owned());
            PullPage {
                messages: vec![m],
                unparseable: 0,
                next_cursor: cursor,
                has_more: more,
            }
        };
        cloud.inner.lock().unwrap().pages.extend([
            page(1, 10, true),
            page(2, 20, true),
            page(3, 30, false),
        ]);
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        assert_eq!(local.0.lock().unwrap().ingested.len(), 3);
        let pulls: Vec<u64> = cloud
            .inner
            .lock()
            .unwrap()
            .pulls
            .iter()
            .map(|p| p.0)
            .collect();
        assert_eq!(pulls, [0, 10, 20]);
    }

    // ----- overflow, failure, backoff -------------------------------------

    #[tokio::test]
    async fn a_full_outbox_is_dropped_and_recovered_by_cursor_catch_up() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        for n in 1..=10 {
            local.append(msg(n, "alice"));
        }
        cloud.failing.store(true, Ordering::SeqCst);
        // Batches of 2 with room for only 4 queued messages.
        let (mut e, status) = engine(dir.path(), &local, &cloud, 2, 4);
        for _ in 0..4 {
            assert!(e.tick().await.is_err(), "the cloud is down");
        }
        let health = status.snapshot(now_ms());
        assert!(
            health.cloud_dropped_batches_total >= 1,
            "the outbox overflowed"
        );
        assert!(e.outbox.len() <= 4, "memory stays bounded");
        assert!(!health.cloud_reachable);
        assert_eq!(e.cursors.push_stream_id, "0-0", "nothing was acknowledged");
        // The cloud comes back: every message arrives, in order, none lost.
        cloud.failing.store(false, Ordering::SeqCst);
        for _ in 0..6 {
            e.tick().await.unwrap();
        }
        let ids = cloud.pushed_ids();
        let expected: Vec<String> = (1..=10).map(|n| format!("id-{n}")).collect();
        assert_eq!(ids, expected);
        assert_eq!(e.cursors.push_stream_id, "10-0");
    }

    #[tokio::test(start_paused = true)]
    async fn failures_back_off_instead_of_busy_looping() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        local.append(msg(1, "alice"));
        cloud.failing.store(true, Ordering::SeqCst);
        let (e, status) = engine(dir.path(), &local, &cloud, 500, 5000);
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(run(e, Duration::from_millis(10), Arc::clone(&shutdown)));
        tokio::time::sleep(Duration::from_secs(120)).await;
        let attempts = cloud.calls.load(Ordering::SeqCst);
        assert!(attempts >= 3, "it keeps retrying ({attempts})");
        assert!(
            attempts <= 40,
            "but with backoff, not a busy loop ({attempts})"
        );
        let health = status.snapshot(now_ms());
        assert!(!health.cloud_reachable);
        assert_eq!(
            health
                .cloud_last_error
                .as_deref()
                .map(|e| e.contains("503")),
            Some(true)
        );
        // Recovery clears the error and the push goes through.
        cloud.failing.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(70)).await;
        assert_eq!(cloud.pushed_ids(), ["id-1"]);
        assert!(status.snapshot(now_ms()).cloud_reachable);
        assert!(status.snapshot(now_ms()).cloud_last_error.is_none());
        shutdown.notify_waiters();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_a_cloud_call_that_never_returns() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        local.append(msg(1, "alice"));
        cloud.hang.store(true, Ordering::SeqCst);
        let (e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(run(e, Duration::from_millis(10), Arc::clone(&shutdown)));
        tokio::time::timeout(Duration::from_secs(5), cloud.hung_call_started.notified())
            .await
            .expect("the engine never entered the blocked cloud call");

        // Broadcast while tick is still pending, before its backoff waiter.
        shutdown.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown was lost while a cloud call was pending")
            .unwrap();
    }

    #[tokio::test]
    async fn local_writes_are_not_slowed_while_the_cloud_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        local.append(msg(1, "alice"));
        cloud.hang.store(true, Ordering::SeqCst);
        let (e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(run(e, Duration::from_millis(5), Arc::clone(&shutdown)));
        // Wait until the engine is actually stuck inside the cloud call.
        let deadline = Instant::now() + Duration::from_secs(5);
        while cloud.calls.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "the engine never reached the cloud"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Local writes proceed at full speed; if the engine held the store
        // lock across its hung call, these would block forever.
        let started = Instant::now();
        let writers = local.clone();
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                for n in 2..=2_000 {
                    writers.append(msg(n, "alice"));
                }
            }),
        )
        .await
        .expect("local writes were blocked by a hung cloud")
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(local.stream_len(), 2_000);
        task.abort();
    }

    // ----- presence --------------------------------------------------------

    #[tokio::test]
    async fn presence_is_pushed_once_and_the_cursor_advances() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        local
            .0
            .lock()
            .unwrap()
            .presence
            .extend([(5, presence("a")), (6, presence("b"))]);
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        e.tick().await.unwrap();
        assert_eq!(cloud.inner.lock().unwrap().pushed_presence, [vec![5, 6]]);
        assert_eq!(e.cursors.presence_origin_id, 6);
    }

    #[tokio::test]
    async fn pulled_presence_carries_its_origin_into_the_local_merge() {
        let dir = tempfile::tempdir().unwrap();
        let (local, cloud) = (MemLocal::default(), MockCloud::default());
        cloud
            .inner
            .lock()
            .unwrap()
            .presence_rows
            .push((presence("roamer"), Some("cloud".into())));
        let (mut e, _) = engine(dir.path(), &local, &cloud, 500, 5000);
        e.tick().await.unwrap();
        let applied = local.0.lock().unwrap().applied_presence.clone();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].metadata["origin_hub"], "cloud");
        // Applying pulled presence does not create local presence history, so
        // it can never be pushed back.
        e.tick().await.unwrap();
        assert!(cloud.inner.lock().unwrap().pushed_presence.is_empty());
    }

    // ----- health ----------------------------------------------------------

    #[test]
    fn invalid_configuration_is_visible_in_health() {
        let health = CloudSyncStatus::invalid("hub_identity is required").snapshot(now_ms());
        assert!(health.cloud_configured && !health.cloud_reachable);
        assert_eq!(
            health.cloud_last_error.as_deref(),
            Some("hub_identity is required")
        );
        assert!(health.cloud_last_push_at_utc.is_none());
    }

    #[test]
    fn snapshot_reports_ages_from_the_supplied_clock() {
        let status = CloudSyncStatus::default();
        status.record_push_ok(1_000_000);
        status.record_pull_ok(1_030_000);
        let health = status.snapshot(1_090_000);
        assert_eq!(health.cloud_last_push_age_seconds, Some(90));
        assert_eq!(health.cloud_last_pull_age_seconds, Some(60));
        assert_eq!(
            health.cloud_last_push_at_utc.as_deref(),
            Some("1970-01-01T00:16:40.000Z")
        );
    }

    // ----- real HTTP client against a mock Worker --------------------------

    #[derive(Default)]
    struct Seen {
        paths: Vec<String>,
        push_bodies: Vec<serde_json::Value>,
        authed: Vec<bool>,
        pull_queries: Vec<std::collections::HashMap<String, String>>,
    }
    type SharedSeen = Arc<Mutex<Seen>>;

    fn note(seen: &SharedSeen, path: &str, headers: &HeaderMap) {
        let mut s = seen.lock().unwrap();
        s.paths.push(path.to_owned());
        s.authed.push(
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer test-token"),
        );
    }

    async fn mock_worker() -> (String, SharedSeen) {
        let seen: SharedSeen = Arc::default();

        let app = Router::new()
            .route(
                "/sync/push",
                post(|State(seen): State<SharedSeen>, headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    let ids: Vec<_> = body["messages"].as_array().unwrap().iter().map(|m| m["id"].clone()).collect();
                    let mut s = seen.lock().unwrap();
                    s.paths.push("/sync/push".into());
                    s.authed.push(headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer test-token"));
                    s.push_bodies.push(body);
                    Json(serde_json::json!({"accepted": ids, "duplicates": [], "conflicts": [],
                        "rejected": [{"id": "x", "reason": "too long"}], "cursor": 1}))
                }),
            )
            .route(
                "/sync/push-presence",
                post(|State(seen): State<SharedSeen>, headers: HeaderMap, Json(_b): Json<serde_json::Value>| async move {
                    let mut s = seen.lock().unwrap();
                    s.paths.push("/sync/push-presence".into());
                    s.authed.push(headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer test-token"));
                    Json(serde_json::json!({"accepted": 1, "duplicates": 0, "rejected": []}))
                }),
            )
            .route(
                "/sync/pull",
                get(|State(seen): State<SharedSeen>, headers: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    seen.lock().unwrap().pull_queries.push(q);
                    note(&seen, "/sync/pull", &headers);
                    Json(serde_json::json!({
                        "messages": [
                            {"id": "r1", "timestamp_utc": "2026-01-01T00:00:00.000Z", "protocol_version": "1.0",
                             "from": "roamer", "to": "all", "topic": "status", "body": "hi", "tags": [],
                             "priority": "normal", "request_ack": false, "metadata": null, "origin_hub": "cloud",
                             "origin_host": "laptop"},
                            {"id": "bad", "from": 5}
                        ],
                        "next_cursor": 3, "has_more": false
                    }))
                }),
            )
            .route(
                "/presence",
                get(|State(seen): State<SharedSeen>, headers: HeaderMap| async move {
                    note(&seen, "/presence", &headers);
                    Json(serde_json::json!([{
                        "agent": "roamer", "status": "online", "protocol_version": "1.0",
                        "timestamp_utc": "2026-01-01T00:00:00.000Z", "session_id": "s",
                        "capabilities": [], "metadata": null, "ttl_seconds": 300,
                        "origin_hub": "cloud", "network_context": "offsite"
                    }]))
                }),
            )
            .with_state(Arc::clone(&seen));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn http_cloud(base: &str) -> HttpCloud {
        let dir = std::env::temp_dir();
        let mut cfg = config(&dir, 500, 5000);
        cfg.base_url = base.to_owned();
        HttpCloud::new(&cfg).unwrap()
    }

    #[tokio::test]
    async fn the_http_client_speaks_the_workers_wire_format_and_never_touches_claims() {
        let (base, seen) = mock_worker().await;
        let dir = tempfile::tempdir().unwrap();
        let local = MemLocal::default();
        let mut m = msg(1, "alice");
        m.reply_to = Some("alice".to_owned()); // the local default, not a real reply
        m.tags.push("repo:x".to_owned());
        local.append(m);
        local
            .0
            .lock()
            .unwrap()
            .presence
            .push((3, presence("alice")));
        let status = Arc::new(CloudSyncStatus::default());
        let mut e = Engine::new(
            local.clone(),
            http_cloud(&base),
            &config(dir.path(), 500, 5000),
            Arc::clone(&status),
            SyncCursors::default(),
        );
        e.tick().await.unwrap();

        let s = seen.lock().unwrap();
        let allowed: BTreeSet<&str> = [
            "/sync/push",
            "/sync/push-presence",
            "/sync/pull",
            "/presence",
        ]
        .into();
        assert!(
            s.paths.iter().all(|p| allowed.contains(p.as_str())),
            "unexpected routes {:?}; claims must never be proxied",
            s.paths
        );
        assert!(
            s.paths
                .iter()
                .all(|p| !p.contains("arbitrate") && !p.contains("claim"))
        );
        assert!(
            s.authed.iter().all(|ok| *ok),
            "every request carries the bearer token"
        );
        let item = &s.push_bodies[0]["messages"][0];
        assert_eq!(item["sender"], "alice", "push uses sender/recipient");
        assert_eq!(item["recipient"], "all");
        assert!(item.get("from").is_none() && item.get("to").is_none());
        assert!(
            item.get("stream_id").is_none(),
            "local stream ids stay local"
        );
        assert!(
            item.get("reply_to").is_none(),
            "the sender-default reply_to is dropped"
        );
        assert!(
            item.get("origin_hub").is_none(),
            "the Worker fills origin_hub from the token"
        );
        assert_eq!(item["tags"], serde_json::json!(["repo:x"]));
        assert_eq!(s.pull_queries[0]["exclude_origin"], "hub-a");
        assert_eq!(s.pull_queries[0]["since"], "0");
        drop(s);

        // The pulled message was ingested; the unparseable one was skipped.
        let inner = local.0.lock().unwrap();
        assert_eq!(inner.ingested.len(), 1);
        assert_eq!(inner.ingested[0].origin_hub.as_deref(), Some("cloud"));
        assert_eq!(inner.applied_presence[0].metadata["origin_hub"], "cloud");
    }

    #[tokio::test]
    async fn an_unreachable_cloud_is_an_error_that_does_not_leak_the_token() {
        let err = http_cloud("http://127.0.0.1:1")
            .push_messages(&[msg(1, "alice")])
            .await
            .unwrap_err();
        assert!(!format!("{err:#}").contains("test-token"));
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error() {
        let app = Router::new().fallback(|| async { axum::http::StatusCode::UNAUTHORIZED });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let err = http_cloud(&format!("http://{addr}"))
            .pull_presence()
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("401"));
    }

    // ----- backend: the real Redis store end to end ------------------------

    /// The whole loop on a real Redis: local writes are pushed, a pulled
    /// message lands in the stream with its origin and is never pushed back,
    /// and replaying the pull writes nothing twice. Needs a DISPOSABLE Redis
    /// in `AGENT_BUS_TEST_REDIS_URL`; fails, never skips, when unset.
    #[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (see tests/support/backend_env.rs)"]
    #[tokio::test]
    async fn redis_backed_round_trip_has_no_echo_and_no_duplicates() {
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let mut settings = Settings::from_env();
        settings.redis_url = backend_env::backend_url(backend_env::REDIS_URL_VAR);
        settings.database_url = None;
        settings.stream_key = format!("agent_bus_test:cloudsync:{tag}:messages");
        settings.presence_prefix = format!("agent_bus_test:cloudsync:{tag}:presence:");
        let settings = Arc::new(settings);
        let pool = RedisPool::new(&settings).expect("disposable Redis unreachable");
        {
            let mut conn = pool.get_connection().unwrap();
            for (n, metadata) in [
                serde_json::json!({}),
                serde_json::json!({"sensitivity": "no-offsite"}),
                serde_json::json!({}),
            ]
            .into_iter()
            .enumerate()
            {
                agent_bus_core::redis_bus::bus_post_message(
                    &mut conn,
                    &settings,
                    "alice",
                    "all",
                    "status",
                    &format!("local message {n}"),
                    None,
                    &[],
                    "normal",
                    false,
                    None,
                    &metadata,
                    None,
                    false,
                )
                .unwrap();
            }
        }

        let (base, seen) = mock_worker().await;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), 500, 5000);
        cfg.base_url = base;
        let local = RedisLocal::new(Arc::clone(&settings), pool.clone(), "hub-a".to_owned());
        let status = Arc::new(CloudSyncStatus::default());
        let mut e = Engine::new(
            local,
            HttpCloud::new(&cfg).unwrap(),
            &cfg,
            status,
            SyncCursors::default(),
        );
        e.tick().await.unwrap();

        let pushed: Vec<String> = {
            let s = seen.lock().unwrap();
            s.push_bodies
                .iter()
                .flat_map(|b| b["messages"].as_array().unwrap().iter())
                .map(|m| m["body"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            pushed,
            ["local message 0", "local message 2"],
            "no-offsite stays home"
        );

        let mut conn = pool.get_connection().unwrap();
        let stream = sync_store::read_messages_after(&mut conn, &settings, "0-0", 100).unwrap();
        let pulled: Vec<_> = stream.iter().filter(|m| m.id == "r1").collect();
        assert_eq!(pulled.len(), 1, "the pulled message was ingested once");
        assert_eq!(pulled[0].origin_hub.as_deref(), Some("cloud"));

        // More ticks and a replayed pull: nothing is pushed again, nothing duplicated.
        e.cursors.pull_cursor = 0;
        e.tick().await.unwrap();
        e.tick().await.unwrap();
        let after = sync_store::read_messages_after(&mut conn, &settings, "0-0", 100).unwrap();
        assert_eq!(
            after.len(),
            stream.len(),
            "a replayed pull wrote nothing new"
        );
        let pushed_again = seen.lock().unwrap().push_bodies.len();
        assert_eq!(pushed_again, 1, "r1 was not echoed back to the cloud");

        let _: () = redis::cmd("DEL")
            .arg(&settings.stream_key)
            .arg(sync_store::ingest_seen_key("cloud", "r1"))
            .query(&mut *conn)
            .unwrap();
    }
}
