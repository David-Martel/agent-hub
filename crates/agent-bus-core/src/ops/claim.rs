//! Typed claim/arbitration operations wrapping [`crate::channels`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface rather
//! than calling [`crate::channels`] directly.
//!
//! All functions are pure wrappers — no business logic lives here.

use anyhow::Result;

use crate::channels::{
    ArbitrationState, ClaimOptions, OwnershipClaim, ResourceLeaseMode, ResourceScope,
};
use crate::settings::Settings;
use crate::validation::non_empty;

// ---------------------------------------------------------------------------
// Mode parsing
// ---------------------------------------------------------------------------

/// Parse a resource scope string into a [`ResourceScope`].
///
/// Accepts `"repo"` and `"machine"`.
///
/// # Errors
///
/// Returns an error if the string is not one of the recognised values.
pub fn parse_resource_scope(scope: &str) -> Result<ResourceScope> {
    match scope {
        "repo" => Ok(ResourceScope::Repo),
        "machine" => Ok(ResourceScope::Machine),
        other => anyhow::bail!("invalid scope '{other}'; expected repo|machine"),
    }
}

/// Parse a lease mode string into a [`ResourceLeaseMode`].
///
/// Accepts `"shared"`, `"shared_namespaced"`, and `"exclusive"`.
///
/// # Errors
///
/// Returns an error if the string is not one of the recognised values.
pub fn parse_lease_mode(mode: &str) -> Result<ResourceLeaseMode> {
    match mode {
        "shared" => Ok(ResourceLeaseMode::Shared),
        "shared_namespaced" => Ok(ResourceLeaseMode::SharedNamespaced),
        "exclusive" => Ok(ResourceLeaseMode::Exclusive),
        other => anyhow::bail!(
            "invalid lease mode '{other}'; expected shared|shared_namespaced|exclusive"
        ),
    }
}

// ---------------------------------------------------------------------------
// Claim resource
// ---------------------------------------------------------------------------

/// Typed request for claiming a resource.
#[derive(Debug)]
pub struct ClaimResourceRequest<'a> {
    pub resource: &'a str,
    pub agent: &'a str,
    pub reason: &'a str,
    /// One of `"shared"`, `"shared_namespaced"`, or `"exclusive"`.
    pub mode: &'a str,
    pub namespace: Option<&'a str>,
    pub scope_kind: Option<&'a str>,
    pub scope_path: Option<&'a str>,
    pub repo_scopes: &'a [String],
    pub thread_id: Option<&'a str>,
    pub lease_ttl_seconds: u64,
    /// Explicit resource scope override (`repo` or `machine`).
    /// When `None`, auto-detection applies based on the resource name.
    pub scope: Option<ResourceScope>,
}

/// Claim ownership of a resource with structured lease options.
///
/// Parses `request.mode` into [`ResourceLeaseMode`] and delegates to
/// [`crate::channels::claim_resource_with_options`].
///
/// # Errors
///
/// Returns an error if the mode string is invalid, if required fields are
/// empty, or if Redis commands fail.
pub fn claim_resource(
    settings: &Settings,
    request: &ClaimResourceRequest<'_>,
) -> Result<OwnershipClaim> {
    let agent = non_empty(request.agent, "agent")?;
    let resource = non_empty(request.resource, "resource")?;

    let options = ClaimOptions {
        mode: parse_lease_mode(request.mode)?,
        namespace: request.namespace.map(str::to_owned),
        scope_kind: request.scope_kind.map(str::to_owned),
        scope_path: request.scope_path.map(str::to_owned),
        repo_scopes: request.repo_scopes.to_vec(),
        thread_id: request.thread_id.map(str::to_owned),
        lease_ttl_seconds: request.lease_ttl_seconds.max(1),
        scope: request.scope.clone(),
    };
    crate::channels::claim_resource_with_options(
        settings,
        resource,
        agent,
        request.reason,
        &options,
    )
}

