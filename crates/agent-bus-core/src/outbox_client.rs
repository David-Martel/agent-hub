//! Ordered client submission scoped to one configured on-site hub.
//!
//! The existing candidate credential provider is checked before journal bytes.
//! Shared bearer access proves hub access, not ownership of the agent argument.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::error::{AgentBusError, Result};
use crate::hub_cache::candidates_fingerprint;
use crate::hub_candidates::{HubAuth, HubRole, SystemEnv};
use crate::outbox::{
    ClientSurface, Journal, JournalInspection, Operation, OutboxError, ReplayFailure,
    ReplayRequest, ReplayResponse, ReplayTransport,
};
use crate::settings::Settings;

/// Native transports implement one bounded, authenticated replay attempt.
pub trait NativeReplayTransport {
    /// Replay using the selected guarded credential provider.
    ///
    /// # Errors
    /// Transport ambiguity or permanent authorization/shape rejection.
    fn replay(
        &self,
        url: &str,
        hub_identity: &str,
        request: &ReplayRequest,
        timeout: Duration,
    ) -> std::result::Result<ReplayResponse, ReplayFailure>;
}

struct HubTransport<'a, T> {
    transport: &'a T,
    routes: Vec<String>,
    hub_identity: String,
    deadline: Instant,
}
impl<T: NativeReplayTransport> ReplayTransport for HubTransport<'_, T> {
    fn replay(
        &self,
        request: &ReplayRequest,
    ) -> std::result::Result<ReplayResponse, ReplayFailure> {
        for url in &self.routes {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ReplayFailure::Transport);
            }
            match self.transport.replay(
                url,
                &self.hub_identity,
                request,
                remaining.min(Duration::from_secs(3)),
            ) {
                Err(ReplayFailure::Transport) => {}
                result => return result,
            }
        }
        Err(ReplayFailure::Transport)
    }
}

fn now_ms() -> Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_sanitized| {
                AgentBusError::Internal("system clock precedes durable epoch".to_owned())
            })?
            .as_millis(),
    )
    .map_err(|_sanitized| AgentBusError::Internal("system clock outside durable range".to_owned()))
}

/// Whether the selected named authority has a configured credential source.
///
/// This examines configuration metadata only. Explicit file/env providers and
/// even invalid configured global tokens must fail visibly at use time; they
/// never fall through to the unauthenticated legacy path.
#[must_use]
pub fn durable_replay_configured(settings: &Settings) -> bool {
    use crate::hub_candidates::CandidateAuth;
    let candidates = settings.effective_hub_candidates();
    let Some(primary) = candidates
        .iter()
        .find(|candidate| candidate.role == HubRole::Authoritative)
    else {
        return false;
    };
    let Some(hub) = primary.hub.as_ref().filter(|hub| !hub.is_empty()) else {
        return false;
    };
    candidates
        .iter()
        .filter(|candidate| {
            candidate.role == HubRole::Authoritative && candidate.hub.as_ref() == Some(hub)
        })
        .any(|candidate| {
            !matches!(candidate.auth, CandidateAuth::Global) || settings.auth_token.is_some()
        })
}

/// Refuse legacy unauthenticated new writes ahead of retained journal debt.
///
/// Unnamed/local/Cloud paths are unchanged. Absence never creates custody;
/// existing state must validate its privacy and exact authority scope.
///
/// # Errors
/// Pending debt, inaccessible/corrupt/private state or changed authority.
pub fn legacy_write_guard(settings: &Settings, path_override: Option<&Path>) -> Result<()> {
    legacy_write_guard_with(settings, || status(settings, path_override))
}

