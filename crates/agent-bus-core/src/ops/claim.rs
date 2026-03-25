//! Typed claim/arbitration operations wrapping [`crate::channels`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface rather
//! than calling [`crate::channels`] directly.
//!
//! All functions are pure wrappers — no business logic lives here.

use anyhow::Result;

use crate::channels::{ArbitrationState, ClaimOptions, OwnershipClaim, ResourceLeaseMode};
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Mode parsing
// ---------------------------------------------------------------------------

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
        other => anyhow::bail!("invalid lease mode '{other}'; expected shared|shared_namespaced|exclusive"),
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
    let options = ClaimOptions {
        mode: parse_lease_mode(request.mode)?,
        namespace: request.namespace.map(str::to_owned),
        scope_kind: request.scope_kind.map(str::to_owned),
        scope_path: request.scope_path.map(str::to_owned),
        repo_scopes: request.repo_scopes.to_vec(),
        thread_id: request.thread_id.map(str::to_owned),
        lease_ttl_seconds: request.lease_ttl_seconds.max(1),
    };
    crate::channels::claim_resource_with_options(
        settings,
        request.resource,
        request.agent,
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
pub fn renew_claim(
    settings: &Settings,
    request: &RenewClaimRequest<'_>,
) -> Result<OwnershipClaim> {
    crate::channels::renew_claim(
        settings,
        request.resource,
        request.agent,
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
    crate::channels::release_claim(settings, request.resource, request.agent)
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
    crate::channels::resolve_claim(
        settings,
        request.resource,
        request.winner,
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
/// Returns an error if the Redis `SCAN` or `HGETALL` commands fail.
pub fn list_claims(
    settings: &Settings,
    request: &ListClaimsRequest<'_>,
) -> Result<Vec<OwnershipClaim>> {
    use crate::channels::ClaimStatus;
    let status_filter: Option<ClaimStatus> = request.status.map(|s| match s {
        "granted" => ClaimStatus::Granted,
        "contested" => ClaimStatus::Contested,
        "review_assigned" => ClaimStatus::ReviewAssigned,
        _ => ClaimStatus::Pending,
    });
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
pub fn get_arbitration_state(
    settings: &Settings,
    resource: &str,
) -> Result<ArbitrationState> {
    crate::channels::get_arbitration_state(settings, resource)
}