// ---------------------------------------------------------------------------
// Renew claim
// ---------------------------------------------------------------------------

/// Typed request for renewing an active claim.
#[derive(Debug)]
pub struct RenewClaimRequest<'a> {
    pub resource: &'a str,
    pub agent: &'a str,
    pub lease_ttl_seconds: u64,
}

/// Renew an active claim, extending its expiry.
///
/// Delegates to [`crate::channels::renew_claim`].
///
/// # Errors
///
/// Returns an error if no active claim exists for the agent, or if Redis
/// commands fail.
pub fn renew_claim(settings: &Settings, request: &RenewClaimRequest<'_>) -> Result<OwnershipClaim> {
    let agent = non_empty(request.agent, "agent")?;
    let resource = non_empty(request.resource, "resource")?;

    crate::channels::renew_claim(
        settings,
        resource,
        agent,
        Some(request.lease_ttl_seconds.max(1)),
    )
}

// ---------------------------------------------------------------------------
// Release claim
// ---------------------------------------------------------------------------

/// Typed request for releasing a claim.
#[derive(Debug)]
pub struct ReleaseClaimRequest<'a> {
    pub resource: &'a str,
    pub agent: &'a str,
}

/// Release a claim held by `request.agent`.
///
/// Delegates to [`crate::channels::release_claim`].
///
/// # Errors
///
/// Returns an error if no active claim exists for the agent, or if Redis
/// commands fail.
pub fn release_claim(
    settings: &Settings,
    request: &ReleaseClaimRequest<'_>,
) -> Result<ArbitrationState> {
    let agent = non_empty(request.agent, "agent")?;
    let resource = non_empty(request.resource, "resource")?;

    crate::channels::release_claim(settings, resource, agent)
}

// ---------------------------------------------------------------------------
// Resolve claim
// ---------------------------------------------------------------------------

/// Typed request for resolving a contested claim.
#[derive(Debug)]
pub struct ResolveClaimRequest<'a> {
    pub resource: &'a str,
    pub winner: &'a str,
    pub reason: &'a str,
    pub resolved_by: &'a str,
}

/// Resolve a contested ownership claim by naming a winner.
///
/// Delegates to [`crate::channels::resolve_claim`].
///
/// # Errors
///
/// Returns an error if no claims exist for the resource, or if Redis commands
/// fail.
pub fn resolve_claim(
    settings: &Settings,
    request: &ResolveClaimRequest<'_>,
) -> Result<ArbitrationState> {
    let winner = non_empty(request.winner, "winner")?;
    let resource = non_empty(request.resource, "resource")?;

    crate::channels::resolve_claim(
        settings,
        resource,
        winner,
        request.reason,
        request.resolved_by,
    )
}

// ---------------------------------------------------------------------------
// List claims
// ---------------------------------------------------------------------------

/// Typed request for listing ownership claims.
#[derive(Debug)]
pub struct ListClaimsRequest<'a> {
    pub resource: Option<&'a str>,
    /// One of `"pending"`, `"granted"`, `"contested"`, `"review_assigned"`,
    /// or `None` for no status filter.
    pub status: Option<&'a str>,
}

/// List ownership claims with optional resource and status filters.
///
/// Parses `request.status` into a [`crate::channels::ClaimStatus`] and
/// delegates to [`crate::channels::list_claims`].
///
/// # Errors
///
/// Returns an error if `request.status` is an unrecognised value or if the
/// Redis `SCAN` or `HGETALL` commands fail.
pub fn list_claims(
    settings: &Settings,
    request: &ListClaimsRequest<'_>,
) -> Result<Vec<OwnershipClaim>> {
    use crate::channels::ClaimStatus;
    let status_filter: Option<ClaimStatus> = request
        .status
        .map(|s| match s {
            "pending" => Ok(ClaimStatus::Pending),
            "granted" => Ok(ClaimStatus::Granted),
            "contested" => Ok(ClaimStatus::Contested),
            "review_assigned" => Ok(ClaimStatus::ReviewAssigned),
            other => anyhow::bail!(
                "unknown claim status '{other}'; expected pending|granted|contested|review_assigned"
            ),
        })
        .transpose()?;
    crate::channels::list_claims(settings, request.resource, status_filter.as_ref())
}