/// The guard's decision over a caller-supplied status read (a test seam).
fn legacy_write_guard_with(
    settings: &Settings,
    read_status: impl FnOnce() -> Result<Value>,
) -> Result<()> {
    let candidates = settings.effective_hub_candidates();
    if candidates
        .iter()
        .find(|candidate| candidate.role == HubRole::Authoritative)
        .is_none_or(|candidate| candidate.hub.as_ref().is_none_or(String::is_empty))
    {
        return Ok(());
    }
    let state = read_status()?;
    if state["pending"].as_u64().unwrap_or(1) != 0 {
        return Err(AgentBusError::InvalidParams(
            "retained durable requests require credentials; preserve journal before new submission"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Derive the journal location without creating or changing filesystem state.
///
/// # Errors
/// Missing current-user directory.
pub fn journal_path(_scope: &str) -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    .ok_or_else(|| AgentBusError::InvalidParams("user outbox root is unavailable".to_owned()))?;
    Ok(journal_path_in(&base))
}

fn journal_path_in(base: &Path) -> PathBuf {
    base.join("agent-bus-outbox").join("journal.jsonl")
}

fn journal_present(path: &Path) -> Result<bool> {
    crate::outbox_private::plain_path(path).map_err(|_sanitized| {
        AgentBusError::Internal("durable outbox path unavailable; preserve journal".to_owned())
    })?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_sanitized) => Err(AgentBusError::Internal(
            "durable outbox metadata unavailable; preserve journal".to_owned(),
        )),
    }
}

/// Open or create the active journal without racing a migration.
///
/// The directory lock is held only for this attempt: it finishes an interrupted
/// migration first, then opens (or creates) the journal, whose own lock then
/// protects it. A migration holds the same lock across its two renames, so a
/// writer waits instead of creating a second active journal in between.
fn open_active(path: &Path, scope: &str) -> std::result::Result<Journal, OutboxError> {
    let _directory = lock_directory_until(path, Instant::now())?;
    if let Err(error) = recover_interrupted_migration(path) {
        tracing::error!(%error, "interrupted outbox migration could not be completed");
        return Err(OutboxError::Custody);
    }
    Journal::open(path, scope)
}

/// Retry busy journals without scheduling waits beyond the original deadline.
fn open_journal_until(
    path: &Path,
    scope: &str,
    deadline: Instant,
) -> std::result::Result<Journal, OutboxError> {
    open_journal_until_with(path, scope, deadline, Instant::now, std::thread::sleep)
}

fn open_journal_until_with(
    path: &Path,
    scope: &str,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> std::result::Result<Journal, OutboxError> {
    let mut delay = Duration::from_millis(10);
    let jitter = Duration::from_millis(u64::from(uuid::Uuid::new_v4().as_bytes()[0] % 17));
    loop {
        // Always acquire after a capped wait, even if the scheduler woke late:
        // the previous writer may have released custody during that wait.
        match open_active(path, scope) {
            Err(OutboxError::Busy) => {
                let remaining = deadline.saturating_duration_since(now());
                if remaining.is_zero() {
                    return Err(OutboxError::Busy);
                }
                sleep((delay + jitter).min(remaining));
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            opened => return opened,
        }
    }
}

/// Submit a retained request, then flush in durable order.
///
/// An offline result is explicitly queued, with a stable request ID and pending
/// count; it is never presented as a sent message, ACK or granted lease.
/// Invalid/auth/unsupported responses surface errors and do not survive as retry.
///
/// # Errors
/// Invalid providers/arguments, private storage, lock contention or permanent
/// rejection. Existing retained requests are preserved when configuration moves.
pub fn submit<T: NativeReplayTransport>(
    settings: &Settings,
    transport: &T,
    operation: Operation,
    surface: ClientSurface,
    arguments: Map<String, Value>,
    path_override: Option<&Path>,
) -> Result<Value> {
    let (scope, route) = replay_context(settings, transport)?;
    let path = if let Some(path) = path_override {
        path.to_owned()
    } else {
        let path = journal_path(&scope)?;
        let parent = path.parent().ok_or_else(|| {
            AgentBusError::Internal("private outbox parent is unavailable".to_owned())
        })?;
        // Only a new submission establishes the default private leaf. An
        // explicit path retains the caller's existing parent and its ACLs.
        crate::outbox_private::create_directory(parent).map_err(|_sanitized| {
            AgentBusError::Internal("private outbox directory could not be established".to_owned())
        })?;
        path
    };
    // `open_journal_until` finishes an interrupted migration under the directory
    // lock before it opens or creates the active journal.
    let mut journal = match open_journal_until(&path, &scope, route.deadline) {
        // Editing the candidate list rebinds a journal that owes nothing to
        // its previous destination; retained requests still refuse below.
        Err(OutboxError::ScopeChanged) => {
            retire_previous_scope(&path, &scope, now_ms()?, RetirePolicy::SettledOnly)?;
            open_journal_until(&path, &scope, route.deadline)
        }
        opened => opened,
    }
    .map_err(|error| match error {
        OutboxError::Busy => AgentBusError::Internal(
            "durable outbox is busy; no new request was retained; retry after the current writer finishes"
                .to_owned(),
        ),
        _sanitized => {
            AgentBusError::Internal("private durable outbox could not be opened".to_owned())
        }
    })?;
    submit_journal(
        &mut journal,
        &route,
        operation,
        surface,
        arguments,
        now_ms()?,
    )
}

fn replay_context<'a, T: NativeReplayTransport>(
    settings: &Settings,
    transport: &'a T,
) -> Result<(String, HubTransport<'a, T>)> {
    if let Some(error) = &settings.hub_config_error {
        return Err(AgentBusError::InvalidParams(error.clone()));
    }
    let candidates = settings.effective_hub_candidates();
    let primary = candidates
        .iter()
        .find(|candidate| candidate.role == HubRole::Authoritative)
        .ok_or_else(|| {
            AgentBusError::InvalidParams(
                "durable replay requires configured on-site authority".to_owned(),
            )
        })?;
    let hub = primary
        .hub
        .as_ref()
        .filter(|hub| !hub.is_empty())
        .ok_or_else(|| {
            AgentBusError::InvalidParams(
                "durable replay requires explicit candidate hub identity".to_owned(),
            )
        })?;
    let auth = HubAuth::new(settings.auth_token.clone(), candidates.clone());
    let routes = candidates
        .iter()
        .filter(|candidate| {
            candidate.role == HubRole::Authoritative && candidate.hub.as_ref() == Some(hub)
        })
        .map(|candidate| {
            match auth.credential_for_url(
                &format!("{}/replay", candidate.url.trim_end_matches('/')),
                &SystemEnv,
            ) {
                Ok(Some(_)) => Ok(candidate.url.clone()),
                _ => Err(AgentBusError::InvalidParams(
                    "durable replay credential provider refused".to_owned(),
                )),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let scope = candidates_fingerprint(&candidates);
    // Scope fingerprints contain configuration source metadata, never token
    // bytes; route changes do not silently move an existing queue to Cloud.

    Ok((
        scope,
        HubTransport {
            transport,
            routes,
            hub_identity: hub.clone(),
            deadline: Instant::now() + Duration::from_secs(12),
        },
    ))
}

/// Age beyond which a retained request is reported as stale (a dead-letter candidate).
///
/// Sends and acknowledgments never expire by age, by design: a request queued
/// during a long outage must still be delivered. Staleness is therefore only
/// reported, in [`status`], so an operator can see work that has waited a week
/// and decide; nothing is dropped or rewritten because of it.
const STALE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// Prefix of a journal written in full beside the active one during a migration.
const STAGING_PREFIX: &str = "journal.migrating-";

/// Prefix of the old journal once a migration has archived it.
///
/// Its existence is the commit point of a migration: a staging journal is
/// promoted only when the archive for the same origin exists, so a staging file
/// left by an attempt that never got that far is never mistaken for one that did.
/// An ordinary retirement uses `journal.retired-` and does not count.
const MIGRATED_PREFIX: &str = "journal.migrated-";

/// Serializes changes to the journal's directory entry across processes.
///
/// Held while a migration moves files and while a writer finishes an interrupted
/// migration and opens or creates the active journal, so a writer can never
/// create a fresh active journal in the gap between a migration's two renames.
const DIRECTORY_LOCK: &str = "journal.lock";

/// Inspect stable pending IDs without submitting a replacement request.
///
/// Read-only apart from the shared journal lock it holds while reading. Each
/// request reports its age and whether a replay was ever attempted; the summary
/// counts the unsent (`never_attempted`), the unsettled attempted
/// (`attempted_unsettled`) and the stale (`stale`, older than `stale_after_ms`).
/// When the journal is bound to another candidate list the report says why and
/// names the supported recovery in `recovery`. After a migration stopped between
/// its renames the report describes the staged journal, which holds the debt,
/// and names it in `interrupted_migration`.
///
/// # Errors
/// Invalid destination or private journal custody, corruption or contention.
pub fn status(settings: &Settings, path_override: Option<&Path>) -> Result<Value> {
    status_with_hooks(settings, path_override, || {}, || {})
}

#[cfg(test)]
fn status_with_hook(
    settings: &Settings,
    path_override: Option<&Path>,
    before_inspect: impl FnMut(),
) -> Result<Value> {
    status_with_hooks(settings, path_override, || {}, before_inspect)
}

/// `status` with two test seams: `after_first_check` runs right after the first
/// presence check and `before_inspect` right before the journal is read.
fn status_with_hooks(
    settings: &Settings,
    path_override: Option<&Path>,
    mut after_first_check: impl FnMut(),
    mut before_inspect: impl FnMut(),
) -> Result<Value> {
    let scope = candidates_fingerprint(&settings.effective_hub_candidates());
    let path = match path_override {
        Some(path) => path.to_owned(),
        None => journal_path(&scope)?,
    };
    // A migration can vacate the active path between the presence check and the
    // read. The read never creates the file, so it reports the miss and we look
    // again, now seeing the staged journal the migration left.
    for _ in 0..3 {
        if let Some(report) =
            status_once(&scope, &path, &mut after_first_check, &mut before_inspect)?
        {
            return Ok(report);
        }
    }
    Err(AgentBusError::Internal(
        "durable outbox changed repeatedly while reading status; retry".to_owned(),
    ))
}

/// One look at the journal; `None` when it vanished between check and read.
fn status_once(
    scope: &str,
    path: &Path,
    after_first_check: &mut impl FnMut(),
    before_inspect: &mut impl FnMut(),
) -> Result<Option<Value>> {
    let present_at_first_look = journal_present(path)?;
    after_first_check();
    let interrupted = if present_at_first_look {
        None
    } else {
        committed_staging(path)?.into_iter().next()
    };
    let (source, interrupted_name) = match &interrupted {
        Some(staging) => (staging.as_path(), Some(file_name_text(staging))),
        None if journal_present(path)? => (path, None),
        // The journal was there at the first look and gone at the second: a
        // migration committed in between, and the staged journal that now holds
        // its debt was never looked for. Look again instead of reporting empty.
        None if present_at_first_look => return Ok(None),
        None => {
            let mut report = json!({"pending":0,"requests":[]});
            add_stale_staging(&mut report, path)?;
            return Ok(Some(report));
        }
    };
    before_inspect();
    let inspection = match Journal::inspect(source) {
        Ok(inspection) => inspection,
        Err(OutboxError::Missing) => return Ok(None),
        Err(_sanitized) => {
            return Err(AgentBusError::Internal(
                "private durable outbox status unavailable; preserve journal".to_owned(),
            ));
        }
    };
    let now = now_ms()?;
    let attempted = inspection
        .pending
        .iter()
        .filter(|request| inspection.was_attempted(request))
        .count();
    let stale = inspection
        .pending
        .iter()
        .filter(|request| request_age_ms(request, now) >= STALE_AFTER_MS)
        .count();
    let mut report = json!({"pending":inspection.pending.len(),
        "never_attempted":inspection.pending.len() - attempted,
        "attempted_unsettled":attempted,
        "stale":stale,"stale_after_ms":STALE_AFTER_MS,
        "oldest_age_ms":inspection.pending.iter().map(|request| request_age_ms(request, now)).max(),
        "requests":inspection.pending.iter().map(|request| {
            let mut summary = request_summary(request);
            summary["age_ms"] = json!(request_age_ms(request, now));
            summary["attempted"] = json!(inspection.was_attempted(request));
            summary["stale"] = json!(request_age_ms(request, now) >= STALE_AFTER_MS);
            summary
        }).collect::<Vec<_>>()});
    if let Some(name) = interrupted_name {
        report["interrupted_migration"] = json!(name);
    }
    if let Some(stored) = inspection
        .destination
        .as_deref()
        .filter(|stored| *stored != scope)
    {
        // Scope fingerprints hash configuration metadata, never token bytes.
        let rotation = if inspection.pending.is_empty() {
            "automatic"
        } else if inspection
            .pending
            .iter()
            .all(|request| inspection.can_retire_expired(request, now))
        {
            "explicit"
        } else {
            "blocked"
        };
        report["scope_mismatch"] = json!(true);
        report["bound_scope_prefix"] = json!(scope_prefix(stored));
        report["current_scope_prefix"] = json!(scope_prefix(scope));
        report["rotation"] = json!(rotation);
        if !inspection.pending.is_empty() {
            report["recovery"] = json!(if attempted == 0 {
                "agent-bus outbox migrate"
            } else {
                "agent-bus outbox migrate --include-attempted"
            });
        }
    }
    add_stale_staging(&mut report, path)?;
    Ok(Some(report))
}

/// Name staging journals that no committed migration owns, without touching them.
fn add_stale_staging(report: &mut Value, path: &Path) -> Result<()> {
    let committed = committed_staging(path)?;
    let stale = staging_journals(path)?
        .into_iter()
        .filter(|staging| !committed.contains(staging))
        .map(|staging| file_name_text(&staging))
        .collect::<Vec<_>>();
    if !stale.is_empty() {
        report["stale_staging"] = json!(stale);
    }
    Ok(())
}

fn request_age_ms(request: &ReplayRequest, now: u64) -> u64 {
    now.saturating_sub(request.created_ms)
}

fn file_name_text(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Staging journals left beside `path` by migrations, committed or not, in name order.
fn staging_journals(path: &Path) -> Result<Vec<PathBuf>> {
    let Some(directory) = path.parent() else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_sanitized) => {
            return Err(AgentBusError::Internal(
                "durable outbox directory unavailable; preserve journal".to_owned(),
            ));
        }
    };
    let mut found = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| {
            let name = file_name_text(candidate);
            name.starts_with(STAGING_PREFIX)
                && Path::new(&name)
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                && !name.contains(".partial.")
        })
        .collect::<Vec<_>>();
    found.sort();
    Ok(found)
}

/// The origin a staging journal was written for, from its name.
fn staging_origin(staging: &Path) -> Option<String> {
    file_name_text(staging)
        .strip_prefix(STAGING_PREFIX)
        .and_then(|name| name.strip_suffix(".jsonl"))
        .map(str::to_owned)
}

/// Staging journals whose migration reached its commit point (the archive exists).
fn committed_staging(path: &Path) -> Result<Vec<PathBuf>> {
    Ok(staging_journals(path)?
        .into_iter()
        .filter(|staging| {
            staging_origin(staging).is_some_and(|origin| {
                std::fs::symlink_metadata(
                    path.with_file_name(format!("{MIGRATED_PREFIX}{origin}.jsonl")),
                )
                .is_ok()
            })
        })
        .collect())
}

/// Hold the directory lock, waiting until `deadline` for another holder.
fn lock_directory_until(
    path: &Path,
    deadline: Instant,
) -> std::result::Result<DirectoryLock, OutboxError> {
    let lock_path = path.with_file_name(DIRECTORY_LOCK);
    let mut delay = Duration::from_millis(10);
    loop {
        let file =
            crate::outbox_private::open(&lock_path).map_err(|_sanitized| OutboxError::Custody)?;
        match file.try_lock() {
            Ok(()) => return Ok(DirectoryLock(file)),
            Err(std::fs::TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(OutboxError::Busy);
                }
                std::thread::sleep(delay.min(remaining));
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(OutboxError::Io(error)),
        }
    }
}

/// An exclusive lock on the journal directory's namespace, released on drop.
struct DirectoryLock(std::fs::File);
impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn request_summary(request: &ReplayRequest) -> Value {
    json!({"request_id":request.request_id,"operation":request.operation,
        "created_ms":request.created_ms,"expires_ms":request.expires_ms})
}

fn scope_prefix(scope: &str) -> &str {
    scope.get(..16).unwrap_or(scope)
}

#[derive(Clone, Copy)]
enum RetirePolicy {
    /// Only a journal with no retained requests (automatic, on write).
    SettledOnly,
    /// Also requests that expired without an attempted replay (explicit action).
    ExpiredOnly,
}

/// Retire a journal bound to a previous candidate list so a new one can bind.
///
/// The old file is renamed beside the new one, never rewritten or deleted.
/// Retained requests that could still matter refuse the move.
fn retire_previous_scope(
    path: &Path,
    scope: &str,
    now: u64,
    policy: RetirePolicy,
) -> Result<Value> {
    retire_previous_scope_with_hook(path, scope, now, policy, |_| {})
}

fn retire_previous_scope_with_hook(
    path: &Path,
    scope: &str,
    now: u64,
    policy: RetirePolicy,
    before_move: impl FnOnce(&Path),
) -> Result<Value> {
    let inspection = Journal::inspect(path).map_err(|_sanitized| {
        AgentBusError::Internal("durable outbox inspection failed; preserve journal".to_owned())
    })?;
    let (Some(stored), Some(origin_id)) = (inspection.destination.as_deref(), inspection.origin_id)
    else {
        return Ok(json!({"rotated":false,"reason":"journal has no bound destination"}));
    };
    if stored == scope {
        return Ok(
            json!({"rotated":false,"reason":"journal already bound to current candidates",
            "pending":inspection.pending.len()}),
        );
    }
    let blocking = inspection
        .pending
        .iter()
        .filter(|request| match policy {
            RetirePolicy::SettledOnly => true,
            RetirePolicy::ExpiredOnly => !inspection.can_retire_expired(request, now),
        })
        .map(|request| request.request_id.to_string())
        .collect::<Vec<_>>();
    if !blocking.is_empty() {
        return Err(AgentBusError::InvalidParams(format!(
            "outbox journal is bound to a previous hub candidate list (scope {}) and retains {} \
             request(s) that are unexpired or await replay settlement: {}; run `agent-bus outbox \
             migrate --dry-run` to see what each would become (unsent requests move to the \
             current candidates, attempted ones are archived only on request), or replay them \
             under the previous configuration with `agent-bus outbox flush`",
            scope_prefix(stored),
            blocking.len(),
            blocking.join(", ")
        )));
    }
    let retired = path.with_file_name(format!("journal.retired-{origin_id}.jsonl"));
    crate::outbox_private::plain_path(&retired).map_err(|_sanitized| {
        AgentBusError::Internal("retired outbox path unavailable; preserve journal".to_owned())
    })?;
    if std::fs::symlink_metadata(&retired).is_ok() {
        return Err(AgentBusError::Internal(
            "retired outbox journal already exists; preserve both journals".to_owned(),
        ));
    }
    before_move(&retired);
    inspection.retire_to(path, &retired).map_err(|_sanitized| {
        AgentBusError::Internal(
            "outbox journal could not be retired (in use?); preserve journal".to_owned(),
        )
    })?;
    Ok(
        json!({"rotated":true,"retired_path":retired.display().to_string(),
        "bound_scope_prefix":scope_prefix(stored),"current_scope_prefix":scope_prefix(scope),
        "retired_requests":inspection.pending.iter().map(request_summary).collect::<Vec<_>>()}),
    )
}

/// Explicitly retire a journal bound to a previous candidate list.
///
/// Allowed when every retained request expired without attempted replay.
/// Sends, ACKs and unresolved attempted requests refuse; matching scopes stay.
///
/// # Errors
/// Unexpired/attempted requests, custody/corruption, contention or rename failure.
pub fn rotate(settings: &Settings, path_override: Option<&Path>, now: u64) -> Result<Value> {
    let scope = candidates_fingerprint(&settings.effective_hub_candidates());
    let path = match path_override {
        Some(path) => path.to_owned(),
        None => journal_path(&scope)?,
    };
    if !journal_present(&path)? {
        return Ok(json!({"rotated":false,"reason":"no journal"}));
    }
    retire_previous_scope(&path, &scope, now, RetirePolicy::ExpiredOnly)
}

/// [`rotate`] at the current time.
///
/// # Errors
/// As [`rotate`], or a clock outside the durable range.
pub fn rotate_now(settings: &Settings) -> Result<Value> {
    rotate(settings, None, now_ms()?)
}

/// What `outbox migrate` may do with retained requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrateOptions {
    /// Report the plan and change nothing.
    pub dry_run: bool,
    /// Also archive requests whose replay was attempted and never settled.
    ///
    /// Such a request may have been accepted by the previous authority, so it
    /// is archived with a report and never re-sent.
    pub include_attempted: bool,
    /// Archive unsent requests too instead of carrying them to the new journal.
    pub archive_only: bool,
}

/// What happens to one retained request during a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Unsent and still live: queued again, same request ID, in the new journal.
    Carry,
    /// Unsent presence or claim whose own deadline passed: nothing left to do.
    Expired,
    /// Unsent but archived on request (`archive_only`).
    Archive,
    /// Attempted and unsettled: archived with a report because the operator said so.
    Abandon,
    /// Attempted and unsettled: the operator has not said so, so nothing moves.
    Blocked,
}

fn fate(
    inspection: &JournalInspection,
    request: &ReplayRequest,
    now: u64,
    options: MigrateOptions,
) -> Fate {
    if inspection.was_attempted(request) {
        if options.include_attempted {
            Fate::Abandon
        } else {
            Fate::Blocked
        }
    } else if request.expires_ms != 0 && now >= request.expires_ms {
        Fate::Expired
    } else if options.archive_only {
        Fate::Archive
    } else {
        Fate::Carry
    }
}

/// Identify a request in a report without its body, tags or other payload.
fn report_entry(request: &ReplayRequest, now: u64) -> Value {
    let mut entry = request_summary(request);
    entry["age_ms"] = json!(request_age_ms(request, now));
    for key in ["topic", "thread_id", "agent"] {
        if let Some(value) = request.arguments.get(key) {
            entry[key] = value.clone();
        }
    }
    entry
}

/// Move a journal bound to another hub candidate list onto the current one.
///
/// This is the supported recovery when the destination changed (a different
/// authority, or any candidate edit) while requests were still retained and
/// the previous configuration can no longer replay them. It never replays
/// anything itself, and it preserves every byte of the old journal.
///
/// Under one exclusive lock on the old journal:
/// 1. each retained request is classified. A request whose replay was never
///    attempted cannot have had any effect on the previous authority, so it is
///    safe to send to the new one: it is carried over with its original request
///    ID. A request that was attempted and left unsettled might have been
///    accepted, and the new authority cannot recognise it. Those refuse the
///    migration unless `include_attempted` says to archive them, and are
///    never re-sent;
/// 2. the carried requests are written in full to a staging journal bound to
///    the current candidates;
/// 3. the old journal is renamed to `journal.migrated-<origin>.jsonl`, never
///    rewritten or deleted. This is the commit point;
/// 4. the staging journal is renamed into place.
///
/// The directory lock (`journal.lock`) is held throughout, and writers take it
/// to open or create the active journal, so none can create one in the gap
/// between steps 3 and 4. A stop in that gap leaves the complete, committed
/// staging journal beside the archive; the next write, flush or migrate
/// finishes the move, and status describes it meanwhile. A staging journal
/// without its archive was never committed and is never promoted.
///
/// The report lists what was carried, expired and abandoned by request ID,
/// operation, topic, thread and age. It never contains a body.
///
/// # Errors
/// An attempted unsettled request without `include_attempted`, a journal that
/// is already current or unbound, custody or corruption, lock contention, an
/// existing archive, or a failed rename. Failures leave the old journal as it
/// was, or, after step 3, both files.
pub fn migrate(
    settings: &Settings,
    path_override: Option<&Path>,
    options: MigrateOptions,
    now: u64,
) -> Result<Value> {
    migrate_with_hook(settings, path_override, options, now, || Ok(()))
}

/// [`migrate`] at the current time.
///
/// # Errors
/// As [`migrate`], or a clock outside the durable range.
pub fn migrate_now(settings: &Settings, options: MigrateOptions) -> Result<Value> {
    migrate(settings, None, options, now_ms()?)
}

// One locked sequence; splitting it would hide the order that makes it crash-safe.
#[allow(clippy::too_many_lines)]
fn migrate_with_hook(
    settings: &Settings,
    path_override: Option<&Path>,
    options: MigrateOptions,
    now: u64,
    after_archive: impl FnOnce() -> Result<()>,
) -> Result<Value> {
    let scope = candidates_fingerprint(&settings.effective_hub_candidates());
    let path = match path_override {
        Some(path) => path.to_owned(),
        None => journal_path(&scope)?,
    };
    let _directory =
        lock_directory_until(&path, Instant::now() + Duration::from_secs(5)).map_err(|error| {
            match error {
                OutboxError::Busy => AgentBusError::Internal(
                    "another outbox command is changing the journal; retry".to_owned(),
                ),
                _sanitized => {
                    AgentBusError::Internal("outbox directory lock could not be taken".to_owned())
                }
            }
        })?;
    if options.dry_run && !journal_present(&path)? {
        // A plan must not rename anything, so it only reports a pending recovery.
        if let Some(staging) = committed_staging(&path)?.first() {
            return Ok(json!({"migrated":false,"dry_run":true,
                "interrupted_migration":file_name_text(staging),
                "reason":"an interrupted migration would be completed first"}));
        }
    }
    if !options.dry_run
        && let Some(staging) = recover_interrupted_migration(&path)?
    {
        return Ok(json!({"migrated":false,"completed_interrupted":true,
            "completed":file_name_text(&staging)}));
    }
    if !journal_present(&path)? {
        return Ok(json!({"migrated":false,"reason":"no journal"}));
    }
    let inspection = Journal::inspect(&path).map_err(|_sanitized| {
        AgentBusError::Internal("durable outbox inspection failed; preserve journal".to_owned())
    })?;
    let (Some(stored), Some(origin_id)) = (inspection.destination.as_deref(), inspection.origin_id)
    else {
        return Ok(json!({"migrated":false,"reason":"journal has no bound destination"}));
    };
    if stored == scope {
        return Ok(json!({"migrated":false,
            "reason":"journal already bound to current candidates",
            "pending":inspection.pending.len()}));
    }
    let plan = inspection
        .pending
        .iter()
        .map(|request| (request, fate(&inspection, request, now, options)))
        .collect::<Vec<_>>();
    let listed = |wanted: Fate| {
        plan.iter()
            .filter(|(_, planned)| *planned == wanted)
            .map(|(request, _)| report_entry(request, now))
            .collect::<Vec<_>>()
    };
    let blocked = listed(Fate::Blocked);
    if options.dry_run {
        return Ok(
            json!({"migrated":false,"dry_run":true,"would_succeed":blocked.is_empty(),
            "bound_scope_prefix":scope_prefix(stored),"current_scope_prefix":scope_prefix(&scope),
            "carried":listed(Fate::Carry),"expired":listed(Fate::Expired),
            "archived":listed(Fate::Archive),"abandoned":listed(Fate::Abandon),"blocked":blocked}),
        );
    }
    if !blocked.is_empty() {
        let ids = blocked
            .iter()
            .filter_map(|entry| entry["request_id"].as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AgentBusError::InvalidParams(format!(
            "outbox journal (scope {}) retains {} request(s) whose replay was attempted and never \
             settled: {ids}. The previous authority may have accepted them, so they cannot be re-sent \
             to the current one. Replay them under the previous configuration (`agent-bus outbox \
             flush`) while it is reachable, or archive them with `agent-bus outbox migrate \
             --include-attempted` (nothing is deleted; they are listed in the report)",
            scope_prefix(stored),
            blocked.len()
        )));
    }
    let mut report = json!({"migrated":true,"dry_run":false,
        "bound_scope_prefix":scope_prefix(stored),"current_scope_prefix":scope_prefix(&scope),
        "carried":listed(Fate::Carry),"expired":listed(Fate::Expired),
        "archived":listed(Fate::Archive),"abandoned":listed(Fate::Abandon)});

    let archive = path.with_file_name(format!("{MIGRATED_PREFIX}{origin_id}.jsonl"));
    let staging = path.with_file_name(format!("{STAGING_PREFIX}{origin_id}.jsonl"));
    for target in [&archive, &staging] {
        crate::outbox_private::plain_path(target).map_err(|_sanitized| {
            AgentBusError::Internal(
                "outbox migration path unavailable; preserve journal".to_owned(),
            )
        })?;
    }
    if std::fs::symlink_metadata(&archive).is_ok() {
        return Err(AgentBusError::Internal(
            "migrated outbox journal already exists; preserve both journals".to_owned(),
        ));
    }
    set_aside_partial_staging(&staging, origin_id)?;
    // Write the carried requests in full before anything moves, so that no stop
    // can leave them only in a file this command has already renamed.
    let mut next = Journal::open(&staging, &scope).map_err(|_sanitized| {
        AgentBusError::Internal("staging outbox journal could not be created".to_owned())
    })?;
    for (request, _) in plan.iter().filter(|(_, planned)| *planned == Fate::Carry) {
        next.adopt(request).map_err(|_sanitized| {
            AgentBusError::Internal(
                "carrying a request to the new outbox journal failed; the old journal is unchanged"
                    .to_owned(),
            )
        })?;
    }
    inspection
        .retire_to(&path, &archive)
        .map_err(|_sanitized| {
            AgentBusError::Internal(
                "outbox journal could not be retired (in use?); the old journal is unchanged"
                    .to_owned(),
            )
        })?;
    after_archive()?;
    next.move_to(&path).map_err(|_sanitized| {
        AgentBusError::Internal(format!(
            "the old journal was archived as {} but the new one could not be put in place; it \
             is complete in {} and the next outbox command finishes the move",
            file_name_text(&archive),
            file_name_text(&staging)
        ))
    })?;
    report["archive"] = json!(archive.display().to_string());
    Ok(report)
}

/// A staging file for this origin means an earlier attempt stopped before the
/// old journal moved, so it is derived data. Keep it under another name.
fn set_aside_partial_staging(staging: &Path, origin_id: uuid::Uuid) -> Result<()> {
    if std::fs::symlink_metadata(staging).is_err() {
        return Ok(());
    }
    let partial = staging.with_file_name(format!("{STAGING_PREFIX}{origin_id}.partial.jsonl"));
    let stale = Journal::inspect(staging).map_err(|_sanitized| {
        AgentBusError::Internal(
            "an earlier migration left an unreadable staging journal; preserve it and the journal"
                .to_owned(),
        )
    })?;
    if std::fs::symlink_metadata(&partial).is_ok() {
        return Err(AgentBusError::Internal(
            "earlier migration staging journals already exist; preserve them and the journal"
                .to_owned(),
        ));
    }
    stale.retire_to(staging, &partial).map_err(|_sanitized| {
        AgentBusError::Internal("a stale staging journal could not be set aside".to_owned())
    })
}

/// Finish a migration that stopped after the old journal was archived.
///
/// Only acts when there is no active journal and exactly one committed staging
/// journal (its `journal.migrated-` archive exists); returns it when it moved
/// one into place. A staging file without that archive belongs to an attempt
/// that never reached the commit point, and may belong to a journal that is
/// still live, so it is left alone. The caller holds the directory lock.
fn recover_interrupted_migration(path: &Path) -> Result<Option<PathBuf>> {
    if journal_present(path)? {
        return Ok(None);
    }
    let mut staged = committed_staging(path)?;
    let Some(staging) = staged.pop() else {
        return Ok(None);
    };
    if !staged.is_empty() {
        return Err(AgentBusError::Internal(
            "several interrupted outbox migrations; preserve every staging journal".to_owned(),
        ));
    }
    let inspection = Journal::inspect(&staging).map_err(|_sanitized| {
        AgentBusError::Internal(
            "interrupted outbox migration is unreadable; preserve the staging journal".to_owned(),
        )
    })?;
    if inspection.destination.is_none() {
        return Err(AgentBusError::Internal(
            "interrupted outbox migration has no bound destination; preserve the staging journal"
                .to_owned(),
        ));
    }
    inspection.retire_to(&staging, path).map_err(|_sanitized| {
        AgentBusError::Internal(
            "interrupted outbox migration could not be completed; preserve both journals"
                .to_owned(),
        )
    })?;
    Ok(Some(staging))
}

/// Drain existing requests only, preserving order and each original ID.
///
/// # Errors
/// Credential, scope, custody, contention or durable settlement failures.
pub fn flush_pending<T: NativeReplayTransport>(
    settings: &Settings,
    transport: &T,
    path_override: Option<&Path>,
) -> Result<Value> {
    flush_pending_with_hook(settings, transport, path_override, || {})
}

fn flush_pending_with_hook<T: NativeReplayTransport>(
    settings: &Settings,
    transport: &T,
    path_override: Option<&Path>,
    before_disposition: impl FnOnce(),
) -> Result<Value> {
    let scope = candidates_fingerprint(&settings.effective_hub_candidates());
    let path = match path_override {
        Some(path) => path.to_owned(),
        None => journal_path(&scope)?,
    };
    // With the active path vacant, a committed staging journal holds the debt;
    // opening below finishes the migration that left it.
    if !journal_present(&path)? && committed_staging(&path)?.is_empty() {
        return Ok(json!({"remaining":0,"applied":0,"requests_submitted":0}));
    }
    let (scope, route) = replay_context(settings, transport)?;
    let mut journal = match open_journal_until(&path, &scope, route.deadline) {
        // Nothing owed to the previous destination: retire it and report an
        // empty queue rather than a recovery state.
        Err(OutboxError::ScopeChanged) => {
            before_disposition();
            let disposition =
                retire_previous_scope(&path, &scope, now_ms()?, RetirePolicy::SettledOnly)?;
            if disposition["rotated"] == true {
                return Ok(json!({"remaining":0,"applied":0,"requests_submitted":0}));
            }
            // Another caller can already have rebound the active journal and
            // enqueued current-scope debt. Reopen once under custody and drain;
            // contention shares the original deadline; another scope move
            // still fails visibly rather than repeating namespace disposition.
            open_journal_until(&path, &scope, route.deadline)
        }
        opened => opened,
    }
    .map_err(|error| match error {
        OutboxError::Busy => AgentBusError::Internal(
            "durable outbox is busy; preserve the journal and original request IDs, then retry outbox flush"
                .to_owned(),
        ),
        _sanitized => AgentBusError::Internal(
            "private durable outbox unavailable; preserve journal".to_owned(),
        ),
    })?;
    let report = journal.flush(&route, now_ms()?, 16).map_err(|_sanitized| {
        AgentBusError::Internal(
            "durable flush settlement failed; preserve journal and stable identities".to_owned(),
        )
    })?;
    serde_json::to_value(report).map_err(AgentBusError::from)
}
fn submit_journal(
    journal: &mut Journal,
    transport: &impl ReplayTransport,
    operation: Operation,
    surface: ClientSurface,
    arguments: Map<String, Value>,
    now: u64,
) -> Result<Value> {
    let request = journal
        .enqueue(operation, surface, arguments, now)
        .map_err(|_sanitized| {
            AgentBusError::InvalidParams(
                "durable enqueue failed; retention is not established, preserve journal".to_owned(),
            )
        })?;
    let Ok(mut report) = journal.flush(transport, now, 16) else {
        return Ok(
            json!({"status":"pending_recovery","request_id":request.request_id,
            "operation":operation,"claim_granted":false,"retry_new_submission":false,
            "error":"durable settlement failed; preserve journal and original request identity"}),
        );
    };
    if report.rejected > 0 && report.blocked_request == Some(request.request_id) {
        // This exact entry was durably rejected, so it is no longer pending.
        return Err(AgentBusError::InvalidParams(
            "durable replay rejected; no offline grant or unsafe fallback".to_owned(),
        ));
    }
    if let Some(
        disposition
        @ (crate::outbox::Disposition::Expired | crate::outbox::Disposition::Superseded),
    ) = report.dispositions.get(&request.request_id)
    {
        return Ok(json!({"status":disposition,"request_id":request.request_id,
            "operation":operation,"pending":report.remaining,"claim_granted":false,"retained":false}));
    }
    if let Some(result) = report.results.remove(&request.request_id) {
        return Ok(result);
    }
    // Older A can be rejected while freshly durable B remains queued. Never
    // report a failed B with no ID: a caller retry would create duplicate C.
    Ok(json!({"status":"queued","request_id":request.request_id,
        "operation":operation,"pending":report.remaining,"claim_granted":false,
        "older_request_rejected":report.rejected>0,
        "blocked_request_id":report.blocked_request}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs::OpenOptions;
    use tempfile::NamedTempFile;

    fn open_journal(temp: &NamedTempFile) -> Journal {
        Journal::from_private_file(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path())
                .unwrap(),
            "fixture-scope",
        )
        .unwrap()
    }
    fn args(body: &str) -> Map<String, Value> {
        json!({"sender":"fixture","recipient":"receiver","topic":"status","body":body})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn removed_credentials_refuse_new_legacy_write_ahead_of_retained_debt() {
        use crate::hub_candidates::{CandidateAuth, HubCandidate};
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let private_root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(private_root.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let temp = NamedTempFile::new_in(private_root.path()).unwrap();
        let mut settings = Settings::for_test();
        settings.server_urls = vec!["http://localhost:8484".to_owned()];
        settings.hub_candidates = vec![HubCandidate {
            url: settings.server_urls[0].clone(),
            role: HubRole::Authoritative,
            auth: CandidateAuth::Global,
            hub: Some("fixture-authority".to_owned()),
            sites: Vec::new(),
        }];
        settings.auth_token = Some("synthetic-fixture-token".to_owned());
        let scope = candidates_fingerprint(&settings.effective_hub_candidates());
        let mut journal = Journal::from_private_file(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path())
                .unwrap(),
            &scope,
        )
        .unwrap();
        let original = journal
            .enqueue(Operation::Send, ClientSurface::Cli, args("retained"), 100)
            .unwrap();
        drop(journal);
        let before = std::fs::read(temp.path()).unwrap();
        settings.auth_token = None;
        assert!(!durable_replay_configured(&settings));
        let error = legacy_write_guard(&settings, Some(temp.path())).unwrap_err();
        #[cfg(unix)]
        assert!(error.to_string().contains("retained durable requests"));
        #[cfg(windows)]
        let _ = error; // A non-private test handle must also refuse, including Wine.
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
        let retained = Journal::from_private_file(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path())
                .unwrap(),
            &scope,
        )
        .unwrap();
        assert_eq!(
            retained
                .pending()
                .map(|request| request.request_id)
                .collect::<Vec<_>>(),
            vec![original.request_id]
        );
        assert!(legacy_write_guard(&settings, Some(&temp.path().with_extension("absent"))).is_ok());
    }

    /// Settings naming one authority reachable at each URL, in order.
    fn authority_settings(urls: &[&str]) -> Settings {
        use crate::hub_candidates::{CandidateAuth, HubCandidate};
        let mut settings = Settings::for_test();
        settings.server_urls = urls.iter().map(|url| (*url).to_owned()).collect();
        settings.hub_candidates = urls
            .iter()
            .map(|url| HubCandidate {
                url: (*url).to_owned(),
                role: HubRole::Authoritative,
                auth: CandidateAuth::Global,
                hub: Some("fixture-authority".to_owned()),
                sites: Vec::new(),
            })
            .collect();
        settings.auth_token = Some("synthetic-fixture-token".to_owned());
        settings
    }

    /// A private outbox directory, as `submit` would establish it.
    fn private_journal_path(root: &tempfile::TempDir) -> PathBuf {
        let path = journal_path_in(root.path());
        crate::outbox_private::create_directory(path.parent().unwrap()).unwrap();
        path
    }

    fn retired_siblings(path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|candidate| candidate != path && file_name_text(candidate) != DIRECTORY_LOCK)
            .collect()
    }

    struct Applied;
    impl NativeReplayTransport for Applied {
        fn replay(
            &self,
            _url: &str,
            _hub_identity: &str,
            request: &ReplayRequest,
            _timeout: Duration,
        ) -> std::result::Result<ReplayResponse, ReplayFailure> {
            Ok(ReplayResponse::Applied {
                result: json!({"id":request.request_id}),
            })
        }
    }

    /// Protects: concurrent native submissions and one stable retained identity.
    /// Detects: fail-fast contention, duplicate submission or lost replay debt.
    /// Needs: an owned private journal and synthetic transport; no hub or production state.
    /// Breadcrumb: `submit`, `submit_journal` and `flush_pending` in this module.
    #[test]
    fn contended_submission_waits_then_retains_one_original_request() {
        struct Offline(RefCell<Vec<uuid::Uuid>>);
        impl NativeReplayTransport for Offline {
            fn replay(
                &self,
                _url: &str,
                _hub_identity: &str,
                request: &ReplayRequest,
                _timeout: Duration,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                self.0.borrow_mut().push(request.request_id);
                Err(ReplayFailure::Transport)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let settings = authority_settings(&["http://localhost:8484"]);
        let scope = candidates_fingerprint(&settings.effective_hub_candidates());
        let held = Journal::open(&path, &scope).unwrap();
        assert!(matches!(
            Journal::open(&path, &scope),
            Err(OutboxError::Busy)
        ));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            drop(held);
        });
        let transport = Offline(RefCell::new(Vec::new()));
        let response = submit(
            &settings,
            &transport,
            Operation::Send,
            ClientSurface::Cli,
            args("one retained request after contention"),
            Some(&path),
        );
        release.join().unwrap();
        let response = response.expect("a short-lived writer must not reject a new submission");
        assert_eq!(response["status"], "queued");
        let retained = {
            let inspection = Journal::inspect(&path).unwrap();
            assert_eq!(inspection.pending.len(), 1);
            inspection.pending[0].request_id
        };
        assert_eq!(response["request_id"], retained.to_string());
        assert_eq!(*transport.0.borrow(), vec![retained]);
        let report = flush_pending(&settings, &Applied, Some(&path)).unwrap();
        assert_eq!(
            (report["applied"].as_u64(), report["remaining"].as_u64()),
            (Some(1), Some(0))
        );
        assert_eq!(
            Journal::inspect(&path).unwrap().pending,
            [] as [ReplayRequest; 0]
        );
    }

    /// Protects: a finite lock budget and existing private journal bytes.
    /// Detects: a reset deadline, unbounded waiting or mutation before acquisition.
    /// Needs: an owned locked journal; no transport or production state.
    /// Breadcrumb: `open_journal_until` in this module.
    #[test]
    fn contended_journal_exhausts_original_deadline_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        // Windows byte-range locks also reject reads through a second handle.
        // Snapshot outside the held lock lifetime instead of weakening custody.
        drop(Journal::open(&path, "fixture-scope").unwrap());
        let before = std::fs::read(&path).unwrap();
        let held = Journal::open(&path, "fixture-scope").unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(70);
        assert!(matches!(
            open_journal_until(&path, "fixture-scope", deadline),
            Err(OutboxError::Busy)
        ));
        assert!(Instant::now() >= deadline);
        assert!(matches!(
            open_journal_until(&path, "fixture-scope", deadline),
            Err(OutboxError::Busy)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(held.pending().count(), 0);
        drop(held);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Protects: a released writer remains acquirable after the final capped sleep.
    /// Detects: reporting stale contention when a scheduler wakes beyond the deadline.
    /// Needs: a real owned journal lock and injected time; no real sleep or transport.
    /// Breadcrumb: `open_journal_until_with`, called by `open_journal_until`.
    #[test]
    fn journal_acquires_after_capped_sleep_even_if_scheduler_passes_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "fixture-scope").unwrap());
        let before = std::fs::read(&path).unwrap();
        let mut held = Some(Journal::open(&path, "fixture-scope").unwrap());
        assert!(matches!(
            Journal::open(&path, "fixture-scope"),
            Err(OutboxError::Busy)
        ));
        let started = Instant::now();
        let budget = Duration::from_millis(5);
        let deadline = started + budget;
        let clock = std::cell::Cell::new(started);
        let mut sleeps = 0;
        let opened = open_journal_until_with(
            &path,
            "fixture-scope",
            deadline,
            || clock.get(),
            |duration| {
                assert_eq!(
                    duration, budget,
                    "the last sleep must use the remaining budget"
                );
                sleeps += 1;
                drop(held.take());
                clock.set(deadline + Duration::from_millis(1));
            },
        )
        .expect("a released writer must be acquired on the final attempt");
        assert_eq!(sleeps, 1);
        assert!(clock.get() > deadline);
        assert_eq!(opened.pending().count(), 0);
        drop(opened);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Protects: an exhausted lock budget never schedules an additional sleep.
    /// Detects: a reset budget or retry loop after the final acquisition remains busy.
    /// Needs: a real owned held journal and injected time; no real sleep or transport.
    /// Breadcrumb: `open_journal_until_with`, called by `open_journal_until`.
    #[test]
    fn journal_stops_after_final_busy_acquisition_when_scheduler_passes_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "fixture-scope").unwrap());
        let before = std::fs::read(&path).unwrap();
        let held = Journal::open(&path, "fixture-scope").unwrap();
        assert!(matches!(
            Journal::open(&path, "fixture-scope"),
            Err(OutboxError::Busy)
        ));
        let started = Instant::now();
        let budget = Duration::from_millis(5);
        let deadline = started + budget;
        let clock = std::cell::Cell::new(started);
        let mut sleeps = 0;
        let result = open_journal_until_with(
            &path,
            "fixture-scope",
            deadline,
            || clock.get(),
            |duration| {
                assert_eq!(
                    duration, budget,
                    "the last sleep must use the remaining budget"
                );
                sleeps += 1;
                clock.set(deadline + Duration::from_millis(1));
            },
        );
        assert!(matches!(result, Err(OutboxError::Busy)));
        assert_eq!(sleeps, 1);
        assert_eq!(held.pending().count(), 0);
        drop(held);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Protects: callers can distinguish exhausted contention from failed private custody.
    /// Detects: a generic error, early rejection or retention before lock acquisition.
    /// Needs: an owned journal held for the existing twelve-second request budget.
    /// Breadcrumb: `submit` and `open_journal_until` in this module.
    #[test]
    fn contended_submission_deadline_reports_that_no_request_was_retained() {
        struct NoCall;
        impl NativeReplayTransport for NoCall {
            fn replay(
                &self,
                _url: &str,
                _hub_identity: &str,
                _request: &ReplayRequest,
                _timeout: Duration,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                panic!("exhausted lock acquisition must never reach transport");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let settings = authority_settings(&["http://localhost:8484"]);
        let scope = candidates_fingerprint(&settings.effective_hub_candidates());
        drop(Journal::open(&path, &scope).unwrap());
        let before = std::fs::read(&path).unwrap();
        let held = Journal::open(&path, &scope).unwrap();
        let started = Instant::now();
        let error = submit(
            &settings,
            &NoCall,
            Operation::Send,
            ClientSurface::Cli,
            args("must not be retained"),
            Some(&path),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("busy"));
        assert!(error.contains("no new request was retained"));
        assert!(started.elapsed() >= Duration::from_secs(12));
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(held.pending().count(), 0);
        drop(held);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Protects: corruption remains a fail-closed storage failure.
    /// Detects: retrying non-contention errors or rewriting the invalid journal.
    /// Needs: only an owned private journal with synthetic invalid bytes.
    /// Breadcrumb: `open_journal_until` and `Journal::open`.
    #[test]
    fn corrupt_journal_is_not_retried_or_rewritten() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "fixture-scope").unwrap());
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not a journal record\n")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let started = Instant::now();
        assert!(matches!(
            open_journal_until(&path, "fixture-scope", started + Duration::from_secs(10)),
            Err(OutboxError::Corrupt)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Protects: an existing queue drains after a concurrent writer releases custody.
    /// Detects: fail-fast flush contention, new submission or lost stable IDs.
    /// Needs: one owned retained request and a synthetic accepting transport.
    /// Breadcrumb: `flush_pending_with_hook` and `open_journal_until`.
    #[test]
    fn contended_flush_waits_without_submitting_another_request() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let settings = authority_settings(&["http://localhost:8484"]);
        let scope = candidates_fingerprint(&settings.effective_hub_candidates());
        let mut held = Journal::open(&path, &scope).unwrap();
        held.enqueue(
            Operation::Send,
            ClientSurface::Cli,
            args("existing request only"),
            100,
        )
        .unwrap();
        assert!(matches!(
            Journal::open(&path, &scope),
            Err(OutboxError::Busy)
        ));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            drop(held);
        });
        let report = flush_pending(&settings, &Applied, Some(&path));
        release.join().unwrap();
        let report = report.expect("flush must wait for a short-lived writer");
        assert_eq!(report["applied"], 1);
        assert_eq!(report["remaining"], 0);
        let mut reopened = Journal::open(&path, &scope).unwrap();
        let next = reopened
            .enqueue(Operation::Send, ClientSurface::Cli, args("next"), 101)
            .unwrap();
        assert_eq!(next.sequence, 2, "flush must not enqueue a replacement");
    }

    // dtm-p1gen7, 2026-10-14: adding one tailnet URL to the same hub's
    // candidate list changed the fingerprint, and every later write failed
    // with "could not be opened" although the journal held no pending debt.
    // A fully settled journal carries no obligation to the old destination.
    #[test]
    fn candidate_edit_retires_a_settled_journal_and_the_write_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let before_edit = authority_settings(&["http://localhost:8484"]);
        let response = submit(
            &before_edit,
            &Applied,
            Operation::Send,
            ClientSurface::Cli,
            args("before edit"),
            Some(&path),
        )
        .unwrap();
        assert_eq!(response["id"].as_str().map(str::is_empty), Some(false));
        let settled_bytes = std::fs::read(&path).unwrap();

        let after_edit = authority_settings(&["http://localhost:8484", "http://127.0.0.1:8485"]);
        let response = submit(
            &after_edit,
            &Applied,
            Operation::Send,
            ClientSurface::Cli,
            args("after edit"),
            Some(&path),
        )
        .unwrap();
        assert_eq!(response["id"].as_str().map(str::is_empty), Some(false));

        // The previous journal is preserved byte for byte beside the new one.
        let retired = retired_siblings(&path);
        assert_eq!(retired.len(), 1);
        assert_eq!(std::fs::read(&retired[0]).unwrap(), settled_bytes);
        let current = status(&after_edit, Some(&path)).unwrap();
        assert_eq!(current["pending"], 0);
        assert!(current.get("scope_mismatch").is_none());
    }

    // Retained work stays bound to the authority it was queued for. Status
    // must still describe it (a read must not fail), and neither a new write
    // nor an explicit rotation may move or rewrite it.
    #[test]
    fn unexpired_retained_requests_block_retirement_but_status_reports_them() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let before_edit = authority_settings(&["http://localhost:8484"]);
        let scope = candidates_fingerprint(&before_edit.effective_hub_candidates());
        let mut journal = Journal::open(&path, &scope).unwrap();
        let retained = journal
            .enqueue(Operation::Send, ClientSurface::Cli, args("retained"), 100)
            .unwrap();
        drop(journal);
        let original = std::fs::read(&path).unwrap();

        let after_edit = authority_settings(&["http://localhost:8484", "http://127.0.0.1:8485"]);
        let report = status(&after_edit, Some(&path)).unwrap();
        assert_eq!(report["pending"], 1);
        assert_eq!(report["scope_mismatch"], true);
        assert_eq!(
            report["requests"][0]["request_id"],
            retained.request_id.to_string()
        );
        assert_eq!(report["rotation"], "blocked");

        assert!(rotate(&after_edit, Some(&path), u64::MAX / 2).is_err());
        assert!(
            submit(
                &after_edit,
                &Applied,
                Operation::Send,
                ClientSurface::Cli,
                args("new"),
                Some(&path),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(retired_siblings(&path), [] as [std::path::PathBuf; 0]);
    }

    // Presence heartbeats and claim requests are time-bounded; once expired
    // they have no remaining effect, so an operator may retire them
    // explicitly. The journal is renamed, never rewritten or deleted.
    #[test]
    fn explicit_rotation_retires_expired_only_requests_preserving_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let before_edit = authority_settings(&["http://localhost:8484"]);
        let scope = candidates_fingerprint(&before_edit.effective_hub_candidates());
        let mut journal = Journal::open(&path, &scope).unwrap();
        let presence = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                json!({"agent":"fixture","ttl_seconds":1})
                    .as_object()
                    .unwrap()
                    .clone(),
                100,
            )
            .unwrap();
        drop(journal);
        let original = std::fs::read(&path).unwrap();

        let after_edit = authority_settings(&["http://127.0.0.1:8485"]);
        assert_eq!(
            status(&after_edit, Some(&path)).unwrap()["rotation"],
            "explicit"
        );
        // Not yet expired at the request's own clock: refused.
        assert!(rotate(&after_edit, Some(&path), presence.created_ms).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let report = rotate(&after_edit, Some(&path), presence.expires_ms).unwrap();
        assert_eq!(report["rotated"], true);
        assert_eq!(
            report["retired_requests"][0]["request_id"],
            presence.request_id.to_string()
        );
        let retired = retired_siblings(&path);
        assert_eq!(retired.len(), 1);
        assert_eq!(std::fs::read(&retired[0]).unwrap(), original);
        assert!(!path.exists());
        assert_eq!(status(&after_edit, Some(&path)).unwrap()["pending"], 0);
        // A second rotation finds nothing left to retire.
        assert_eq!(
            rotate(&after_edit, Some(&path), presence.expires_ms).unwrap()["rotated"],
            false
        );
    }

    // Force the old-scope writer into the inspection-to-retirement interval.
    // Losing that writer's newly queued request would strand durable debt.
    #[test]
    fn retirement_holds_inspection_custody_against_old_scope_enqueue() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "old-scope").unwrap());
        let original = std::fs::read(&path).unwrap();
        let report = retire_previous_scope_with_hook(
            &path,
            "new-scope",
            100,
            RetirePolicy::SettledOnly,
            |retired| {
                assert!(!retired.exists());
                let writer = Journal::open(&path, "old-scope").and_then(|mut journal| {
                    journal.enqueue(
                        Operation::Send,
                        ClientSurface::Cli,
                        args("intervening"),
                        100,
                    )
                });
                assert!(
                    matches!(writer, Err(OutboxError::Busy)),
                    "old-scope writer must not enqueue after retirement inspection"
                );
            },
        )
        .unwrap();
        assert_eq!(report["rotated"], true);
        let retired = retired_siblings(&path);
        assert_eq!(retired.len(), 1);
        assert_eq!(std::fs::read(&retired[0]).unwrap(), original);
    }

    #[test]
    fn retirement_blocks_concurrent_rotation_before_new_active_can_be_moved() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "old-scope").unwrap());
        let original = std::fs::read(&path).unwrap();
        retire_previous_scope_with_hook(&path, "new-scope", 100, RetirePolicy::SettledOnly, |_| {
            assert!(
                retire_previous_scope(&path, "competing-scope", 100, RetirePolicy::SettledOnly,)
                    .is_err(),
                "competing retirement must not move the inspected source"
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                original.len() as u64
            );
        })
        .unwrap();
        let mut new_active = Journal::open(&path, "new-scope").unwrap();
        let pending = new_active
            .enqueue(Operation::Send, ClientSurface::Cli, args("new active"), 101)
            .unwrap();
        drop(new_active);
        let active = Journal::inspect(&path).unwrap();
        assert_eq!(active.pending[0].request_id, pending.request_id);
        let retired = retired_siblings(&path);
        assert_eq!(retired.len(), 1);
        assert_eq!(std::fs::read(&retired[0]).unwrap(), original);
    }

    #[test]
    fn retirement_archive_collision_at_move_preserves_both_journals() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "old-scope").unwrap());
        let original = std::fs::read(&path).unwrap();
        let collision = b"existing archive must survive";
        let outcome = retire_previous_scope_with_hook(
            &path,
            "new-scope",
            100,
            RetirePolicy::SettledOnly,
            |retired| {
                let mut file = crate::outbox_private::open(retired).unwrap();
                std::io::Write::write_all(&mut file, collision).unwrap();
                file.sync_all().unwrap();
            },
        );
        assert!(
            outcome.is_err(),
            "archive publication must never replace a collision"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let retired = retired_siblings(&path);
        assert_eq!(retired.len(), 1);
        assert_eq!(std::fs::read(&retired[0]).unwrap(), collision);
    }

    #[test]
    fn attempted_expired_presence_blocks_retirement_and_keeps_cached_repair_identity() {
        struct Pending;
        impl ReplayTransport for Pending {
            fn replay(
                &self,
                _: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                Ok(ReplayResponse::Pending)
            }
        }
        struct Cached {
            expected: uuid::Uuid,
            calls: RefCell<Vec<uuid::Uuid>>,
        }
        impl ReplayTransport for Cached {
            fn replay(
                &self,
                request: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                assert_eq!(request.request_id, self.expected);
                self.calls.borrow_mut().push(request.request_id);
                Ok(ReplayResponse::Applied {
                    result: json!({"cached_history_repair":true}),
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let before = authority_settings(&["http://localhost:8484"]);
        let scope = candidates_fingerprint(&before.effective_hub_candidates());
        let mut journal = Journal::open(&path, &scope).unwrap();
        let request = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                json!({"agent":"fixture","ttl_seconds":1})
                    .as_object()
                    .unwrap()
                    .clone(),
                100,
            )
            .unwrap();
        assert_eq!(journal.flush(&Pending, 100, 1).unwrap().remaining, 1);
        drop(journal);
        let original = std::fs::read(&path).unwrap();
        let after = authority_settings(&["http://localhost:8485"]);
        assert!(rotate(&after, Some(&path), request.expires_ms).is_err());
        assert_eq!(status(&after, Some(&path)).unwrap()["rotation"], "blocked");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(retired_siblings(&path), [] as [std::path::PathBuf; 0]);
        let mut journal = Journal::open(&path, &scope).unwrap();
        let cached = Cached {
            expected: request.request_id,
            calls: RefCell::new(Vec::new()),
        };
        let repaired = journal.flush(&cached, request.expires_ms, 1).unwrap();
        assert_eq!(
            (repaired.applied, repaired.expired, repaired.remaining),
            (1, 0, 0)
        );
        assert_eq!(*cached.calls.borrow(), vec![request.request_id]);
    }

    #[test]
    fn retirement_source_replacement_preserves_original_and_new_active_debt() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "old-scope").unwrap());
        let original = std::fs::read(&path).unwrap();
        let displaced = path.with_file_name("displaced-original.jsonl");
        let retained_id = RefCell::new(None);
        let outcome = retire_previous_scope_with_hook(
            &path,
            "new-scope",
            100,
            RetirePolicy::SettledOnly,
            |_| {
                std::fs::rename(&path, &displaced).unwrap();
                let mut replacement = Journal::open(&path, "replacement-scope").unwrap();
                let request = replacement
                    .enqueue(
                        Operation::Send,
                        ClientSurface::Cli,
                        args("replacement debt"),
                        101,
                    )
                    .unwrap();
                *retained_id.borrow_mut() = Some(request.request_id);
            },
        );
        assert!(outcome.is_err(), "retirement must refuse a replaced source");
        assert_eq!(std::fs::read(&displaced).unwrap(), original);
        let active = Journal::inspect(&path).unwrap();
        assert_eq!(active.destination.as_deref(), Some("replacement-scope"));
        assert_eq!(active.pending[0].request_id, retained_id.borrow().unwrap());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            2
        );
    }

    #[test]
    fn flush_reopens_debt_created_after_scope_change_before_disposition() {
        struct TrackedApplied(RefCell<Vec<uuid::Uuid>>);
        impl NativeReplayTransport for TrackedApplied {
            fn replay(
                &self,
                _url: &str,
                _hub: &str,
                request: &ReplayRequest,
                _timeout: Duration,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                self.0.borrow_mut().push(request.request_id);
                Ok(ReplayResponse::Applied {
                    result: json!({"id":request.request_id}),
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        drop(Journal::open(&path, "old-scope").unwrap());
        let settings = authority_settings(&["http://localhost:8485"]);
        let scope = candidates_fingerprint(&settings.effective_hub_candidates());
        let transport = TrackedApplied(RefCell::new(Vec::new()));
        let retained_id = RefCell::new(None);
        let report =
            flush_pending_with_hook(&settings, &transport, Some(&path), || {
                assert_eq!(
                    retire_previous_scope(&path, &scope, 100, RetirePolicy::SettledOnly,).unwrap()
                        ["rotated"],
                    true
                );
                let mut current = Journal::open(&path, &scope).unwrap();
                let request = current
                    .enqueue(
                        Operation::Send,
                        ClientSurface::Cli,
                        args("current-scope debt"),
                        101,
                    )
                    .unwrap();
                *retained_id.borrow_mut() = Some(request.request_id);
            })
            .unwrap();
        assert_eq!(
            report["applied"], 1,
            "current-scope debt must be flushed before reporting empty"
        );
        assert_eq!(report["remaining"], 0);
        assert_eq!(*transport.0.borrow(), vec![retained_id.borrow().unwrap()]);
        assert_eq!(
            Journal::inspect(&path).unwrap().pending,
            [] as [crate::outbox::ReplayRequest; 0]
        );
    }

    #[test]
    fn absent_journal_status_and_flush_do_not_create_state_or_contact_transport() {
        struct NoCall;
        impl NativeReplayTransport for NoCall {
            fn replay(
                &self,
                _url: &str,
                _hub_identity: &str,
                _request: &ReplayRequest,
                _timeout: Duration,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                panic!("absent journal must not contact a provider");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = journal_path_in(root.path());
        assert!(!path.parent().unwrap().exists());
        let settings = Settings::for_test();
        assert_eq!(status(&settings, Some(&path)).unwrap()["pending"], 0);
        assert_eq!(
            flush_pending(&settings, &NoCall, Some(&path)).unwrap()["remaining"],
            0
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        // An existing non-journal object is not silently classified as absent.
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(journal_present(&path).unwrap());
        assert!(status(&settings, Some(&path)).is_err());
        assert!(flush_pending(&settings, &NoCall, Some(&path)).is_err());
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_journal_link_is_present_and_refused_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal");
        std::os::unix::fs::symlink(root.path().join("missing"), &path).unwrap();
        assert!(journal_present(&path).is_err());
        assert!(status(&Settings::for_test(), Some(&path)).is_err());
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!root.path().join("missing").exists());
        let ancestor = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join("missing-directory"), &ancestor).unwrap();
        assert!(status(&Settings::for_test(), Some(&ancestor.join("journal"))).is_err());
        assert!(!root.path().join("missing-directory").exists());
        let non_directory = root.path().join("regular-file");
        std::fs::write(&non_directory, b"original").unwrap();
        assert!(journal_present(&non_directory.join("journal")).is_err());
        assert_eq!(std::fs::read(&non_directory).unwrap(), b"original");
    }

    #[test]
    fn superseded_new_submission_is_terminal_and_never_reported_queued() {
        struct Superseded;
        impl ReplayTransport for Superseded {
            fn replay(
                &self,
                _request: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                Ok(ReplayResponse::Superseded)
            }
        }
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open_journal(&temp);
        let response = submit_journal(
            &mut journal,
            &Superseded,
            Operation::Presence,
            ClientSurface::Mcp,
            json!({"agent":"fixture","ttl_seconds":1})
                .as_object()
                .unwrap()
                .clone(),
            100,
        )
        .unwrap();
        assert_eq!(response["status"], "superseded");
        assert_eq!(response["retained"], false);
        assert_eq!(response["pending"], 0);
        assert_eq!(response["claim_granted"], false);
        let id = response["request_id"].as_str().unwrap().to_owned();
        drop(journal);
        assert_eq!(open_journal(&temp).pending().count(), 0);
        assert_ne!(id, "");
    }

    #[test]
    fn older_rejection_returns_new_retained_id_then_flushes_it_exactly_once() {
        struct Transport {
            calls: RefCell<Vec<uuid::Uuid>>,
            reject: uuid::Uuid,
        }
        impl ReplayTransport for Transport {
            fn replay(
                &self,
                request: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                self.calls.borrow_mut().push(request.request_id);
                if request.request_id == self.reject {
                    return Err(ReplayFailure::Permanent);
                }
                Ok(ReplayResponse::Applied {
                    result: json!({"id":request.request_id}),
                })
            }
        }
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open_journal(&temp);
        let old = journal
            .enqueue(Operation::Send, ClientSurface::Cli, args("older"), 100)
            .unwrap();
        let transport = Transport {
            calls: RefCell::new(vec![]),
            reject: old.request_id,
        };
        let response = submit_journal(
            &mut journal,
            &transport,
            Operation::Send,
            ClientSurface::Cli,
            args("new"),
            101,
        )
        .unwrap();
        assert_eq!(response["status"], "queued");
        assert_eq!(response["older_request_rejected"], true);
        let retained = journal.pending().next().unwrap().request_id;
        assert_eq!(response["request_id"], retained.to_string());
        assert_eq!(journal.pending().count(), 1);
        assert_eq!(*transport.calls.borrow(), vec![old.request_id]);
        let report = journal.flush(&transport, 102, 16).unwrap();
        assert_eq!((report.applied, report.remaining), (1, 0));
        assert_eq!(*transport.calls.borrow(), vec![old.request_id, retained]);
        drop(journal);
        assert_eq!(open_journal(&temp).pending().count(), 0);
    }

    #[test]
    fn real_post_acceptance_settlement_capacity_failure_preserves_visible_stable_id() {
        struct Fault {
            path: PathBuf,
            accepted: RefCell<Vec<uuid::Uuid>>,
        }
        impl ReplayTransport for Fault {
            fn replay(
                &self,
                request: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                self.accepted.borrow_mut().push(request.request_id);
                // External inert owned fault at the post-response append seam.
                // No fake storage return: actual file capacity prevents append.
                OpenOptions::new()
                    .write(true)
                    .open(&self.path)
                    .unwrap()
                    .set_len(32 * 1024 * 1024)
                    .unwrap();
                Ok(ReplayResponse::Applied {
                    result: json!({"id":request.request_id}),
                })
            }
        }
        let temp = NamedTempFile::new().unwrap();
        let mut journal = open_journal(&temp);
        let transport = Fault {
            path: temp.path().to_owned(),
            accepted: RefCell::new(vec![]),
        };
        let response = submit_journal(
            &mut journal,
            &transport,
            Operation::Send,
            ClientSurface::Cli,
            args("accepted"),
            100,
        )
        .unwrap();
        let retained = journal.pending().next().unwrap().request_id;
        assert_eq!(response["status"], "pending_recovery");
        assert_eq!(response["request_id"], retained.to_string());
        assert_eq!(response["retry_new_submission"], false);
        assert_eq!(*transport.accepted.borrow(), vec![retained]);
        assert_eq!(journal.pending().count(), 1);
        // Corrupted/capacity evidence is retained for recovery, not repaired.
        assert_eq!(
            std::fs::metadata(temp.path()).unwrap().len(),
            32 * 1024 * 1024
        );
    }
    // ---- outbox migrate -------------------------------------------------
    //
    // dtm-p1gen7, 2026-11-04: the fleet hub's identity changed (asuspro13 ->
    // dtm-work) while eleven requests were retained. Every write failed, the
    // old identity could no longer replay anything, and `rotate` refused the
    // never-expiring sends. These tests pin the supported recovery.

    /// Settings naming `hub` as the one authority, reachable at `url`.
    fn named_authority_settings(hub: &str, url: &str) -> Settings {
        let mut settings = authority_settings(&[url]);
        for candidate in &mut settings.hub_candidates {
            candidate.hub = Some(hub.to_owned());
        }
        settings
    }

    fn scope_of(settings: &Settings) -> String {
        candidates_fingerprint(&settings.effective_hub_candidates())
    }

    fn presence_args(agent: &str, ttl_seconds: u64) -> Map<String, Value> {
        json!({"agent":agent,"ttl_seconds":ttl_seconds})
            .as_object()
            .unwrap()
            .clone()
    }

    fn send_args(body: &str, topic: &str, thread: &str) -> Map<String, Value> {
        json!({"sender":"fixture","recipient":"receiver","topic":topic,"thread_id":thread,
            "body":body})
        .as_object()
        .unwrap()
        .clone()
    }

    struct Unreachable;
    impl crate::outbox::ReplayTransport for Unreachable {
        fn replay(
            &self,
            _request: &ReplayRequest,
        ) -> std::result::Result<ReplayResponse, ReplayFailure> {
            Err(ReplayFailure::Transport)
        }
    }

    /// A journal bound to the old authority holding, in order: a live unsent
    /// presence, an unsent send, and a presence whose own deadline passes four
    /// seconds after the base time. Times are real, because `flush` and `submit`
    /// read the wall clock when they decide what has expired.
    fn journal_on_old_authority(path: &Path) -> (Settings, Vec<ReplayRequest>, u64) {
        let base = now_ms().unwrap();
        let old = named_authority_settings("old-hub", "http://localhost:8484");
        let mut journal = Journal::open(path, &scope_of(&old)).unwrap();
        let live_presence = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                presence_args("live-agent", 3_600),
                base + 1_000,
            )
            .unwrap();
        let send = journal
            .enqueue(
                Operation::Send,
                ClientSurface::Cli,
                send_args("SECRET-BODY-TEXT", "task-failures", "tasks-thread"),
                base + 2_000,
            )
            .unwrap();
        let short_presence = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                presence_args("short-agent", 1),
                base + 3_000,
            )
            .unwrap();
        (old, vec![live_presence, send, short_presence], base)
    }

    /// Protects: unsent work survives an authority change with its stable IDs, and
    /// a dry run changes nothing.
    /// Detects: A journal stranded behind the old scope, requests re-queued under new
    /// IDs (a duplicate if one had been sent), bytes lost from the archive, or a report
    /// that leaks a body.
    /// Needs: owned private journal, synthetic transport; no hub or production state.
    /// Breadcrumb: `migrate`, `Journal::adopt`, `recover_interrupted_migration`.
    #[test]
    fn migrate_carries_unsent_requests_to_the_new_authority_under_their_own_ids() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, queued, base) = journal_on_old_authority(&path);
        let original = std::fs::read(&path).unwrap();
        let new = named_authority_settings("new-hub", "http://localhost:8485");

        let before = status(&new, Some(&path)).unwrap();
        assert_eq!(before["scope_mismatch"], true);
        assert_eq!(before["never_attempted"], 3);
        assert_eq!(before["attempted_unsettled"], 0);
        assert_eq!(before["recovery"], "agent-bus outbox migrate");

        // A write is refused and points at the supported command.
        let refused = submit(
            &new,
            &Applied,
            Operation::Send,
            ClientSurface::Cli,
            args("blocked"),
            Some(&path),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("outbox migrate"), "{refused}");

        // Dry run: the plan, no change.
        let options = MigrateOptions {
            dry_run: true,
            ..MigrateOptions::default()
        };
        let plan = migrate(&new, Some(&path), options, base + 10_000).unwrap();
        assert_eq!(plan["dry_run"], true);
        assert_eq!(plan["would_succeed"], true);
        assert_eq!(plan["carried"].as_array().unwrap().len(), 2);
        assert_eq!(plan["expired"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(retired_siblings(&path), [] as [PathBuf; 0]);

        let report = migrate(&new, Some(&path), MigrateOptions::default(), base + 10_000).unwrap();
        assert_eq!(report["migrated"], true);
        let carried = report["carried"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["request_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            carried,
            vec![
                queued[0].request_id.to_string(),
                queued[1].request_id.to_string()
            ]
        );
        assert_eq!(
            report["expired"][0]["request_id"],
            queued[2].request_id.to_string()
        );
        // The report identifies the send by topic and thread, never by body.
        assert_eq!(report["carried"][1]["topic"], "task-failures");
        assert_eq!(report["carried"][1]["thread_id"], "tasks-thread");
        assert!(!report.to_string().contains("SECRET-BODY-TEXT"));

        // The old journal is preserved byte for byte; no staging file is left.
        let siblings = retired_siblings(&path);
        assert_eq!(siblings.len(), 1, "{siblings:?}");
        assert_eq!(std::fs::read(&siblings[0]).unwrap(), original);
        assert!(file_name_text(&siblings[0]).starts_with(MIGRATED_PREFIX));

        // The new journal is bound to the new authority and holds the same IDs.
        let after = status(&new, Some(&path)).unwrap();
        assert!(after.get("scope_mismatch").is_none());
        assert_eq!(after["pending"], 2);
        let flushed = flush_pending(&new, &Applied, Some(&path)).unwrap();
        assert_eq!(flushed["applied"], 2);
        assert_eq!(flushed["remaining"], 0);
        let applied_ids = flushed["results"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            applied_ids,
            carried
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }

    /// Protects: a request that may already have reached the old authority is never
    /// re-sent to the new one.
    /// Detects: Carrying an attempted request (a duplicate delivery), or archiving it
    /// without the operator saying so.
    /// Needs: owned private journal and a synthetic unreachable transport.
    /// Breadcrumb: `fate`, `Journal::flush` marking a request attempted.
    #[test]
    fn migrate_refuses_attempted_requests_until_asked_and_never_resends_them() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let old = named_authority_settings("old-hub", "http://localhost:8484");
        let mut journal = Journal::open(&path, &scope_of(&old)).unwrap();
        // The head presence is attempted and left unsettled, as on dtm-p1gen7;
        // ordered replay then stops, so the sends behind it are never attempted.
        let attempted = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                presence_args("head", 3_600),
                1_000,
            )
            .unwrap();
        let report = journal.flush(&Unreachable, 1_001, 16).unwrap();
        assert_eq!(report.blocked_request, Some(attempted.request_id));
        let unsent = journal
            .enqueue(
                Operation::Send,
                ClientSurface::Cli,
                send_args("unsent", "task-failures", "tasks-thread"),
                1_002,
            )
            .unwrap();
        drop(journal);
        let original = std::fs::read(&path).unwrap();
        let new = named_authority_settings("new-hub", "http://localhost:8485");

        let state = status(&new, Some(&path)).unwrap();
        assert_eq!(state["attempted_unsettled"], 1);
        assert_eq!(state["never_attempted"], 1);
        assert_eq!(state["requests"][0]["attempted"], true);
        assert_eq!(
            state["recovery"],
            "agent-bus outbox migrate --include-attempted"
        );

        let refusal = migrate(&new, Some(&path), MigrateOptions::default(), 5_000)
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains(&attempted.request_id.to_string()),
            "{refusal}"
        );
        assert!(refusal.contains("--include-attempted"), "{refusal}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(retired_siblings(&path), [] as [PathBuf; 0]);

        let dry = migrate(
            &new,
            Some(&path),
            MigrateOptions {
                dry_run: true,
                ..MigrateOptions::default()
            },
            5_000,
        )
        .unwrap();
        assert_eq!(dry["would_succeed"], false);
        assert_eq!(
            dry["blocked"][0]["request_id"],
            attempted.request_id.to_string()
        );

        let report = migrate(
            &new,
            Some(&path),
            MigrateOptions {
                include_attempted: true,
                ..MigrateOptions::default()
            },
            5_000,
        )
        .unwrap();
        assert_eq!(
            report["abandoned"][0]["request_id"],
            attempted.request_id.to_string()
        );
        assert_eq!(
            report["carried"][0]["request_id"],
            unsent.request_id.to_string()
        );
        // Only the unsent request is in the new journal; the attempted one is archived.
        let after = status(&new, Some(&path)).unwrap();
        assert_eq!(after["pending"], 1);
        assert_eq!(
            after["requests"][0]["request_id"],
            unsent.request_id.to_string()
        );
        let siblings = retired_siblings(&path);
        assert_eq!(siblings.len(), 1);
        assert_eq!(std::fs::read(&siblings[0]).unwrap(), original);
    }

    /// Protects: an operator can dead-letter unsent requests instead of carrying them.
    /// Detects: `archive_only` still re-queuing requests, or losing them from the archive.
    /// Needs: owned private journal; no transport.
    /// Breadcrumb: `Fate::Archive`.
    #[test]
    fn migrate_archive_only_dead_letters_unsent_requests_with_a_report() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, queued, base) = journal_on_old_authority(&path);
        let original = std::fs::read(&path).unwrap();
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let report = migrate(
            &new,
            Some(&path),
            MigrateOptions {
                archive_only: true,
                ..MigrateOptions::default()
            },
            base + 1_500,
        )
        .unwrap();
        assert_eq!(report["carried"], json!([]));
        assert_eq!(report["archived"].as_array().unwrap().len(), 3);
        assert_eq!(
            report["archived"][1]["request_id"],
            queued[1].request_id.to_string()
        );
        assert_eq!(status(&new, Some(&path)).unwrap()["pending"], 0);
        let siblings = retired_siblings(&path);
        assert_eq!(siblings.len(), 1);
        assert_eq!(std::fs::read(&siblings[0]).unwrap(), original);
    }

    /// Protects: migrating a current or missing journal is a harmless no-op.
    /// Detects: Retiring a journal that already matches, or inventing one.
    /// Needs: owned private directory.
    /// Breadcrumb: `migrate` early returns.
    #[test]
    fn migrate_is_a_noop_for_a_current_or_missing_journal() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let current = named_authority_settings("new-hub", "http://localhost:8485");
        let report = migrate(&current, Some(&path), MigrateOptions::default(), 1).unwrap();
        assert_eq!(report["migrated"], false);
        assert!(!path.exists());
        let mut journal = Journal::open(&path, &scope_of(&current)).unwrap();
        journal
            .enqueue(Operation::Send, ClientSurface::Cli, args("kept"), 10)
            .unwrap();
        drop(journal);
        let before = std::fs::read(&path).unwrap();
        let report = migrate(&current, Some(&path), MigrateOptions::default(), 1).unwrap();
        assert_eq!(report["migrated"], false);
        assert_eq!(report["pending"], 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(retired_siblings(&path), [] as [PathBuf; 0]);
    }

    /// Protects: a migration that stops after archiving the old journal loses nothing and
    /// cannot leave two active journals.
    /// Detects: A write creating a fresh active journal beside the complete staging file,
    /// carried requests living only in the archive, or status mutating state.
    /// Needs: owned private journal; the hook stands in for a crash between the renames.
    /// Breadcrumb: `migrate_with_hook` and `recover_interrupted_migration`.
    #[test]
    fn interrupted_migration_is_finished_by_the_next_write_before_it_creates_a_journal() {
        let (_root, path, new, queued) = migration_stopped_between_renames();
        let names = journal_directory_names(&path);
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().any(|name| name.starts_with(MIGRATED_PREFIX)));
        assert!(names.iter().any(|name| name.starts_with(STAGING_PREFIX)));

        // Looking changes nothing and says what happened.
        let report = status(&new, Some(&path)).unwrap();
        assert!(report["interrupted_migration"].is_string());
        assert_eq!(journal_directory_names(&path), names);

        // The next write finishes the move first, so the carried requests keep
        // their place ahead of the new one.
        let response = submit(
            &new,
            &Unreachable2,
            Operation::Send,
            ClientSurface::Cli,
            args("after the stop"),
            Some(&path),
        )
        .unwrap();
        assert_eq!(response["status"], "queued", "{response}");
        let names = journal_directory_names(&path);
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|name| !name.starts_with(STAGING_PREFIX)));
        let after = status(&new, Some(&path)).unwrap();
        let ids = after["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["request_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], queued[0].request_id.to_string());
        assert_eq!(ids[1], queued[1].request_id.to_string());
    }

    /// A migration stopped between its two renames: the old journal is archived,
    /// the carried requests are complete in the staging journal, and the active
    /// path is vacant.
    fn migration_stopped_between_renames()
    -> (tempfile::TempDir, PathBuf, Settings, Vec<ReplayRequest>) {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, queued, base) = journal_on_old_authority(&path);
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let stopped = migrate_with_hook(
            &new,
            Some(&path),
            MigrateOptions::default(),
            base + 10_000,
            || Err(AgentBusError::Internal("simulated stop".to_owned())),
        );
        assert!(stopped.is_err());
        assert!(!path.exists(), "the old journal was archived");
        (root, path, new, queued)
    }

    fn journal_directory_names(path: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| file_name_text(&entry.unwrap().path()))
            .filter(|name| name != DIRECTORY_LOCK)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Protects: debt held by a migration that stopped between its renames stays visible
    /// to the guards that refuse a legacy write ahead of retained requests, and a plan
    /// changes nothing.
    /// Detects: status reporting `pending: 0` while the staged journal holds the debt
    /// (the guard then allows a write ahead of it); a dry run that completes the
    /// recovery by renaming the staging journal.
    /// Needs: owned private journal; the hook stands in for a stop between the renames.
    /// Breadcrumb: `status`, `legacy_write_guard`, `migrate_with_hook`.
    #[test]
    fn interrupted_migration_debt_is_visible_and_a_dry_run_does_not_finish_it() {
        let (_root, path, new, queued) = migration_stopped_between_renames();
        let names = journal_directory_names(&path);

        let report = status(&new, Some(&path)).unwrap();
        assert_eq!(report["pending"], 2, "{report}");
        assert!(report["interrupted_migration"].is_string(), "{report}");
        assert_eq!(
            report["requests"][0]["request_id"],
            queued[0].request_id.to_string()
        );
        assert!(
            legacy_write_guard(&new, Some(&path))
                .unwrap_err()
                .to_string()
                .contains("retained durable requests"),
            "a legacy write must not pass ahead of staged debt"
        );

        let plan = migrate(
            &new,
            Some(&path),
            MigrateOptions {
                dry_run: true,
                ..MigrateOptions::default()
            },
            1,
        )
        .unwrap();
        assert!(plan["interrupted_migration"].is_string(), "{plan}");
        assert_eq!(
            journal_directory_names(&path),
            names,
            "dry run renamed files"
        );
        assert!(!path.exists());

        let finished = migrate(&new, Some(&path), MigrateOptions::default(), 1).unwrap();
        assert_eq!(finished["completed_interrupted"], true, "{finished}");
        assert!(path.exists());
        assert_eq!(status(&new, Some(&path)).unwrap()["pending"], 2);
    }

    /// Protects: a writer cannot create a second active journal while a migration is
    /// between its renames, so carried work is never hidden behind newer requests.
    /// Detects: A writer creating a fresh active journal in the gap (the promotion then
    /// fails and the staged requests are stranded).
    /// Needs: owned private journal; a writer thread started from the gap.
    /// Breadcrumb: `open_active`, `lock_directory_until`, `DIRECTORY_LOCK`.
    #[test]
    fn writer_waits_for_a_migration_instead_of_creating_a_second_active_journal() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, queued, base) = journal_on_old_authority(&path);
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let raced = RefCell::new(None);
        let report = migrate_with_hook(
            &new,
            Some(&path),
            MigrateOptions::default(),
            base + 10_000,
            || {
                let (settings, journal) = (new.clone(), path.clone());
                let writer = std::thread::spawn(move || {
                    submit(
                        &settings,
                        &Unreachable2,
                        Operation::Send,
                        ClientSurface::Cli,
                        args("written during the migration"),
                        Some(&journal),
                    )
                });
                std::thread::sleep(Duration::from_millis(400));
                assert!(!writer.is_finished(), "the writer must wait for the lock");
                assert!(!path.exists(), "no active journal may exist in the gap");
                *raced.borrow_mut() = Some(writer);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(report["migrated"], true);
        let response = raced
            .into_inner()
            .unwrap()
            .join()
            .unwrap()
            .expect("the waiting writer succeeds once the migration is done");
        assert_eq!(response["status"], "queued", "{response}");
        let ids = status(&new, Some(&path)).unwrap()["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["request_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert_eq!(ids[0], queued[0].request_id.to_string());
        assert_eq!(ids[1], queued[1].request_id.to_string());
    }

    /// Protects: a staging journal from an attempt that never archived the old journal
    /// is never promoted, even after that old journal is retired another way.
    /// Detects: Promoting a stale copy whose requests the original may already have
    /// delivered (a duplicate delivery to the previous authority's successor).
    /// Needs: owned private journal.
    /// Breadcrumb: `committed_staging`, `MIGRATED_PREFIX`.
    #[test]
    fn staging_without_its_archive_is_never_promoted() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let old = named_authority_settings("old-hub", "http://localhost:8484");
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        // The original journal is settled: an ordinary write retires it.
        drop(Journal::open(&path, &scope_of(&old)).unwrap());
        // A copy a stopped migration wrote for it, never committed.
        let origin = Journal::inspect(&path).unwrap().origin_id.unwrap();
        let staging = path.with_file_name(format!("{STAGING_PREFIX}{origin}.jsonl"));
        let mut stale = Journal::open(&staging, &scope_of(&new)).unwrap();
        stale
            .enqueue(
                Operation::Send,
                ClientSurface::Cli,
                args("stale copy of a delivered request"),
                now_ms().unwrap(),
            )
            .unwrap();
        drop(stale);

        let response = submit(
            &new,
            &Unreachable2,
            Operation::Send,
            ClientSurface::Cli,
            args("a new request"),
            Some(&path),
        )
        .unwrap();
        assert_eq!(response["status"], "queued", "{response}");
        let report = status(&new, Some(&path)).unwrap();
        assert_eq!(report["pending"], 1, "only the new request: {report}");
        assert!(report.get("interrupted_migration").is_none(), "{report}");
        assert_eq!(report["stale_staging"][0], file_name_text(&staging));
        assert!(staging.exists(), "the stale copy is left in place");
    }

    /// Protects: reading the journal never creates it. A migration vacates the active
    /// path between its two renames, and a reader that created an empty file there
    /// would make the later promotion fail and hide the staged requests.
    /// Detects: `status` or the retirement inspection creating an empty active journal
    /// when the path vanishes between the presence check and the read.
    /// Needs: owned private journal; the hook vacates the path as a migration does.
    /// Breadcrumb: `Journal::inspect`, `outbox_private::open_existing_only`, `status_once`.
    #[test]
    fn reading_the_journal_never_creates_it_when_a_migration_vacates_the_path() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, _queued, base) = journal_on_old_authority(&path);
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let archive = path.with_file_name(format!("{MIGRATED_PREFIX}vacated.jsonl"));
        let report = status_with_hook(&new, Some(&path), || {
            std::fs::rename(&path, &archive).unwrap();
        })
        .unwrap();
        assert!(!path.exists(), "status created an active journal: {report}");
        assert_eq!(report["pending"], 0, "{report}");
        assert!(archive.exists());
        // The ordinary retirement inspection has the same obligation.
        assert!(
            retire_previous_scope(&path, &scope_of(&new), base, RetirePolicy::SettledOnly).is_err()
        );
        assert!(!path.exists(), "retirement created an active journal");
    }

    /// Protects: status never reports an empty outbox while a migration that committed
    /// during the read holds the debt in its staged journal, so the legacy-write guard
    /// keeps refusing a write ahead of the retained requests.
    /// Detects: the two presence checks of `status` straddling a migration's commit, the
    /// first seeing the active journal and the second seeing the vacated path, which
    /// returned `pending: 0` without looking for committed staging.
    /// Needs: owned private journal; the hook runs a real migration that stops between
    /// its two renames, exactly where a concurrent process would leave the directory.
    /// Breadcrumb: `status_once`, `legacy_write_guard_with`, `migrate_with_hook`.
    #[test]
    fn status_between_its_presence_checks_does_not_miss_a_migration_that_commits() {
        let migrate_and_stop = |new: &Settings, path: &Path, base: u64| {
            let stopped = migrate_with_hook(
                new,
                Some(path),
                MigrateOptions::default(),
                base + 10_000,
                || Err(AgentBusError::Internal("simulated stop".to_owned())),
            );
            assert!(stopped.is_err());
            assert!(!path.exists(), "the migration vacated the active path");
        };

        // The status read itself.
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, _queued, base) = journal_on_old_authority(&path);
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let mut commits = 0;
        let report = status_with_hooks(
            &new,
            Some(&path),
            || {
                // Only the first look races; the retry sees the settled state.
                if commits == 0 {
                    migrate_and_stop(&new, &path, base);
                }
                commits += 1;
            },
            || {},
        )
        .unwrap();
        assert_eq!(
            report["pending"], 2,
            "the debt is staged, not gone: {report}"
        );
        assert!(report["interrupted_migration"].is_string(), "{report}");
        assert_eq!(commits, 2, "the first look raced and the retry settled");
        assert!(!path.exists(), "status created an active journal: {report}");

        // The guard that decides on it.
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, _queued, base) = journal_on_old_authority(&path);
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let mut raced = false;
        let refused = legacy_write_guard_with(&new, || {
            status_with_hooks(
                &new,
                Some(&path),
                || {
                    if !raced {
                        raced = true;
                        migrate_and_stop(&new, &path, base);
                    }
                },
                || {},
            )
        });
        assert!(
            refused.is_err(),
            "a legacy write was allowed ahead of retained requests"
        );
    }

    /// Transport that cannot reach the hub, in the native-transport shape.
    struct Unreachable2;
    impl NativeReplayTransport for Unreachable2 {
        fn replay(
            &self,
            _url: &str,
            _hub_identity: &str,
            _request: &ReplayRequest,
            _timeout: Duration,
        ) -> std::result::Result<ReplayResponse, ReplayFailure> {
            Err(ReplayFailure::Transport)
        }
    }

    /// Protects: a staging file left by an attempt that never archived the old journal
    /// does not block the next migration.
    /// Detects: A permanent refusal after one crash, or deleting the stale file.
    /// Needs: owned private journal.
    /// Breadcrumb: `set_aside_partial_staging`.
    #[test]
    fn stale_staging_from_an_earlier_attempt_is_set_aside_not_deleted() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let (_old, _queued, base) = journal_on_old_authority(&path);
        let new = named_authority_settings("new-hub", "http://localhost:8485");
        let origin = Journal::inspect(&path).unwrap().origin_id.unwrap();
        let staging = path.with_file_name(format!("{STAGING_PREFIX}{origin}.jsonl"));
        drop(Journal::open(&staging, "half-written-scope").unwrap());
        let stale_bytes = std::fs::read(&staging).unwrap();

        let report = migrate(&new, Some(&path), MigrateOptions::default(), base + 10_000).unwrap();
        assert_eq!(report["migrated"], true);
        let partial = path.with_file_name(format!("{STAGING_PREFIX}{origin}.partial.jsonl"));
        assert_eq!(std::fs::read(&partial).unwrap(), stale_bytes);
        assert_eq!(status(&new, Some(&path)).unwrap()["pending"], 2);
        // A partial file is not mistaken for an interrupted migration.
        assert!(
            status(&new, Some(&path))
                .unwrap()
                .get("interrupted_migration")
                .is_none()
        );
    }

    /// Protects: waiting work stays visible. A send never expires by age, so the only
    /// protection against a forgotten queue is saying how old it is.
    /// Detects: Status that hides age, attempts or staleness.
    /// Needs: owned private journal; no transport.
    /// Breadcrumb: `status`, `STALE_AFTER_MS`.
    #[test]
    fn status_reports_age_attempts_and_stale_requests() {
        let root = tempfile::tempdir().unwrap();
        let path = private_journal_path(&root);
        let settings = named_authority_settings("hub", "http://localhost:8484");
        let mut journal = Journal::open(&path, &scope_of(&settings)).unwrap();
        // Created at the epoch: far older than a week.
        let old = journal
            .enqueue(Operation::Send, ClientSurface::Cli, args("old"), 100)
            .unwrap();
        let fresh = journal
            .enqueue(
                Operation::Send,
                ClientSurface::Cli,
                args("fresh"),
                now_ms().unwrap(),
            )
            .unwrap();
        drop(journal);
        let report = status(&settings, Some(&path)).unwrap();
        assert_eq!(report["pending"], 2);
        assert_eq!(report["stale"], 1);
        assert_eq!(report["stale_after_ms"], STALE_AFTER_MS);
        assert!(report["oldest_age_ms"].as_u64().unwrap() >= STALE_AFTER_MS);
        assert_eq!(
            report["requests"][0]["request_id"],
            old.request_id.to_string()
        );
        assert_eq!(report["requests"][0]["stale"], true);
        assert_eq!(
            report["requests"][1]["request_id"],
            fresh.request_id.to_string()
        );
        assert_eq!(report["requests"][1]["stale"], false);
        assert!(report.get("scope_mismatch").is_none());
        assert!(report.get("recovery").is_none());
    }
}
