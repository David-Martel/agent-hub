//! Subscription management operations wrapping [`crate::redis_bus`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface for
//! creating, listing, and deleting subscriptions.

use crate::error::Result;
use chrono::Utc;
use uuid::Uuid;

use crate::models::{Subscription, SubscriptionScopes, VALID_SUBSCRIPTION_PRIORITIES};
use crate::redis_bus;
use crate::settings::Settings;
use crate::validation::non_empty;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Typed request for creating a new subscription.
#[derive(Debug)]
pub struct SubscribeRequest<'a> {
    /// The subscribing agent (must not be empty).
    pub agent: &'a str,
    /// The scope filters for this subscription.
    pub scopes: &'a SubscriptionScopes,
    /// Optional time-to-live in seconds.
    pub ttl_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// Subscribe
// ---------------------------------------------------------------------------

/// Create and persist a new subscription for the given agent.
///
/// Auto-generates the subscription `id` (UUID v4), `created_at` (ISO 8601
/// UTC), and `expires_at` (when `ttl_seconds` is provided).
///
/// # Errors
///
/// Returns an error if:
/// - `agent` is empty or whitespace-only
/// - `priority_min` is set but not a valid priority
/// - The Redis connection or SET command fails
pub fn subscribe(settings: &Settings, request: &SubscribeRequest<'_>) -> Result<Subscription> {
    let agent = non_empty(request.agent, "agent")?;

    // Validate priority_min if provided.
    if let Some(ref pmin) = request.scopes.priority_min
        && !VALID_SUBSCRIPTION_PRIORITIES.contains(&pmin.as_str())
    {
        return Err(crate::error::AgentBusError::InvalidParams(format!(
            "invalid priority_min '{pmin}'; must be one of: {}",
            VALID_SUBSCRIPTION_PRIORITIES.join(", "))));
    }

    let now = Utc::now();
    let expires_at = request.ttl_seconds.map(|ttl| {
        let expiry = now + chrono::Duration::seconds(i64::try_from(ttl).unwrap_or(i64::MAX));
        expiry.to_rfc3339()
    });

    let sub = Subscription {
        id: Uuid::new_v4().to_string(),
        agent: agent.to_owned(),
        scopes: request.scopes.clone(),
        created_at: now.to_rfc3339(),
        ttl_seconds: request.ttl_seconds,
        expires_at,
    };

    redis_bus::save_subscription(settings, &sub)?;
    Ok(sub)
}

// ---------------------------------------------------------------------------
// Unsubscribe
// ---------------------------------------------------------------------------

/// Delete a specific subscription by agent and subscription ID.
///
/// Returns `true` if the subscription existed and was deleted, `false` if
/// it was already absent (e.g. expired or never created).
///
/// # Errors
///
/// Returns an error if `agent` or `subscription_id` is empty, or if the
/// Redis connection or DEL command fails.
pub fn unsubscribe(settings: &Settings, agent: &str, subscription_id: &str) -> Result<bool> {
    let agent = non_empty(agent, "agent")?;
    let sub_id = non_empty(subscription_id, "subscription_id")?;
    redis_bus::delete_subscription(settings, agent, sub_id)
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// List all active subscriptions for a given agent.
///
/// Expired keys are automatically excluded by Redis TTL.
///
/// # Errors
///
/// Returns an error if `agent` is empty, or if the Redis connection fails.
pub fn list_subscriptions(settings: &Settings, agent: &str) -> Result<Vec<Subscription>> {
    let agent = non_empty(agent, "agent")?;
    redis_bus::list_subscriptions(settings, agent)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> Settings {
        Settings::from_env()
    }

    // -- subscribe validation -------------------------------------------------

    #[test]
    fn subscribe_rejects_empty_agent() {
        let settings = test_settings();
        let req = SubscribeRequest {
            agent: "   ",
            scopes: &SubscriptionScopes::default(),
            ttl_seconds: None,
        };
        let result = subscribe(&settings, &req);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn subscribe_rejects_invalid_priority_min() {
        let settings = test_settings();
        let scopes = SubscriptionScopes {
            priority_min: Some("critical".to_owned()),
            ..Default::default()
        };
        let req = SubscribeRequest {
            agent: "claude",
            scopes: &scopes,
            ttl_seconds: None,
        };
        let result = subscribe(&settings, &req);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid priority_min"),
            "error should mention 'invalid priority_min': {msg}"
        );
    }

    #[test]
    fn subscribe_accepts_valid_priority_min() {
        for pmin in VALID_SUBSCRIPTION_PRIORITIES {
            let settings = test_settings();
            let scopes = SubscriptionScopes {
                priority_min: Some((*pmin).to_owned()),
                ..Default::default()
            };
            let req = SubscribeRequest {
                agent: "claude",
                scopes: &scopes,
                ttl_seconds: None,
            };
            let result = subscribe(&settings, &req);
            // Will fail on Redis connect, but should NOT fail on validation.
            if let Err(ref err) = result {
                let msg = err.to_string();
                assert!(
                    !msg.contains("invalid priority_min"),
                    "priority_min '{pmin}' should be accepted but got: {msg}"
                );
            }
        }
    }

    // -- unsubscribe validation -----------------------------------------------

    #[test]
    fn unsubscribe_rejects_empty_agent() {
        let settings = test_settings();
        let result = unsubscribe(&settings, "", "sub-123");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn unsubscribe_rejects_empty_subscription_id() {
        let settings = test_settings();
        let result = unsubscribe(&settings, "claude", "");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("subscription_id"),
            "error should mention 'subscription_id': {msg}"
        );
    }

    // -- list_subscriptions validation ----------------------------------------

    #[test]
    fn list_subscriptions_rejects_empty_agent() {
        let settings = test_settings();
        let result = list_subscriptions(&settings, "  ");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }
}