// ---------------------------------------------------------------------------
// Arbitration state
// ---------------------------------------------------------------------------

/// Get the full arbitration state for a single resource.
///
/// Delegates to [`crate::channels::get_arbitration_state`].
///
/// # Errors
///
/// Returns an error if the Redis `HGETALL` fails.
pub fn get_arbitration_state(settings: &Settings, resource: &str) -> Result<ArbitrationState> {
    crate::channels::get_arbitration_state(settings, resource)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a [`Settings`] from the environment/config file.  This never
    /// connects to Redis — it only reads env vars and the optional config JSON.
    fn test_settings() -> Settings {
        Settings::from_env()
    }

    // -- parse_resource_scope ---------------------------------------------------

    #[test]
    fn parse_resource_scope_accepts_repo() {
        assert!(matches!(
            parse_resource_scope("repo").unwrap(),
            ResourceScope::Repo
        ));
    }

    #[test]
    fn parse_resource_scope_accepts_machine() {
        assert!(matches!(
            parse_resource_scope("machine").unwrap(),
            ResourceScope::Machine
        ));
    }

    #[test]
    fn parse_resource_scope_rejects_unknown() {
        let err = parse_resource_scope("global").unwrap_err();
        assert!(
            err.to_string().contains("invalid scope"),
            "error should mention 'invalid scope': {err}"
        );
    }

    // -- parse_lease_mode -----------------------------------------------------

    #[test]
    fn parse_lease_mode_accepts_shared() {
        assert!(matches!(
            parse_lease_mode("shared").unwrap(),
            ResourceLeaseMode::Shared
        ));
    }

    #[test]
    fn parse_lease_mode_accepts_shared_namespaced() {
        assert!(matches!(
            parse_lease_mode("shared_namespaced").unwrap(),
            ResourceLeaseMode::SharedNamespaced
        ));
    }

    #[test]
    fn parse_lease_mode_accepts_exclusive() {
        assert!(matches!(
            parse_lease_mode("exclusive").unwrap(),
            ResourceLeaseMode::Exclusive
        ));
    }

    #[test]
    fn parse_lease_mode_rejects_unknown() {
        let err = parse_lease_mode("bogus").unwrap_err();
        assert!(
            err.to_string().contains("invalid lease mode"),
            "error should mention 'invalid lease mode': {err}"
        );
    }

    // -- claim_resource validation -------------------------------------------

    #[test]
    fn claim_resource_rejects_empty_agent() {
        let settings = test_settings();
        let result = claim_resource(
            &settings,
            &ClaimResourceRequest {
                resource: "test-resource",
                agent: "",
                reason: "testing",
                mode: "exclusive",
                namespace: None,
                scope_kind: None,
                scope_path: None,
                repo_scopes: &[],
                thread_id: None,
                lease_ttl_seconds: 300,
                scope: None,
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn claim_resource_rejects_whitespace_only_agent() {
        let settings = test_settings();
        let result = claim_resource(
            &settings,
            &ClaimResourceRequest {
                resource: "test-resource",
                agent: "   ",
                reason: "testing",
                mode: "exclusive",
                namespace: None,
                scope_kind: None,
                scope_path: None,
                repo_scopes: &[],
                thread_id: None,
                lease_ttl_seconds: 300,
                scope: None,
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn claim_resource_rejects_empty_resource() {
        let settings = test_settings();
        let result = claim_resource(
            &settings,
            &ClaimResourceRequest {
                resource: "",
                agent: "test-agent",
                reason: "testing",
                mode: "exclusive",
                namespace: None,
                scope_kind: None,
                scope_path: None,
                repo_scopes: &[],
                thread_id: None,
                lease_ttl_seconds: 300,
                scope: None,
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("resource"),
            "error should mention 'resource': {msg}"
        );
    }

    #[test]
    fn claim_resource_rejects_invalid_mode() {
        let settings = test_settings();
        let result = claim_resource(
            &settings,
            &ClaimResourceRequest {
                resource: "test-resource",
                agent: "test-agent",
                reason: "testing",
                mode: "bogus",
                namespace: None,
                scope_kind: None,
                scope_path: None,
                repo_scopes: &[],
                thread_id: None,
                lease_ttl_seconds: 300,
                scope: None,
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid lease mode"),
            "error should mention 'invalid lease mode': {msg}"
        );
    }

    // -- renew_claim validation -----------------------------------------------

    #[test]
    fn renew_claim_rejects_empty_agent() {
        let settings = test_settings();
        let result = renew_claim(
            &settings,
            &RenewClaimRequest {
                resource: "test-resource",
                agent: "",
                lease_ttl_seconds: 300,
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn renew_claim_rejects_empty_resource() {
        let settings = test_settings();
        let result = renew_claim(
            &settings,
            &RenewClaimRequest {
                resource: "",
                agent: "test-agent",
                lease_ttl_seconds: 300,
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("resource"),
            "error should mention 'resource': {msg}"
        );
    }

    // -- release_claim validation ---------------------------------------------

    #[test]
    fn release_claim_rejects_empty_agent() {
        let settings = test_settings();
        let result = release_claim(
            &settings,
            &ReleaseClaimRequest {
                resource: "test-resource",
                agent: "",
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn release_claim_rejects_empty_resource() {
        let settings = test_settings();
        let result = release_claim(
            &settings,
            &ReleaseClaimRequest {
                resource: "",
                agent: "test-agent",
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("resource"),
            "error should mention 'resource': {msg}"
        );
    }

    // -- resolve_claim validation ---------------------------------------------

    #[test]
    fn resolve_claim_rejects_empty_winner() {
        let settings = test_settings();
        let result = resolve_claim(
            &settings,
            &ResolveClaimRequest {
                resource: "test-resource",
                winner: "",
                reason: "testing",
                resolved_by: "orchestrator",
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("winner"),
            "error should mention 'winner': {msg}"
        );
    }

    #[test]
    fn resolve_claim_rejects_empty_resource() {
        let settings = test_settings();
        let result = resolve_claim(
            &settings,
            &ResolveClaimRequest {
                resource: "",
                winner: "test-agent",
                reason: "testing",
                resolved_by: "orchestrator",
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("resource"),
            "error should mention 'resource': {msg}"
        );
    }

    // -- list_claims status parsing -------------------------------------------

    #[test]
    fn list_claims_rejects_unknown_status() {
        let settings = test_settings();
        let result = list_claims(
            &settings,
            &ListClaimsRequest {
                resource: None,
                status: Some("bogus"),
            },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown claim status"),
            "error should mention 'unknown claim status': {msg}"
        );
    }

    #[test]
    fn list_claims_accepts_all_valid_statuses() {
        // These calls will parse the status successfully but then fail when
        // trying to connect to Redis.  We verify the status parsing itself
        // does not reject valid values by checking the error does NOT mention
        // "unknown claim status".
        let settings = test_settings();
        for status in &["pending", "granted", "contested", "review_assigned"] {
            let result = list_claims(
                &settings,
                &ListClaimsRequest {
                    resource: None,
                    status: Some(status),
                },
            );
            // The call will likely fail with a Redis connection error, but it
            // should NOT fail with a status-parsing error.
            if let Err(ref err) = result {
                let msg = err.to_string();
                assert!(
                    !msg.contains("unknown claim status"),
                    "status '{status}' should be accepted but got: {msg}"
                );
            }
            // If it somehow succeeds (unlikely without Redis), that is also fine.
        }
    }
}
