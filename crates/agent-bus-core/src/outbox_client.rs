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
    ClientSurface, Journal, Operation, ReplayFailure, ReplayRequest, ReplayResponse,
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
    let mut journal = Journal::open(&path, &scope).map_err(|_sanitized| {
        AgentBusError::Internal("private durable outbox could not be opened".to_owned())
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
    let journal = Journal::open(&path, &scope).map_err(|_sanitized| {
        AgentBusError::Internal(
            "private durable outbox status unavailable; preserve journal".to_owned(),
        )
    })?;
    Ok(
        json!({"pending":journal.pending().count(),"requests":journal.pending().map(|request|
        json!({"request_id":request.request_id,"operation":request.operation,"created_ms":request.created_ms,"expires_ms":request.expires_ms})).collect::<Vec<_>>()}),
    )
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
    let scope = candidates_fingerprint(&settings.effective_hub_candidates());
    let path = match path_override {
        Some(path) => path.to_owned(),
        None => journal_path(&scope)?,
    };
    if !journal_present(&path)? {
        return Ok(json!({"remaining":0,"applied":0,"requests_submitted":0}));
    }
    let (scope, route) = replay_context(settings, transport)?;
    let mut journal = Journal::open(&path, &scope).map_err(|_sanitized| {
        AgentBusError::Internal("private durable outbox unavailable; preserve journal".to_owned())
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
        let private_root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(private_root.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
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
        assert!(!id.is_empty());
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
