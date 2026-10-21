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
    ClientSurface, Journal, Operation, OutboxError, ReplayFailure, ReplayRequest, ReplayResponse,
    ReplayTransport,
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
    let candidates = settings.effective_hub_candidates();
    if candidates
        .iter()
        .find(|candidate| candidate.role == HubRole::Authoritative)
        .is_none_or(|candidate| candidate.hub.as_ref().is_none_or(String::is_empty))
    {
        return Ok(());
    }
    let state = status(settings, path_override)?;
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

/// Retry only a busy journal, within the caller's original transport deadline.
fn open_journal_until(
    path: &Path,
    scope: &str,
    deadline: Instant,
) -> std::result::Result<Journal, OutboxError> {
    let mut waited = false;
    let mut delay = Duration::from_millis(10);
    let jitter = Duration::from_millis(u64::from(uuid::Uuid::new_v4().as_bytes()[0] % 17));
    loop {
        if waited && Instant::now() >= deadline {
            return Err(OutboxError::Busy);
        }
        match Journal::open(path, scope) {
            Err(OutboxError::Busy) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(OutboxError::Busy);
                }
                std::thread::sleep((delay + jitter).min(remaining));
                delay = (delay * 2).min(Duration::from_millis(100));
                waited = true;
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

/// Inspect stable pending IDs without submitting a replacement request.
///
/// # Errors
/// Invalid destination or private journal custody, corruption or contention.
pub fn status(settings: &Settings, path_override: Option<&Path>) -> Result<Value> {
    let scope = candidates_fingerprint(&settings.effective_hub_candidates());
    let path = match path_override {
        Some(path) => path.to_owned(),
        None => journal_path(&scope)?,
    };
    if !journal_present(&path)? {
        return Ok(json!({"pending":0,"requests":[]}));
    }
    let inspection = Journal::inspect(&path).map_err(|_sanitized| {
        AgentBusError::Internal(
            "private durable outbox status unavailable; preserve journal".to_owned(),
        )
    })?;
    let mut report = json!({"pending":inspection.pending.len(),
        "requests":inspection.pending.iter().map(request_summary).collect::<Vec<_>>()});
    if let Some(stored) = inspection
        .destination
        .as_deref()
        .filter(|stored| *stored != scope)
    {
        // Scope fingerprints hash configuration metadata, never token bytes.
        let now = now_ms()?;
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
        report["current_scope_prefix"] = json!(scope_prefix(&scope));
        report["rotation"] = json!(rotation);
    }
    Ok(report)
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
             request(s) that are unexpired or await replay settlement: {}; replay them with the previous configuration \
             (`agent-bus outbox flush`) or preserve the journal",
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
    if !journal_present(&path)? {
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
            .filter(|candidate| candidate != path)
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
}
